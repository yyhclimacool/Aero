//! Recoverable multi-file writes. Each file is atomic; the workspace is journaled.

use std::collections::{HashMap, HashSet};
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

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u8,
    vault: String,
    phase: Phase,
    files: Vec<Record>,
}

pub struct Transaction {
    directory: PathBuf,
    manifest: Manifest,
    changes: Vec<FileChange>,
    observations: Vec<FileChange>,
    restorations: HashMap<VaultPath, Vec<u8>>,
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

fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path).and_then(|file| file.sync_all()).map_err(|e| e.to_string())
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
        fs::create_dir_all(root).map_err(|e| e.to_string())?;
        let directory = root.join(ulid::Ulid::generate().to_string().to_ascii_lowercase());
        fs::create_dir(&directory).map_err(|e| e.to_string())?;
        let records = changes
            .iter()
            .map(|change| Record {
                path: change.path.as_str().into(),
                before: change.before.as_deref().map(digest),
                after: change.after.as_deref().map(digest),
                restore: restorations.get(&change.path).map(|b| digest(b)),
            })
            .collect();
        let mut transaction = Self {
            directory,
            changes,
            observations,
            restorations,
            manifest: Manifest {
                version: 1,
                vault: vault.id().into(),
                phase: Phase::Prepared,
                files: records,
            },
        };
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
        transaction.phase(Phase::Prepared)?;
        sync_directory(root)?;
        Ok(transaction)
    }

    pub fn directory(&self) -> &Path {
        &self.directory
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
        sync_directory(&self.directory)
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
                let bytes = fs::read(directory.join(format!("{i}.{name}"))).map_err(|e| e.to_string())?;
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
        Ok(Self { directory, manifest, changes, observations: Vec::new(), restorations })
    }
}

/// Must finish before opening writable editors. Conflicts retain all journal versions.
pub fn recover(vault: &Vault, root: &Path) -> Result<(), String> {
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
            let mut transaction = Transaction::load(vault, directory.clone())?;
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
            let result = recover(&vault, &root);
            if external {
                assert!(result.is_err());
                assert_eq!(current(&vault, &changes[0].path).unwrap(), Some(b"external".to_vec()));
                assert!(tx.directory.join("0.before").exists());
            } else {
                result.unwrap();
                assert_eq!(current(&vault, &changes[0].path).unwrap(), changes[0].before);
                recover(&vault, &root).unwrap();
            }
        }
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
            recover(&vault, &root).unwrap();
            assert_eq!(vault.read_bytes(&source).unwrap().0, b"original with cut block");
            assert_eq!(vault.read_bytes(&changes[0].path).unwrap().0, b"first");
        }
    }
}
