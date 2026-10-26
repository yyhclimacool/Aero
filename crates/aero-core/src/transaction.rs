//! Recoverable multi-file writes. Each file is atomic; the workspace is journaled.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{Vault, VaultPath};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct FileChange {
    pub path: VaultPath,
    /// None means create-only; Some is the exact version observed before preparation.
    pub before: Option<Vec<u8>>,
    /// None means delete.
    pub after: Option<Vec<u8>>,
}

impl FileChange {
    pub fn reverse(&self) -> Self {
        Self { path: self.path.clone(), before: self.after.clone(), after: self.before.clone() }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    PendingCut,
    Prepared,
    Committing,
    Committed,
    RollingBack,
}

#[derive(Serialize, Deserialize)]
struct Record {
    path: String,
    before: Option<String>,
    after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    restore: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct CutIdentity {
    token: String,
    version: u64,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u8,
    vault: String,
    phase: Phase,
    files: Vec<Record>,
    #[serde(default)]
    generation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cut: Option<CutIdentity>,
}

pub struct Transaction {
    directory: PathBuf,
    manifest: Manifest,
    changes: Vec<FileChange>,
    observations: Vec<FileChange>,
    observed_notes: Option<BTreeSet<VaultPath>>,
    restorations: HashMap<VaultPath, Vec<u8>>,
    lease: Option<std::sync::Arc<File>>,
}

/// The lease protects this single journal owner from another window's crash recovery.
pub struct PendingCut {
    transaction: Option<Transaction>,
    directory: PathBuf,
    source: VaultPath,
    stage: CutStage,
    lease: Option<std::sync::Arc<File>>,
}

#[derive(PartialEq, Eq)]
enum CutStage {
    Unpublished { root: PathBuf, token: String },
    PendingCut,
    Upgraded,
    Restored,
}

impl PendingCut {
    pub fn unpublished(root: PathBuf, source: VaultPath, token: String) -> Self {
        Self {
            directory: root.join(ulid::Ulid::generate().to_string().to_ascii_lowercase()),
            stage: CutStage::Unpublished { root, token },
            source,
            transaction: None,
            lease: None,
        }
    }

    pub fn is_unpublished(&self) -> bool {
        matches!(self.stage, CutStage::Unpublished { .. })
    }

    fn initialize(
        &mut self,
        vault: &Vault,
        change: FileChange,
        restore: Vec<u8>,
        version: u64,
    ) -> Result<(), String> {
        if let CutStage::Unpublished { root, token } = &self.stage {
            if change.path != self.source || change.before.is_none() || change.after.is_none() {
                return Err("剪切初始化必须使用已校验的来源快照".into());
            }
            let mut tx = Transaction::stage_new(
                vault,
                root,
                vec![change],
                HashMap::from([(self.source.clone(), restore)]),
                Some(CutIdentity { token: token.clone(), version }),
            )?;
            let result = tx.phase(Phase::PendingCut).and_then(|_| sync_directory(root));
            self.directory = tx.directory.clone();
            self.lease = tx.lease.clone();
            self.transaction = Some(tx);
            self.stage = CutStage::PendingCut;
            return result;
        }
        Ok(())
    }

