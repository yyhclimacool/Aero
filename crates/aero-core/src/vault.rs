//! Vault：一个普通文件夹（方案第 5 节）。
//!
//! 只负责路径、枚举笔记、读取与原子写入。索引与缓存不在 vault 内（5.3），这里不涉及。

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::document::Document;

/// vault 内的相对路径：`/` 分隔、无前导 `/`、无 `.`/`..` 分量。
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VaultPath(String);

impl VaultPath {
    pub fn new(s: &str) -> Result<Self, VaultError> {
        let s = s.replace('\\', "/");
        let s = s.strip_prefix("./").unwrap_or(&s);
        if s.is_empty() || s.starts_with('/') || s.ends_with('/') {
            return Err(VaultError::InvalidPath(s.to_owned()));
        }
        if s.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
            return Err(VaultError::InvalidPath(s.to_owned()));
        }
        Ok(VaultPath(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn to_fs(&self, root: &Path) -> PathBuf {
        let mut p = root.to_path_buf();
        p.extend(self.0.split('/'));
        p
    }

    pub fn from_fs(root: &Path, path: &Path) -> Result<Self, VaultError> {
        let rel = path.strip_prefix(root).map_err(|_| VaultError::InvalidPath(path.display().to_string()))?;
        let s = rel.to_str().ok_or_else(|| VaultError::InvalidPath(path.display().to_string()))?;
        Self::new(s)
    }

    pub fn is_note(&self) -> bool {
        self.0.ends_with(".md")
    }

    /// 文件名（含扩展名）
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// 不含 `.md` 的文件名，作为无 front matter `title` 时的默认标题
    pub fn stem(&self) -> &str {
        let name = self.file_name();
        name.strip_suffix(".md").unwrap_or(name)
    }

    /// 所在目录（vault 根为空串）
    pub fn parent(&self) -> &str {
        self.0.rsplit_once('/').map_or("", |(d, _)| d)
    }

    /// 该笔记的附件目录 `<note>.assets/`（5.4）
    pub fn assets_dir(&self) -> Option<VaultPath> {
        self.is_note().then(|| VaultPath(format!("{}.assets", self.0.strip_suffix(".md").unwrap())))
    }
}

impl fmt::Display for VaultPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 文件在某一时刻的指纹，用于变更检测与写前校验（5.5）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    pub mtime: SystemTime,
    pub size: u64,
    /// 内容 xxh3-64
    pub hash: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid vault path {0:?}")]
    InvalidPath(String),
    #[error("{0} is not a directory")]
    NotADirectory(PathBuf),
    #[error(".aero/vault.toml: {0}")]
    Config(String),
    #[error("{path}: not valid UTF-8")]
    Utf8 { path: PathBuf },
    #[error("walk: {0}")]
    Walk(String),
    #[error("{0} already exists")]
    AlreadyExists(String),
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> VaultError + '_ {
    move |source| VaultError::Io { path: path.to_path_buf(), source }
}

/// `.aero/vault.toml`（5.2）
#[derive(Debug, Serialize, Deserialize)]
struct VaultToml {
    vault_id: String,
    format: u32,
}

const VAULT_FORMAT: u32 = 1;

#[derive(Debug, Clone)]
pub struct Vault {
    root: PathBuf,
    id: String,
}

impl Vault {
    /// 打开一个 vault；没有 `.aero/vault.toml` 时创建（这是打开已有文件夹时唯一会写入的东西）。
    pub fn open(root: impl AsRef<Path>) -> Result<Vault, VaultError> {
        let root = root.as_ref();
        if !root.is_dir() {
            return Err(VaultError::NotADirectory(root.to_path_buf()));
        }
        let root = root.canonicalize().map_err(io_err(root))?;
        let cfg_path = root.join(".aero").join("vault.toml");
        let id = if cfg_path.exists() {
            let s = fs::read_to_string(&cfg_path).map_err(io_err(&cfg_path))?;
            let cfg: VaultToml = toml::from_str(&s).map_err(|e| VaultError::Config(e.to_string()))?;
            if cfg.format > VAULT_FORMAT {
                return Err(VaultError::Config(format!(
                    "format {} is newer than supported {VAULT_FORMAT}",
                    cfg.format
                )));
            }
            cfg.vault_id
        } else {
            let cfg = VaultToml { vault_id: ulid::Ulid::generate().to_string(), format: VAULT_FORMAT };
            fs::create_dir_all(cfg_path.parent().unwrap()).map_err(io_err(&cfg_path))?;
            let body = toml::to_string(&cfg).map_err(|e| VaultError::Config(e.to_string()))?;
            atomic_write(&cfg_path, body.as_bytes(), true)?;
            cfg.vault_id
        };
        Ok(Vault { root, id })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// ULID，用于关联应用数据目录下的索引
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn fs_path(&self, p: &VaultPath) -> PathBuf {
        p.to_fs(&self.root)
    }

    /// 枚举全部笔记（`*.md`），按路径排序。跳过隐藏目录（含 `.aero/`、`.git/`）、`*.assets/`，
    /// 尊重 `.gitignore` 与 `.aero/ignore`。
    pub fn notes(&self) -> Result<Vec<VaultPath>, VaultError> {
        let mut wb = ignore::WalkBuilder::new(&self.root);
        wb.hidden(true).git_ignore(true).require_git(false).follow_links(false);
        let custom = self.root.join(".aero").join("ignore");
        if custom.is_file()
            && let Some(e) = wb.add_ignore(&custom)
        {
            return Err(VaultError::Walk(e.to_string()));
        }
        wb.filter_entry(|e| {
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            !(is_dir && e.file_name().to_str().is_some_and(|n| n.ends_with(".assets")))
        });
        let mut out = Vec::new();
        for entry in wb.build() {
            let entry = entry.map_err(|e| VaultError::Walk(e.to_string()))?;
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            if entry.path().extension().is_some_and(|x| x == "md") {
                out.push(VaultPath::from_fs(&self.root, entry.path())?);
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn folders(&self) -> Result<Vec<VaultPath>, VaultError> {
        let mut walk = ignore::WalkBuilder::new(&self.root);
        walk.hidden(true).git_ignore(true).require_git(false).follow_links(false);
        let custom = self.root.join(".aero/ignore");
        if custom.is_file()
            && let Some(error) = walk.add_ignore(custom)
        {
            return Err(VaultError::Walk(error.to_string()));
        }
        walk.filter_entry(|e| !e.file_name().to_str().is_some_and(|name| name.ends_with(".assets")));
        let mut folders = Vec::new();
        for entry in walk.build() {
            let entry = entry.map_err(|e| VaultError::Walk(e.to_string()))?;
            if entry.depth() > 0 && entry.file_type().is_some_and(|t| t.is_dir()) {
                folders.push(VaultPath::from_fs(&self.root, entry.path())?);
            }
        }
        folders.sort();
        Ok(folders)
    }

    pub fn read_bytes(&self, p: &VaultPath) -> Result<(Vec<u8>, FileStamp), VaultError> {
        let path = self.fs_path(p);
        let bytes = fs::read(&path).map_err(io_err(&path))?;
        let meta = fs::metadata(&path).map_err(io_err(&path))?;
        let stamp = FileStamp {
            mtime: meta.modified().map_err(io_err(&path))?,
            size: meta.len(),
            hash: crate::parse::hash_bytes(&bytes),
        };
        Ok((bytes, stamp))
    }

    pub fn read(&self, p: &VaultPath) -> Result<(Document, FileStamp), VaultError> {
        let (bytes, stamp) = self.read_bytes(p)?;
        let doc = Document::from_bytes(&bytes).map_err(|_| VaultError::Utf8 { path: self.fs_path(p) })?;
        Ok((doc, stamp))
    }

    /// 只取 mtime 与大小（不读内容），用于快速判断文件是否可能变过。
    pub fn stat(&self, p: &VaultPath) -> Result<(SystemTime, u64), VaultError> {
        let path = self.fs_path(p);
        let meta = fs::metadata(&path).map_err(io_err(&path))?;
        Ok((meta.modified().map_err(io_err(&path))?, meta.len()))
    }

    /// 原子写入（临时文件 + rename），保留原文件权限。父目录不存在时创建。
    pub fn write_bytes(&self, p: &VaultPath, bytes: &[u8]) -> Result<FileStamp, VaultError> {
        self.write_bytes_impl(p, bytes, true)
    }

    /// 原子创建新文件；目标存在（含符号链接）时失败，绝不覆盖已有冲突副本。
    pub fn create_bytes(&self, p: &VaultPath, bytes: &[u8]) -> Result<FileStamp, VaultError> {
        self.write_bytes_impl(p, bytes, false)
    }

    fn write_bytes_impl(
        &self,
        p: &VaultPath,
        bytes: &[u8],
        overwrite: bool,
    ) -> Result<FileStamp, VaultError> {
        let path = self.fs_path(p);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(io_err(dir))?;
        }
        atomic_write(&path, bytes, overwrite)?;
        let meta = fs::metadata(&path).map_err(io_err(&path))?;
        Ok(FileStamp {
            mtime: meta.modified().map_err(io_err(&path))?,
            size: meta.len(),
            hash: crate::parse::hash_bytes(bytes),
        })
    }

    pub fn write(&self, p: &VaultPath, doc: &Document) -> Result<FileStamp, VaultError> {
        self.write_bytes(p, &doc.to_bytes())
    }

    /// 重命名 / 移动一篇笔记；`<note>.assets/` 存在时一起搬（5.4）。目标已存在时拒绝，绝不覆盖。
    /// 这里只动文件；包含链接更新的操作使用 [`crate::rename::RenamePlan`]。
    pub fn rename(&self, from: &VaultPath, to: &VaultPath) -> Result<(), VaultError> {
        if from == to {
            return Ok(());
        }
        let (src, dst) = (self.fs_path(from), self.fs_path(to));
        self.validate_rename(from, to)?;
        if let Some(dir) = dst.parent() {
            fs::create_dir_all(dir).map_err(io_err(dir))?;
        }
        if fs::symlink_metadata(&src).map_err(io_err(&src))?.is_dir() {
            return move_path(&src, &dst, true).map_err(io_err(&dst));
        }
        // create-only 发布新路径，目标在预检后被创建也不会被覆盖。
        // 同一文件系统用硬链接；跨设备（EXDEV）退回复制，调用方随后删掉源文件。
        publish_file(&src, &dst).map_err(io_err(&dst))?;
        let assets = from
            .assets_dir()
            .zip(to.assets_dir())
            .map(|(a, b)| (self.fs_path(&a), self.fs_path(&b)))
            .filter(|(a, _)| a.is_dir());
        if let Some((a, b)) = &assets {
            if let Err(error) = move_path(a, b, true) {
                let _ = fs::remove_file(&dst);
                return Err(io_err(a)(error));
            }
        }
        if let Err(error) = fs::remove_file(&src) {
            if let Some((a, b)) = assets {
                move_path(&b, &a, true).map_err(io_err(&b))?;
            }
            fs::remove_file(&dst).map_err(io_err(&dst))?;
            return Err(io_err(&src)(error));
        }
        Ok(())
    }

    pub(crate) fn validate_rename(&self, from: &VaultPath, to: &VaultPath) -> Result<(), VaultError> {
        self.validate_local_path(from)?;
        self.validate_local_path(to)?;
        let source = self.fs_path(from);
        let meta = fs::symlink_metadata(&source).map_err(io_err(&source))?;
        if (!meta.is_file() && !meta.is_dir())
            || (meta.is_dir() && to.as_str().starts_with(&format!("{from}/")))
        {
            return Err(VaultError::InvalidPath(from.to_string()));
        }
        let mut destinations = vec![to.clone()];
        if meta.is_file()
            && let Some((a, b)) = from.assets_dir().zip(to.assets_dir())
        {
            self.validate_local_path(&a)?;
            if let Ok(meta) = fs::symlink_metadata(self.fs_path(&a)) {
                if !meta.is_dir() {
                    return Err(VaultError::InvalidPath(a.to_string()));
                }
                self.validate_local_path(&b)?;
                destinations.push(b);
            }
        }
        for destination in destinations {
            let path = self.fs_path(&destination);
            match fs::symlink_metadata(&path) {
                Ok(_) => return Err(VaultError::AlreadyExists(destination.to_string())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(&path)(e)),
            }
        }
        Ok(())
    }

    pub(crate) fn validate_local_path(&self, path: &VaultPath) -> Result<(), VaultError> {
        let mut current = self.root.clone();
        for part in path.as_str().split('/') {
            current.push(part);
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(VaultError::InvalidPath(path.to_string()));
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => return Err(io_err(&current)(e)),
            }
        }
        Ok(())
    }

    /// 笔记是否已存在（用于重命名前的校验与生成不冲突的名字）。
    pub fn exists(&self, p: &VaultPath) -> bool {
        self.fs_path(p).exists()
    }
}

/// `rename` / `hard_link` 跨文件系统时的 errno。
fn crosses_devices(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(18)
}

/// 发布一个还不存在的目标文件。同一设备用硬链接；跨设备则复制，且不用会覆盖的 `copy`。
fn publish_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    match fs::hard_link(src, dst) {
        Ok(()) => Ok(()),
        Err(err) if crosses_devices(&err) => copy_new(src, dst),
        Err(err) => Err(err),
    }
}

/// 目录移动。跨设备时整棵复制再删掉源目录。
fn move_path(src: &Path, dst: &Path, is_dir: bool) -> std::io::Result<()> {
    let moved = if is_dir { rename_exclusive(src, dst) } else { publish_file(src, dst) };
    match moved {
        Ok(()) => Ok(()),
        Err(err) if is_dir && crosses_devices(&err) => {
            copy_tree(src, dst)?;
            fs::remove_dir_all(src)
        }
        Err(err) => Err(err),
    }
}

fn copy_new(src: &Path, dst: &Path) -> std::io::Result<()> {
    let mut input = fs::File::open(src)?;
    let mut output = fs::OpenOptions::new().write(true).create_new(true).open(dst)?;
    std::io::copy(&mut input, &mut output)?;
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        let meta = fs::symlink_metadata(entry.path())?;
        if meta.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else if meta.is_file() {
            copy_new(&entry.path(), &to)?;
        }
    }
    Ok(())
}

fn rename_exclusive(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::ffi::{CString, c_char, c_int};
        use std::os::unix::ffi::OsStrExt;
        let from = CString::new(from.as_os_str().as_bytes())?;
        let to = CString::new(to.as_os_str().as_bytes())?;
        #[cfg(target_os = "macos")]
        unsafe extern "C" {
            fn renamex_np(from: *const c_char, to: *const c_char, flags: u32) -> c_int;
        }
        #[cfg(target_os = "linux")]
        unsafe extern "C" {
            fn renameat2(
                from_dir: c_int,
                from: *const c_char,
                to_dir: c_int,
                to: *const c_char,
                flags: u32,
            ) -> c_int;
        }
        // RENAME_EXCL / RENAME_NOREPLACE refuses even a concurrently created empty directory.
        #[cfg(target_os = "macos")]
        let result = unsafe { renamex_np(from.as_ptr(), to.as_ptr(), 0x4) };
        #[cfg(target_os = "linux")]
        let result = unsafe { renameat2(-100, from.as_ptr(), -100, to.as_ptr(), 1) };
        if result == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
    }
    #[cfg(target_os = "windows")]
    {
        fs::rename(from, to)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (from, to);
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "exclusive directory rename is unsupported"))
    }
}