    pub fn prepare(
        vault: &Vault,
        root: &Path,
        token: &str,
        change: FileChange,
        restore: Vec<u8>,
        version: u64,
    ) -> Result<Self, String> {
        let source = change.path.clone();
        let restorations = HashMap::from([(source.clone(), restore)]);
        let transaction = Transaction::prepare_new(
            vault,
            root,
            vec![change],
            restorations,
            Some(CutIdentity { token: token.into(), version }),
        )?;
        let lease = transaction.lease.clone().ok_or("缺少剪切日志租约")?;
        Ok(Self {
            directory: transaction.directory.clone(),
            transaction: Some(transaction),
            source,
            stage: CutStage::PendingCut,
            lease: Some(lease),
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn is_pending(&self) -> bool {
        self.is_unpublished()
            || self.transaction.as_ref().is_some_and(|tx| tx.manifest.phase == Phase::PendingCut)
    }

    pub fn publish(
        &mut self,
        vault: &Vault,
        change: FileChange,
        restore: Vec<u8>,
        version: u64,
    ) -> Result<crate::FileStamp, String> {
        if self.is_unpublished() {
            self.initialize(vault, change, restore, version)?;
        } else {
            if !self.is_pending() {
                return Err("剪切恢复尚未完成，禁止保存".into());
            }
            self.update(vault, change, restore, version)?;
        }
        let tx = self.transaction.as_ref().ok_or("剪切事务已移交，禁止继续保存")?;
        let change = tx.changes.iter().find(|c| c.path == self.source).ok_or("缺少剪切来源")?;
        sync_directory(tx.directory.parent().ok_or("缺少剪切恢复目录")?)?;
        apply(vault, change, &change.before, &change.after)?;
        vault.read_bytes(&change.path).map(|(_, stamp)| stamp).map_err(|e| e.to_string())
    }

    fn update(
        &mut self,
        vault: &Vault,
        change: FileChange,
        restore: Vec<u8>,
        version: u64,
    ) -> Result<(), String> {
        if self.stage != CutStage::PendingCut {
            return Err("剪切事务已移交，禁止继续保存".into());
        }
        let tx = self.transaction.as_mut().ok_or("剪切事务已移交，禁止继续保存")?;
        let previous = tx.changes.iter().find(|c| c.path == self.source).ok_or("缺少剪切来源")?;
        if change.path != self.source
            || change.after.is_none()
            || (change.before != previous.before && change.before != previous.after)
        {
            return Err(format!("剪切来源快照已变化：{}", self.source));
        }
        let path = self.source.clone();
        let mut identity = tx.manifest.cut.clone().ok_or("缺少剪切身份")?;
        identity.version = version;
        tx.replace_generation(
            vault,
            vec![change],
            HashMap::from([(path, restore)]),
            Phase::PendingCut,
            Some(identity),
            |_| Ok(()),
        )
    }

    /// Cancellation is durable before the caller invalidates the one-shot token.
    pub fn restore(
        &mut self,
        vault: &Vault,
        change: FileChange,
        version: u64,
    ) -> Result<crate::FileStamp, String> {
        let bytes = change.after.clone().ok_or("缺少剪切恢复内容")?;
        if change.path != self.source {
            return Err("剪切恢复来源不匹配".into());
        }
        if self.is_unpublished() {
            self.initialize(vault, change, bytes, version)?;
        } else if self.stage == CutStage::PendingCut {
            self.update(vault, change, bytes, version)?;
        }
        self.recover_cancel(vault)?;
        vault.read_bytes(&self.source).map(|(_, stamp)| stamp).map_err(|e| e.to_string())
    }

    pub fn prepare_paste(
        &mut self,
        vault: &Vault,
        changes: Vec<FileChange>,
        restorations: HashMap<VaultPath, Vec<u8>>,
    ) -> Result<Transaction, String> {
        let tx = self.transaction.as_mut().ok_or("剪切令牌已使用")?;
        let source = tx.changes.iter().find(|c| c.path == self.source).ok_or("缺少剪切来源")?;
        if tx.manifest.phase != Phase::PendingCut
            || !changes.iter().any(|c| c.path == source.path && c.before == source.after)
            || restorations.get(&source.path) != tx.restorations.get(&source.path)
        {
            return Err("粘贴必须接管当前剪切来源及其恢复镜像".into());
        }
        let identity = tx.manifest.cut.clone();
        let result =
            tx.replace_generation(vault, changes, restorations, Phase::Prepared, identity, |_| Ok(()));
        if tx.manifest.phase != Phase::PendingCut {
            self.stage = CutStage::Upgraded;
        }
        result?;
        Ok(self.transaction.take().unwrap())
    }

    pub fn freeze(&mut self) -> Result<(), String> {
        if let Some(tx) = &mut self.transaction {
            tx.phase(Phase::RollingBack)?;
        }
        Ok(())
    }

    /// Retry an interrupted rollback after a paste worker has taken ownership.
    pub fn recover_cancel(&mut self, vault: &Vault) -> Result<(), String> {
        if let Some(tx) = &mut self.transaction {
            tx.rollback(vault)?;
            self.transaction = None;
            self.stage = CutStage::Restored;
            return Ok(());
        }
        if self.directory.join("manifest.toml").exists() {
            let mut tx = Transaction::load(vault, self.directory.clone())?;
            tx.lease = self.lease.clone();
            if tx.manifest.phase == Phase::Committed {
                return Err("剪切已经提交，不能再次恢复来源".into());
            }
            tx.rollback(vault)?;
        }
        self.transaction = None;
        self.stage = CutStage::Restored;
        Ok(())
    }
}

/// Pre-save admission must not use ordinary save's backup-and-overwrite policy or stat-only fast path.
pub fn validate_write(
    vault: &Vault,
    path: &VaultPath,
    known: crate::FileStamp,
    bytes: Vec<u8>,
) -> Result<FileChange, String> {
    let (before, stamp) = vault.read_bytes(path).map_err(|e| e.to_string())?;
    if stamp != known {
        return Err(format!("文件版本冲突：{path}"));
    }
    let change = FileChange { path: path.clone(), before: Some(before), after: Some(bytes) };
    check(vault, &change, &change.before)?;
    Ok(change)
}

pub fn write_checked(
    vault: &Vault,
    path: &VaultPath,
    known: crate::FileStamp,
    bytes: Vec<u8>,
) -> Result<crate::FileStamp, String> {
    let change = validate_write(vault, path, known, bytes)?;
    apply(vault, &change, &change.before, &change.after)?;
    let (actual, stamp) = vault.read_bytes(path).map_err(|e| e.to_string())?;
    if Some(actual) != change.after {
        return Err(format!("文件版本冲突：{path}"));
    }
    Ok(stamp)
}

fn digest(bytes: &[u8]) -> String {
    format!("{:016x}", crate::parse::hash_bytes(bytes))
}

fn current(vault: &Vault, path: &VaultPath) -> Result<Option<Vec<u8>>, String> {
    vault.validate_local_path(path).map_err(|e| e.to_string())?;
    match vault.read_bytes(path) {
        Ok((bytes, _)) => Ok(Some(bytes)),
        Err(crate::VaultError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(None)
        }
        Err(error) => Err(error.to_string()),
    }
}

fn check(vault: &Vault, change: &FileChange, expected: &Option<Vec<u8>>) -> Result<(), String> {
    if &current(vault, &change.path)? != expected {
        return Err(format!("文件版本冲突：{}", change.path));
    }
    if change.before == change.after {
        return Ok(());
    }
    let path = vault.fs_path(&change.path);
    if let Ok(metadata) = fs::metadata(&path)
        && metadata.permissions().readonly()
    {
        return Err(format!("文件只读：{}", change.path));
    }
    let mut directory = path.parent();
    while let Some(dir) = directory {
        match fs::metadata(dir) {
            Ok(meta) if meta.permissions().readonly() => return Err(format!("目录只读：{}", dir.display())),
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => directory = dir.parent(),
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static FAIL_MANIFEST_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_DIRECTORY_SYNC: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(test)]
    if FAIL_DIRECTORY_SYNC.with(|fail| {
        let mut fail = fail.borrow_mut();
        if fail.as_deref() == Some(path) {
            fail.take();
            true
        } else {
            false
        }
    }) {
        return Err("injected directory sync failure".into());
    }
    File::open(path).and_then(|file| file.sync_all()).map_err(|e| e.to_string())
}

fn create_durable_directory(path: &Path) -> Result<(), String> {
    if path.is_dir() {
        return Ok(());
    }
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
    create_durable_directory(parent)?;
    match fs::create_dir(path) {
        Ok(()) => {
            if let Err(error) = sync_directory(parent) {
                let _ = fs::remove_dir(path);
                return Err(error);
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {
            sync_directory(parent)
        }
        Err(error) => Err(error.to_string()),
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path).map_err(|e| e.to_string())?;
    file.write_all(bytes).and_then(|_| file.sync_all()).map_err(|e| e.to_string())
}

fn apply(
    vault: &Vault,
    change: &FileChange,
    expected: &Option<Vec<u8>>,
    after: &Option<Vec<u8>>,
) -> Result<(), String> {
    check(vault, change, expected)?;
    if expected == after {
        return Ok(());
    }
    match (expected, after) {
        (None, Some(bytes)) => {
            vault.create_bytes(&change.path, bytes).map_err(|e| e.to_string())?;
        }
        (Some(_), Some(bytes)) => {
            vault.write_bytes(&change.path, bytes).map_err(|e| e.to_string())?;
        }
        (Some(_), None) => {
            fs::remove_file(vault.fs_path(&change.path)).map_err(|e| e.to_string())?;
        }
        (None, None) => return Ok(()),
    }
    let path = vault.fs_path(&change.path);
    if after.is_some() {
        File::open(&path).and_then(|file| file.sync_all()).map_err(|e| e.to_string())?;
    }
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

impl Transaction {
    pub fn prepare(vault: &Vault, root: &Path, changes: Vec<FileChange>) -> Result<Self, String> {
        Self::prepare_restoring(vault, root, changes, HashMap::new())
    }

    /// Pending cuts already changed their source before this transaction.
    /// Their recovery image therefore differs from the version being validated.
    pub fn prepare_restoring(
        vault: &Vault,
        root: &Path,
        changes: Vec<FileChange>,
        restorations: HashMap<VaultPath, Vec<u8>>,
    ) -> Result<Self, String> {
        Self::prepare_new(vault, root, changes, restorations, None)
    }

    fn prepare_new(
        vault: &Vault,
        root: &Path,
        changes: Vec<FileChange>,
        restorations: HashMap<VaultPath, Vec<u8>>,
        cut: Option<CutIdentity>,
    ) -> Result<Self, String> {
        let mut transaction = Self::stage_new(vault, root, changes, restorations, cut)?;
        transaction.phase(transaction.manifest.phase)?;
        sync_directory(root)?;
        Ok(transaction)
    }

    fn stage_new(
        vault: &Vault,
        root: &Path,
        changes: Vec<FileChange>,
        restorations: HashMap<VaultPath, Vec<u8>>,
        cut: Option<CutIdentity>,
    ) -> Result<Self, String> {
        check_recovery(root)?;
        let mut paths = HashSet::new();
        for change in &changes {
            if !paths.insert(change.path.clone()) {
                return Err(format!("事务含重复文件：{}", change.path));
            }
            check(vault, change, &change.before)?;
        }
        if restorations.keys().any(|path| !paths.contains(path)) {
            return Err("恢复来源不在事务参与文件中".into());
        }
        let (observations, changes): (Vec<_>, Vec<_>) = changes
            .into_iter()
            .partition(|change| change.before == change.after && !restorations.contains_key(&change.path));
        create_durable_directory(root)?;
        let directory = root.join(ulid::Ulid::generate().to_string().to_ascii_lowercase());
        fs::create_dir(&directory).map_err(|e| e.to_string())?;
        let lease = std::sync::Arc::new(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(directory.join("lease"))
                .map_err(|e| e.to_string())?,
        );
        lease.try_lock().map_err(|e| e.to_string())?;
        let lease = Some(lease);
        let phase = if cut.is_some() { Phase::PendingCut } else { Phase::Prepared };
        let records = changes
            .iter()
            .map(|change| Record {
                path: change.path.as_str().into(),
                before: change.before.as_deref().map(digest),
                after: change.after.as_deref().map(digest),
                restore: restorations.get(&change.path).map(|b| digest(b)),
            })
            .collect();
        let transaction = Self {
            directory,
            changes,
            observations,
            observed_notes: None,
            restorations,
            lease,
            manifest: Manifest {
                version: 1,
                vault: vault.id().into(),
                phase,
                files: records,
                generation: String::new(),
                cut,
            },
        };
        let staging = (|| {
            for (i, change) in transaction.changes.iter().enumerate() {
                for (name, bytes) in [("before", &change.before), ("after", &change.after)] {
                    if let Some(bytes) = bytes {
                        write_synced(&transaction.directory.join(format!("{i}.{name}")), bytes)?;
                    }
                }
                if let Some(bytes) = transaction.restorations.get(&change.path) {
                    write_synced(&transaction.directory.join(format!("{i}.restore")), bytes)?;
                }
            }
            Ok::<_, String>(())
        })();
        if let Err(error) = staging {
            let _ = fs::remove_dir_all(&transaction.directory);
            return Err(error);
        }
        Ok(transaction)
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn image(&self, index: usize, name: &str) -> PathBuf {
        self.directory.join(format!("{}{index}.{name}", self.manifest.generation))
    }

    fn replace_generation(
        &mut self,
        vault: &Vault,
        changes: Vec<FileChange>,
        restorations: HashMap<VaultPath, Vec<u8>>,
        phase: Phase,
        cut: Option<CutIdentity>,
        mut on_image: impl FnMut(usize) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut paths = HashSet::new();
        for change in &changes {
            if !paths.insert(change.path.clone()) {
                return Err(format!("事务含重复文件：{}", change.path));
            }
            check(vault, change, &change.before)?;
        }
        if restorations.keys().any(|path| !paths.contains(path)) {
            return Err("恢复来源不在事务参与文件中".into());
        }
        let (observations, changes): (Vec<_>, Vec<_>) =
            changes.into_iter().partition(|c| c.before == c.after && !restorations.contains_key(&c.path));
        let records = changes
            .iter()
            .map(|c| Record {
                path: c.path.to_string(),
                before: c.before.as_deref().map(digest),
                after: c.after.as_deref().map(digest),
                restore: restorations.get(&c.path).map(|b| digest(b)),
            })
            .collect();
        let mut next = Self {
            directory: self.directory.clone(),
            changes,
            observations,
            observed_notes: self.observed_notes.clone(),
            restorations,
            lease: self.lease.clone(),
            manifest: Manifest {
                version: 1,
                vault: vault.id().into(),
                phase,
                files: records,
                generation: format!("{}-", ulid::Ulid::generate()),
                cut,
            },
        };
        let mut staged = Vec::new();
        let staging = (|| {
            let mut image = 0;
            for (i, c) in next.changes.iter().enumerate() {
                for (name, bytes) in [
                    ("before", c.before.as_ref()),
                    ("after", c.after.as_ref()),
                    ("restore", next.restorations.get(&c.path)),
                ] {
                    if let Some(bytes) = bytes {
                        let path = next.image(i, name);
                        staged.push(path.clone());
                        write_synced(&path, bytes)?;
                        on_image(image)?;
                        image += 1;
                    }
                }
            }
            on_image(image)
        })();
        if let Err(error) = staging {
            for path in staged {
                let _ = fs::remove_file(path);
            }
            return Err(error);
        }
        let result = next.phase(phase);
        let published = fs::read_to_string(self.directory.join("manifest.toml"))
            .ok()
            .and_then(|s| toml::from_str::<Manifest>(&s).ok());
        if result.is_ok() {
            *self = next;
            self.reclaim_images();
        } else if published.as_ref().is_none_or(|m| m.generation == next.manifest.generation) {
            // An uncertain publication must retain every possibly referenced image.
            *self = next;
        } else {
            for path in staged {
                let _ = fs::remove_file(path);
            }
        }
        result
    }

    fn reclaim_images(&self) {
        let active: HashSet<_> = self
            .manifest
            .files
            .iter()
            .enumerate()
            .flat_map(|(i, r)| {
                [("before", &r.before), ("after", &r.after), ("restore", &r.restore)]
                    .into_iter()
                    .filter_map(move |(name, hash)| hash.as_ref().map(|_| self.image(i, name)))
            })
            .collect();
        if let Ok(entries) = fs::read_dir(&self.directory) {
            for entry in entries.flatten() {
                let path = entry.path();
                if matches!(path.extension().and_then(|s| s.to_str()), Some("before" | "after" | "restore"))
                    && !active.contains(&path)
                {
                    let _ = fs::remove_file(path);
                }
            }
        }
    }

    fn phase(&mut self, phase: Phase) -> Result<(), String> {
        self.manifest.phase = phase;
        let bytes = toml::to_string(&self.manifest).map_err(|e| e.to_string())?;
        let temp = self.directory.join("manifest.next");
        if temp.exists() {
            fs::remove_file(&temp).map_err(|e| e.to_string())?;
        }
        write_synced(&temp, bytes.as_bytes())?;
        fs::rename(&temp, self.directory.join("manifest.toml")).map_err(|e| e.to_string())?;
        #[cfg(test)]
        if FAIL_MANIFEST_SYNC.with(|fail| fail.replace(false)) {
            return Err("injected directory sync failure after manifest rename".into());
        }
        sync_directory(&self.directory)
    }

    pub fn observe_notes(mut self, expected: BTreeSet<VaultPath>) -> Self {
        self.observed_notes = Some(expected);
        self
    }

    fn check_notes(&self, vault: &Vault) -> Result<(), String> {
        if let Some(expected) = &self.observed_notes
            && &vault.notes().map_err(|e| e.to_string())?.into_iter().collect::<BTreeSet<_>>() != expected
        {
            return Err("笔记列表已变化，请重新选择来源".into());
        }
        Ok(())
    }

    pub fn commit(self, vault: &Vault) -> Result<Vec<FileChange>, String> {
        self.commit_with(vault, |_| Ok(()))
    }

    pub fn cancel(mut self, vault: &Vault) -> Result<(), String> {
        self.rollback(vault)
    }

    fn commit_with(
        mut self,
        vault: &Vault,
        mut hook: impl FnMut(usize) -> Result<(), String>,
    ) -> Result<Vec<FileChange>, String> {
        let commit = (|| {
            // Validate all participants again before the first write.
            for change in self.changes.iter().chain(&self.observations) {
                check(vault, change, &change.before)?;
            }
            self.check_notes(vault)?;
            self.phase(Phase::Committing)?;
            for (i, change) in self.changes.iter().enumerate() {
                apply(vault, change, &change.before, &change.after)?;
                hook(i)?;
            }
            // A checked reference or an earlier participant may change while
            // later files are written. Revalidate before sealing the journal.
            for change in &self.changes {
                check(vault, change, &change.after)?;
            }
            for change in &self.observations {
                check(vault, change, &change.before)?;
            }
            self.check_notes(vault)?;
            self.phase(Phase::Committed)
        })();
        if let Err(cause) = commit {
            let rollback = self.rollback(vault);
            return Err(match rollback {
                Ok(()) => format!("事务失败，已恢复原内容：{cause}"),
                Err(error) => {
                    format!("事务失败：{cause}；恢复冲突：{error}；恢复副本：{}", self.directory.display())
                }
            });
        }
        // A failed cleanup is harmless: committed journals are idempotently checked on startup.
        let _ = fs::remove_dir_all(&self.directory);
        Ok(self.changes)
    }

    fn rollback(&mut self, vault: &Vault) -> Result<(), String> {
        self.phase(Phase::RollingBack)?;
        let mut conflicts = Vec::new();
        for change in self.changes.iter().rev() {
            let restore = self.restorations.get(&change.path).cloned().or_else(|| change.before.clone());
            match current(vault, &change.path) {
                Ok(value) if value == restore => {}
                Ok(value) if value == change.after || value == change.before => {
                    let recovery = FileChange {
                        path: change.path.clone(),
                        before: value.clone(),
                        after: restore.clone(),
                    };
                    if let Err(error) = apply(vault, &recovery, &value, &restore) {
                        conflicts.push(error);
                    }
                }
                Ok(_) => conflicts.push(format!("{} 已被外部修改，保留当前文件", change.path)),
                Err(error) => conflicts.push(error),
            }
        }
        if conflicts.is_empty() {
            fs::remove_dir_all(&self.directory).map_err(|e| e.to_string())?;
            Ok(())
        } else {
            Err(conflicts.join("；"))
        }
    }

    fn load(vault: &Vault, directory: PathBuf) -> Result<Self, String> {
        let source = fs::read_to_string(directory.join("manifest.toml")).map_err(|e| e.to_string())?;
        let manifest: Manifest = toml::from_str(&source).map_err(|e| e.to_string())?;
        if manifest.version != 1 || manifest.vault != vault.id() {
            return Err("事务日志版本或笔记库不匹配".into());
        }
        let mut paths = HashSet::new();
        let mut changes = Vec::new();
        let mut restorations = HashMap::new();
        for (i, record) in manifest.files.iter().enumerate() {
            let path = VaultPath::new(&record.path).map_err(|e| e.to_string())?;
            if !paths.insert(path.clone()) {
                return Err("事务日志含重复文件".into());
            }
            let read = |name, hash: &Option<String>| -> Result<Option<Vec<u8>>, String> {
                let Some(hash) = hash else { return Ok(None) };
                let bytes = fs::read(directory.join(format!("{}{i}.{name}", manifest.generation)))
                    .map_err(|e| e.to_string())?;
                if &digest(&bytes) != hash {
                    return Err("恢复镜像校验失败".into());
                }
                Ok(Some(bytes))
            };
            changes.push(FileChange {
                path,
                before: read("before", &record.before)?,
                after: read("after", &record.after)?,
            });
            if let Some(bytes) = read("restore", &record.restore)? {
                restorations.insert(changes.last().unwrap().path.clone(), bytes);
            }
        }
        Ok(Self {
            directory,
            manifest,
            changes,
            observations: Vec::new(),
            observed_notes: None,
            restorations,
            lease: None,
        })
    }
}

/// Honor other windows' recovery failures without rolling back live transactions.
pub fn check_recovery(root: &Path) -> Result<(), String> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path().join("manifest.toml");
        if !path.exists() {
            continue;
        }
        let manifest: Manifest = toml::from_str(&fs::read_to_string(&path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        if manifest.phase == Phase::RollingBack {
            return Err(format!("工作区恢复尚未完成：{}", path.display()));
        }
    }
    Ok(())
}

/// Must finish before opening writable editors. Conflicts retain all journal versions.
pub fn recover(vault: &Vault, root: &Path) -> Result<(), String> {
    recover_with(vault, root, |_| Ok(()))
}

fn recover_with(
    vault: &Vault,
    root: &Path,
    mut acquired: impl FnMut(&Path) -> Result<(), String>,
) -> Result<(), String> {
    if !root.exists() {
        return Ok(());
    }
    let mut errors = Vec::new();
    for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            continue;
        }
        let directory = entry.path();
        if !directory.join("manifest.toml").exists() {
            continue;
        }
        let result = (|| {
            let file = match OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(directory.join("lease"))
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.to_string()),
            };
            let lease = match file.try_lock() {
                Ok(()) => Some(std::sync::Arc::new(file)),
                Err(std::fs::TryLockError::WouldBlock) => {
                    let source = match fs::read_to_string(directory.join("manifest.toml")) {
                        Ok(source) => source,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                        Err(error) => return Err(error.to_string()),
                    };
                    let manifest: Manifest = toml::from_str(&source).map_err(|e| e.to_string())?;
                    if manifest.phase == Phase::RollingBack {
                        return Err("仍有未完成的事务恢复".into());
                    }
                    return Ok(());
                }
                Err(error) => return Err(error.to_string()),
            };
            if !directory.join("manifest.toml").exists() {
                return Ok(());
            }
            acquired(&directory)?;
            let mut transaction = Transaction::load(vault, directory.clone())?;
            transaction.lease = lease;
            if transaction.manifest.phase == Phase::Committed {
                for change in &transaction.changes {
                    if current(vault, &change.path)? != change.after {
                        return Err(format!("已提交事务的 {} 有新版本，保留日志供恢复", change.path));
                    }
                }
                fs::remove_dir_all(&directory).map_err(|e| e.to_string())
            } else {
                transaction.rollback(vault)
            }
        })();
        if let Err(error) = result {
            errors.push(format!("{}：{error}", directory.display()));
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(errors.join("\n")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publish_cut(
        cut: &mut PendingCut,
        vault: &Vault,
        bytes: Vec<u8>,
        restore: Vec<u8>,
        version: u64,
    ) -> Result<crate::FileStamp, String> {
        let change =
            FileChange { path: cut.source.clone(), before: current(vault, &cut.source)?, after: Some(bytes) };
        cut.publish(vault, change, restore, version)
    }

    fn restore_cut(
        cut: &mut PendingCut,
        vault: &Vault,
        bytes: Vec<u8>,
        version: u64,
    ) -> Result<crate::FileStamp, String> {
        let change =
            FileChange { path: cut.source.clone(), before: current(vault, &cut.source)?, after: Some(bytes) };
        cut.restore(vault, change, version)
    }

    #[test]
    fn observations_are_checked_without_journaling_or_requiring_write_access() {
        let (_temp, vault, root, mut changes) = fixture();
        let path = VaultPath::new("read-only.md").unwrap();
        vault.create_bytes(&path, b"observed").unwrap();
        let original = fs::metadata(vault.fs_path(&path)).unwrap().permissions();
        let mut permissions = original.clone();
        permissions.set_readonly(true);
        fs::set_permissions(vault.fs_path(&path), permissions).unwrap();
        changes.push(FileChange {
            path: path.clone(),
            before: Some(b"observed".to_vec()),
            after: Some(b"observed".to_vec()),
        });
        let transaction = Transaction::prepare(&vault, &root, changes.clone()).unwrap();
        assert_eq!(transaction.manifest.files.len(), 2);
        transaction.commit(&vault).unwrap();
        let transaction =
            Transaction::prepare(&vault, &root, changes.iter().map(FileChange::reverse).collect()).unwrap();
        fs::set_permissions(vault.fs_path(&path), original).unwrap();
        fs::write(vault.fs_path(&path), b"external").unwrap();
        assert!(transaction.commit(&vault).is_err());
        assert_eq!(vault.read_bytes(&changes[0].path).unwrap().0, b"changed");
    }

    fn fixture() -> (tempfile::TempDir, Vault, PathBuf, Vec<FileChange>) {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("vault")).unwrap();
        let vault = Vault::open(temp.path().join("vault")).unwrap();
        let first = VaultPath::new("first.md").unwrap();
        vault.create_bytes(&first, b"first").unwrap();
        let changes = vec![
            FileChange { path: first, before: Some(b"first".to_vec()), after: Some(b"changed".to_vec()) },
            FileChange {
                path: VaultPath::new("second.md").unwrap(),
                before: None,
                after: Some(b"created".to_vec()),
            },
        ];
        let root = temp.path().join("journal");
        (temp, vault, root, changes)
    }

    #[test]
    fn g07_managed_note_membership_changes_roll_back_without_removing_external_note() {
        for during_commit in [false, true] {
            let (_temp, vault, root, mut changes) = fixture();
            vault.create_bytes(&changes[1].path, b"second").unwrap();
            changes[1].before = Some(b"second".to_vec());
            let membership = vault.notes().unwrap().into_iter().collect();
            let tx = Transaction::prepare(&vault, &root, changes.clone()).unwrap().observe_notes(membership);
            let external = VaultPath::new("duplicate.md").unwrap();
            let duplicate = changes[0].after.as_ref().unwrap();
            if !during_commit {
                vault.create_bytes(&external, duplicate).unwrap();
            }
            let result = tx.commit_with(&vault, |i| {
                if during_commit && i == 1 {
                    vault.create_bytes(&external, duplicate).unwrap();
                }
                Ok(())
            });
            assert!(result.is_err(), "membership mismatch must abort, during_commit={during_commit}");
            for change in &changes {
                assert_eq!(current(&vault, &change.path).unwrap(), change.before);
            }
            assert_eq!(vault.read_bytes(&external).unwrap().0, *duplicate);
        }
    }

    #[test]
    fn failure_rolls_back_every_written_file_and_creation() {
        for failure in 0..2 {
            let (_temp, vault, root, changes) = fixture();
            let tx = Transaction::prepare(&vault, &root, changes.clone()).unwrap();
            assert!(
                tx.commit_with(&vault, |i| if i == failure {
                    Err("injected failure".into())
                } else {
                    Ok(())
                })
                .is_err()
            );
            for change in changes {
                assert_eq!(current(&vault, &change.path).unwrap(), change.before);
            }
        }
    }

    #[test]
    fn changes_during_commit_are_detected_before_success() {
        for participant in [false, true] {
            let (_temp, vault, root, mut changes) = fixture();
            let observed = VaultPath::new("observed.md").unwrap();
            vault.create_bytes(&observed, b"reference").unwrap();
            changes.push(FileChange {
                path: observed.clone(),
                before: Some(b"reference".to_vec()),
                after: Some(b"reference".to_vec()),
            });
            let changed = if participant { changes[0].path.clone() } else { observed };
            let tx = Transaction::prepare(&vault, &root, changes.clone()).unwrap();
            let journal = tx.directory.clone();
            assert!(
                tx.commit_with(&vault, |i| {
                    if i == 1 {
                        vault.write_bytes(&changed, b"external").map_err(|e| e.to_string())?;
                    }
                    Ok(())
                })
                .is_err()
            );
            assert_eq!(vault.read_bytes(&changed).unwrap().0, b"external");
            assert_eq!(current(&vault, &changes[1].path).unwrap(), None);
            if participant {
                assert!(journal.join("0.before").exists());
            } else {
                assert_eq!(current(&vault, &changes[0].path).unwrap(), changes[0].before);
            }
        }
    }

    #[test]
    fn interrupted_commit_recovers_and_never_overwrites_external_edits() {
        for external in [false, true] {
            let (_temp, vault, root, changes) = fixture();
            let mut tx = Transaction::prepare(&vault, &root, changes.clone()).unwrap();
            tx.phase(Phase::Committing).unwrap();
            apply(&vault, &changes[0], &changes[0].before, &changes[0].after).unwrap();
            if external {
                vault.write_bytes(&changes[0].path, b"external").unwrap();
            }
            let directory = tx.directory.clone();
            drop(tx);
            let result = recover(&vault, &root);
            if external {
                assert!(result.is_err());
                assert_eq!(current(&vault, &changes[0].path).unwrap(), Some(b"external".to_vec()));
                assert!(directory.join("0.before").exists());
            } else {
                result.unwrap();
                assert_eq!(current(&vault, &changes[0].path).unwrap(), changes[0].before);
                recover(&vault, &root).unwrap();
            }
        }
    }

    #[test]
    fn g11_new_journal_directories_are_durable_before_writes() {
        let (_temp, vault, root, changes) = fixture();
        let root = root.join("new-transactions").join("vault");
        FAIL_DIRECTORY_SYNC.with(|fail| *fail.borrow_mut() = Some(root.parent().unwrap().to_owned()));
        let result = Transaction::prepare(&vault, &root, changes.clone()).and_then(|tx| tx.commit(&vault));
        FAIL_DIRECTORY_SYNC.with(|fail| fail.borrow_mut().take());
        assert!(result.is_err());
        for change in changes {
            assert_eq!(current(&vault, &change.path).unwrap(), change.before);
        }
    }

    #[test]
    fn g11_lazy_initialization_rejects_changes_after_admission() {
        let (_temp, vault, root, changes) = fixture();
        let path = changes[0].path.clone();
        let (_, stamp) = vault.read_bytes(&path).unwrap();
        let admitted = validate_write(&vault, &path, stamp, b"changed".to_vec()).unwrap();
        let mut cut = PendingCut::unpublished(root.clone(), path.clone(), "token".into());
        vault.write_bytes(&path, b"external version").unwrap();
        assert!(cut.initialize(&vault, admitted, b"first".to_vec(), 1).is_err());
        assert_eq!(vault.read_bytes(&path).unwrap().0, b"external version");
        assert!(cut.is_unpublished());
    }

    #[test]
    fn g11_lazy_initialization_failure_retains_the_only_live_owner() {
        let (_temp, vault, root, changes) = fixture();
        let mut cut = PendingCut::unpublished(root.clone(), changes[0].path.clone(), "token".into());
        FAIL_MANIFEST_SYNC.with(|fail| fail.set(true));
        assert!(cut.initialize(&vault, changes[0].clone(), b"first later".to_vec(), 1).is_err());
        assert!(!cut.is_unpublished(), "published initialization must retain its lease and journal");
        recover(&vault, &root).unwrap();
        assert_eq!(current(&vault, &changes[0].path).unwrap(), Some(b"first".to_vec()));
        publish_cut(&mut cut, &vault, b"changed".to_vec(), b"first later".to_vec(), 2).unwrap();
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        drop(cut);
        recover(&vault, &root).unwrap();
        assert_eq!(current(&vault, &changes[0].path).unwrap(), Some(b"first later".to_vec()));
    }

    #[test]
    fn g11_v1_recovery_acquires_a_lease_before_touching_files() {
        let (_temp, vault, root, changes) = fixture();
        let mut tx = Transaction::prepare(&vault, &root, changes.clone()).unwrap();
        tx.phase(Phase::Committing).unwrap();
        apply(&vault, &changes[0], &changes[0].before, &changes[0].after).unwrap();
        let directory = tx.directory.clone();
        drop(tx);
        fs::remove_file(directory.join("lease")).unwrap();
        let manifest = fs::read_to_string(directory.join("manifest.toml")).unwrap();
        fs::write(directory.join("manifest.toml"), manifest.replace("generation = \"\"\n", "")).unwrap();
        let mut observed = false;
        recover_with(&vault, &root, |directory| {
            observed = true;
            let lease = OpenOptions::new().read(true).write(true).open(directory.join("lease")).unwrap();
            assert!(matches!(lease.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
            recover(&vault, &root)?;
            assert_eq!(current(&vault, &changes[0].path)?, changes[0].after);
            Ok(())
        })
        .unwrap();
        assert!(observed);
        assert_eq!(current(&vault, &changes[0].path).unwrap(), changes[0].before);
        recover(&vault, &root).unwrap();
    }

    #[test]
    fn g11_live_joint_transaction_is_leased_until_owner_drops() {
        let (_temp, vault, root, changes) = fixture();
        Transaction::prepare(&vault, &root, changes.clone())
            .unwrap()
            .commit_with(&vault, |i| {
                recover(&vault, &root)?;
                assert_eq!(current(&vault, &changes[i].path)?, changes[i].after);
                Ok(())
            })
            .unwrap();
        let mut tx =
            Transaction::prepare(&vault, &root, changes.iter().map(FileChange::reverse).collect()).unwrap();
        tx.phase(Phase::Committing).unwrap();
        let change = &tx.changes[0];
        apply(&vault, change, &change.before, &change.after).unwrap();
        drop(tx);
        recover(&vault, &root).unwrap();
        for change in changes {
            assert_eq!(current(&vault, &change.path).unwrap(), change.after);
        }
    }

    #[test]
    fn g11_generations_are_bounded_and_failed_staging_is_reclaimed() {
        let (_temp, vault, root, changes) = fixture();
        let mut cut =
            PendingCut::prepare(&vault, &root, "token", changes[0].clone(), b"first".to_vec(), 1).unwrap();
        for version in 1..=20 {
            publish_cut(&mut cut, &vault, b"changed".to_vec(), b"first".to_vec(), version).unwrap();
            let images = fs::read_dir(cut.directory())
                .unwrap()
                .map(Result::unwrap)
                .filter(|e| {
                    matches!(
                        e.path().extension().and_then(|s| s.to_str()),
                        Some("before" | "after" | "restore")
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(images.len(), 3, "only the active generation should remain");
            assert!(images.iter().map(|e| e.metadata().unwrap().len()).sum::<u64>() <= 19);
        }
        let tx = cut.transaction.as_mut().unwrap();
        let count = fs::read_dir(&tx.directory).unwrap().count();
        for failure in 0..=3 {
            assert!(
                tx.replace_generation(
                    &vault,
                    vec![FileChange {
                        path: changes[0].path.clone(),
                        before: Some(b"changed".to_vec()),
                        after: Some(b"next".to_vec())
                    }],
                    HashMap::from([(changes[0].path.clone(), b"first".to_vec())]),
                    Phase::PendingCut,
                    tx.manifest.cut.clone(),
                    |image| if image == failure { Err("staging failed".into()) } else { Ok(()) },
                )
                .is_err()
            );
            assert_eq!(fs::read_dir(&tx.directory).unwrap().count(), count);
        }
        drop(cut);
        recover(&vault, &root).unwrap();
        assert_eq!(current(&vault, &changes[0].path).unwrap(), Some(b"first".to_vec()));
    }

    #[test]
    fn g11_published_upgrade_failure_cancels_the_entire_transaction() {
        for restart in [false, true] {
            let (_temp, vault, root, changes) = fixture();
            let target = VaultPath::new("a-target.md").unwrap();
            let mut cut =
                PendingCut::prepare(&vault, &root, "token", changes[0].clone(), b"first".to_vec(), 1)
                    .unwrap();
            publish_cut(&mut cut, &vault, b"changed".to_vec(), b"first".to_vec(), 1).unwrap();
            FAIL_MANIFEST_SYNC.with(|fail| fail.set(true));
            assert!(
                cut.prepare_paste(
                    &vault,
                    vec![
                        FileChange { path: target.clone(), before: None, after: Some(b"moved".to_vec()) },
                        FileChange {
                            path: changes[0].path.clone(),
                            before: Some(b"changed".to_vec()),
                            after: Some(b"changed".to_vec())
                        },
                    ],
                    HashMap::from([(changes[0].path.clone(), b"first".to_vec())])
                )
                .is_err()
            );
            assert!(!cut.is_pending(), "a published upgrade is not a single-source cut");
            if !restart {
                restore_cut(&mut cut, &vault, b"first".to_vec(), 1).unwrap();
                cut.recover_cancel(&vault).unwrap();
            }
            drop(cut);
            recover(&vault, &root).unwrap();
            recover(&vault, &root).unwrap();
            assert_eq!(current(&vault, &changes[0].path).unwrap(), Some(b"first".to_vec()));
            assert_eq!(current(&vault, &target).unwrap(), None);
        }
    }

    #[test]
    fn g11_pending_cut_upgrade_must_retain_source_and_recovery_identity() {
        let (_temp, vault, root, changes) = fixture();
        let mut cut =
            PendingCut::prepare(&vault, &root, "token", changes[0].clone(), b"first".to_vec(), 1).unwrap();
        publish_cut(&mut cut, &vault, b"changed".to_vec(), b"first".to_vec(), 1).unwrap();
        assert!(
            cut.prepare_paste(&vault, vec![changes[1].clone()], HashMap::new()).is_err(),
            "upgrade cannot drop the only recovery owner of the saved deletion"
        );
        assert!(cut.is_pending());
        drop(cut);
        recover(&vault, &root).unwrap();
        assert_eq!(current(&vault, &changes[0].path).unwrap(), Some(b"first".to_vec()));
    }

    #[test]
    fn g11_pending_cut_generations_survive_image_and_manifest_interruption() {
        for failure in [Some(0), Some(3), None] {
            let swapped = failure.is_none();
            let (_temp, vault, root, changes) = fixture();
            let mut cut =
                PendingCut::prepare(&vault, &root, "token", changes[0].clone(), b"first".to_vec(), 1)
                    .unwrap();
            publish_cut(&mut cut, &vault, b"changed".to_vec(), b"first".to_vec(), 1).unwrap();
            let tx = cut.transaction.as_mut().unwrap();
            let old_manifest = fs::read(tx.directory.join("manifest.toml")).unwrap();
            let next = FileChange {
                path: changes[0].path.clone(),
                before: Some(b"changed".to_vec()),
                after: Some(b"changed later".to_vec()),
            };
            let result = tx.replace_generation(
                &vault,
                vec![next],
                HashMap::from([(changes[0].path.clone(), b"first later".to_vec())]),
                Phase::PendingCut,
                Some(CutIdentity { token: "token".into(), version: 2 }),
                |image| {
                    if failure == Some(image) {
                        Err("crash during generation publication".into())
                    } else {
                        Ok(())
                    }
                },
            );
            if !swapped {
                assert!(result.is_err());
                assert_eq!(fs::read(tx.directory.join("manifest.toml")).unwrap(), old_manifest);
            } else {
                result.unwrap();
            }
            drop(cut);
            recover(&vault, &root).unwrap();
            assert_eq!(
                vault.read_bytes(&changes[0].path).unwrap().0,
                if swapped { b"first later".as_slice() } else { b"first".as_slice() }
            );
        }
    }

    #[test]
    fn g11_unresolved_live_cut_recovery_blocks_reopen_and_further_publication() {
        let (_temp, vault, root, changes) = fixture();
        let mut cut =
            PendingCut::prepare(&vault, &root, "token", changes[0].clone(), b"first".to_vec(), 1).unwrap();
        publish_cut(&mut cut, &vault, b"changed".to_vec(), b"first".to_vec(), 1).unwrap();
        vault.write_bytes(&changes[0].path, b"external").unwrap();
        assert!(cut.recover_cancel(&vault).is_err());
        assert!(recover(&vault, &root).is_err(), "a live lease must not hide unresolved recovery");
        vault.write_bytes(&changes[0].path, b"changed").unwrap();
        assert!(
            publish_cut(&mut cut, &vault, b"new deletion".to_vec(), b"first later".to_vec(), 2).is_err(),
            "only explicit restoration may unfreeze a failed cancellation"
        );
        restore_cut(&mut cut, &vault, b"first later".to_vec(), 2).unwrap();
        assert_eq!(vault.read_bytes(&changes[0].path).unwrap().0, b"first later");
        assert!(!cut.directory().exists());
    }

    #[test]
    fn g11_successful_paste_seals_the_same_journal_before_recovery() {
        let (_temp, vault, root, changes) = fixture();
        let mut cut =
            PendingCut::prepare(&vault, &root, "token", changes[0].clone(), b"first".to_vec(), 1).unwrap();
        publish_cut(&mut cut, &vault, b"changed".to_vec(), b"first".to_vec(), 1).unwrap();
        let mut paste = vec![changes[1].clone()];
        paste.push(FileChange {
            path: changes[0].path.clone(),
            before: Some(b"changed".to_vec()),
            after: Some(b"changed".to_vec()),
        });
        let mut tx = cut
            .prepare_paste(&vault, paste, HashMap::from([(changes[0].path.clone(), b"first".to_vec())]))
            .unwrap();
        assert_eq!(tx.directory(), cut.directory());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        tx.phase(Phase::Committing).unwrap();
        for c in &tx.changes {
            apply(&vault, c, &c.before, &c.after).unwrap();
        }
        tx.phase(Phase::Committed).unwrap();
        drop(tx);
        drop(cut);
        recover(&vault, &root).unwrap();
        assert_eq!(vault.read_bytes(&changes[0].path).unwrap().0, b"changed");
        assert_eq!(vault.read_bytes(&changes[1].path).unwrap().0, b"created");
    }

    #[test]
    fn pending_cut_recovery_restores_source_even_before_its_first_write() {
        for written in [false, true] {
            let (_temp, vault, root, mut changes) = fixture();
            let source = VaultPath::new("source.md").unwrap();
            vault.create_bytes(&source, b"cut state").unwrap();
            changes.push(FileChange {
                path: source.clone(),
                before: Some(b"cut state".to_vec()),
                after: Some(b"cut state".to_vec()),
            });
            let mut tx = Transaction::prepare_restoring(
                &vault,
                &root,
                changes.clone(),
                HashMap::from([(source.clone(), b"original with cut block".to_vec())]),
            )
            .unwrap();
            tx.phase(Phase::Committing).unwrap();
            if written {
                apply(&vault, &changes[0], &changes[0].before, &changes[0].after).unwrap();
            }
            drop(tx);
            recover(&vault, &root).unwrap();
            assert_eq!(vault.read_bytes(&source).unwrap().0, b"original with cut block");
            assert_eq!(vault.read_bytes(&changes[0].path).unwrap().0, b"first");
        }
    }
}