/// 写临时文件、fsync、rename 覆盖。目标已存在时沿用其权限位。
fn atomic_write(path: &Path, bytes: &[u8], overwrite: bool) -> Result<(), VaultError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.aero-tmp-{}-{}", std::process::id(), ulid::Ulid::generate()));
    let result = (|| {
        let mut f = fs::File::create(&tmp).map_err(io_err(&tmp))?;
        f.write_all(bytes).map_err(io_err(&tmp))?;
        f.sync_all().map_err(io_err(&tmp))?;
        if let Ok(meta) = fs::metadata(path) {
            let _ = fs::set_permissions(&tmp, meta.permissions());
        }
        if overwrite {
            fs::rename(&tmp, path).map_err(io_err(path))
        } else {
            // 同目录硬链接原子发布已 fsync 的完整文件；存在的目标不会被替换。
            fs::hard_link(&tmp, path).map_err(io_err(path))
        }
    })();
    if result.is_err() || !overwrite {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_bytes_never_overwrites_existing_file_or_directory() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let path = VaultPath::new("backup.md").unwrap();
        vault.create_bytes(&path, b"first complete version").unwrap();
        assert!(vault.create_bytes(&path, b"second").is_err());
        assert_eq!(fs::read(vault.fs_path(&path)).unwrap(), b"first complete version");
        let folder = VaultPath::new("folder.md").unwrap();
        fs::create_dir(vault.fs_path(&folder)).unwrap();
        assert!(vault.create_bytes(&folder, b"text").is_err());
        assert!(vault.fs_path(&folder).is_dir());
        assert!(
            !fs::read_dir(dir.path())
                .unwrap()
                .any(|entry| { entry.unwrap().file_name().to_string_lossy().contains("aero-tmp") })
        );
    }

    #[test]
    fn vault_path_rules() {
        assert_eq!(VaultPath::new("a/b.md").unwrap().as_str(), "a/b.md");
        assert_eq!(VaultPath::new("./a\\b.md").unwrap().as_str(), "a/b.md");
        assert!(VaultPath::new("/abs.md").is_err());
        assert!(VaultPath::new("a/../b.md").is_err());
        assert!(VaultPath::new("").is_err());
        assert!(VaultPath::new("a//b.md").is_err());

        let p = VaultPath::new("rust/ripgrep.md").unwrap();
        assert_eq!(p.stem(), "ripgrep");
        assert_eq!(p.parent(), "rust");
        assert_eq!(p.assets_dir().unwrap().as_str(), "rust/ripgrep.assets");
        assert_eq!(VaultPath::new("x.md").unwrap().parent(), "");
        assert_eq!(VaultPath::new("img.png").unwrap().assets_dir(), None);
    }

    #[test]
    fn open_enumerate_read_write() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("rust/ripgrep.assets")).unwrap();
        fs::create_dir_all(root.join("build")).unwrap();
        fs::write(root.join("rust/ripgrep.md"), "# rg\n").unwrap();
        fs::write(root.join("rust/ripgrep.assets/note.md"), "not a note").unwrap();
        fs::write(root.join("daily.md"), "hi\n").unwrap();
        fs::write(root.join("build/gen.md"), "ignored\n").unwrap();
        fs::write(root.join(".gitignore"), "build/\n").unwrap();
        fs::write(root.join("image.png"), b"\x89PNG").unwrap();

        let v = Vault::open(root).unwrap();
        assert_eq!(v.id().len(), 26);
        assert!(root.join(".aero/vault.toml").is_file());
        // 再次打开读到同一个 id
        assert_eq!(Vault::open(root).unwrap().id(), v.id());

        let notes = v.notes().unwrap();
        assert_eq!(notes.iter().map(|p| p.as_str()).collect::<Vec<_>>(), ["daily.md", "rust/ripgrep.md"]);

        let p = VaultPath::new("rust/ripgrep.md").unwrap();
        let (doc, stamp) = v.read(&p).unwrap();
        assert_eq!(doc.to_string(), "# rg\n");
        assert_eq!(stamp.size, 5);

        let new = Document::from_text("# rg\n\n新内容\n");
        let stamp2 = v.write(&p, &new).unwrap();
        assert_ne!(stamp2.hash, stamp.hash);
        assert_eq!(fs::read_to_string(root.join("rust/ripgrep.md")).unwrap(), "# rg\n\n新内容\n");
        // 没有留下临时文件
        let leftovers: Vec<_> = fs::read_dir(root.join("rust"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("aero-tmp"))
            .collect();
        assert!(leftovers.is_empty());

        // 写到不存在的目录
        let q = VaultPath::new("new/dir/note.md").unwrap();
        v.write_bytes(&q, b"x\n").unwrap();
        assert!(v.notes().unwrap().contains(&q));
    }

    #[test]
    fn rename_moves_note_and_assets_but_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("a.assets")).unwrap();
        fs::write(root.join("a.md"), "A\n").unwrap();
        fs::write(root.join("a.assets/img.png"), b"png").unwrap();
        fs::write(root.join("taken.md"), "T\n").unwrap();
        let v = Vault::open(root).unwrap();
        let a = VaultPath::new("a.md").unwrap();

        // 目标已存在：拒绝，且什么都没动
        let taken = VaultPath::new("taken.md").unwrap();
        assert!(matches!(v.rename(&a, &taken), Err(VaultError::AlreadyExists(_))));
        assert!(root.join("a.md").is_file());

        // 移到子目录：目录自动创建，附件目录跟着走
        let b = VaultPath::new("sub/b.md").unwrap();
        v.rename(&a, &b).unwrap();
        assert!(!root.join("a.md").exists());
        assert!(!root.join("a.assets").exists());
        assert_eq!(fs::read_to_string(root.join("sub/b.md")).unwrap(), "A\n");
        assert!(root.join("sub/b.assets/img.png").is_file());
        assert!(v.exists(&b));
        assert!(!v.exists(&a));
    }

    #[test]
    fn folders_include_empty_directories_and_obey_ignore_rules() {
        let dir = tempfile::tempdir().unwrap();
        for folder in ["empty/nested", "with-note", "note.assets/child", ".hidden", "ignored/sub"] {
            fs::create_dir_all(dir.path().join(folder)).unwrap();
        }
        fs::write(dir.path().join("with-note/n.md"), b"body").unwrap();
        fs::write(dir.path().join(".gitignore"), b"ignored/\n").unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        assert_eq!(
            vault.folders().unwrap().iter().map(VaultPath::as_str).collect::<Vec<_>>(),
            ["empty", "empty/nested", "with-note"]
        );
    }

    #[test]
    fn rename_directory_keeps_empty_children_and_refuses_subtree_or_collision() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("folder/empty")).unwrap();
        fs::write(dir.path().join("folder/n.md"), b"note").unwrap();
        fs::create_dir(dir.path().join("taken")).unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let from = VaultPath::new("folder").unwrap();
        assert!(vault.rename(&from, &VaultPath::new("folder/child").unwrap()).is_err());
        assert!(vault.rename(&from, &VaultPath::new("taken").unwrap()).is_err());
        vault.rename(&from, &VaultPath::new("moved").unwrap()).unwrap();
        assert!(dir.path().join("moved/empty").is_dir());
        assert_eq!(fs::read(dir.path().join("moved/n.md")).unwrap(), b"note");
        assert!(!dir.path().join("folder").exists());
    }

    #[cfg(unix)]
    #[test]
    fn rename_refuses_symbolic_source_and_destination_ancestors() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("n.md"), b"outside").unwrap();
        fs::write(dir.path().join("n.md"), b"inside").unwrap();
        symlink(outside.path(), dir.path().join("escape")).unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        assert!(
            vault
                .rename(&VaultPath::new("escape/n.md").unwrap(), &VaultPath::new("moved.md").unwrap())
                .is_err()
        );
        assert!(
            vault
                .rename(&VaultPath::new("n.md").unwrap(), &VaultPath::new("escape/new.md").unwrap())
                .is_err()
        );
        assert_eq!(fs::read(outside.path().join("n.md")).unwrap(), b"outside");
        assert!(!outside.path().join("new.md").exists());
    }

    #[test]
    fn custom_ignore_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".aero")).unwrap();
        fs::write(root.join(".aero/ignore"), "drafts/\n").unwrap();
        fs::create_dir_all(root.join("drafts")).unwrap();
        fs::write(root.join("drafts/x.md"), "").unwrap();
        fs::write(root.join("y.md"), "").unwrap();
        let v = Vault::open(root).unwrap();
        assert_eq!(v.notes().unwrap().iter().map(|p| p.as_str()).collect::<Vec<_>>(), ["y.md"]);
    }

    #[test]
    fn cross_device_errors_are_recognized_and_trees_copy_without_overwrite() {
        assert!(super::crosses_devices(&std::io::Error::from_raw_os_error(18)));
        assert!(!super::crosses_devices(&std::io::Error::from_raw_os_error(17)));
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("a.md"), b"A").unwrap();
        fs::write(src.join("nested/b.md"), b"B").unwrap();
        super::copy_tree(&src, &dst).unwrap();
        assert_eq!(fs::read(dst.join("nested/b.md")).unwrap(), b"B");
        assert!(super::copy_tree(&src, &dst).is_err(), "目标已存在时不覆盖");
        assert_eq!(fs::read(src.join("a.md")).unwrap(), b"A");
    }
}
