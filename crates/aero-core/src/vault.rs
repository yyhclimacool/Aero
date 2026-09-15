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
            atomic_write(&cfg_path, body.as_bytes())?;
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
        let path = self.fs_path(p);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(io_err(dir))?;
        }
        atomic_write(&path, bytes)?;
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
    /// 链接的同步更新是 M2 的事（方案 11 节），这里只动文件。
    pub fn rename(&self, from: &VaultPath, to: &VaultPath) -> Result<(), VaultError> {
        if from == to {
            return Ok(());
        }
        let (src, dst) = (self.fs_path(from), self.fs_path(to));
        if dst.exists() {
            return Err(VaultError::AlreadyExists(to.as_str().to_owned()));
        }
        if let Some(dir) = dst.parent() {
            fs::create_dir_all(dir).map_err(io_err(dir))?;
        }
        fs::rename(&src, &dst).map_err(io_err(&src))?;
        if let (Some(a), Some(b)) = (from.assets_dir(), to.assets_dir()) {
            let (a, b) = (self.fs_path(&a), self.fs_path(&b));
            if a.is_dir() && !b.exists() {
                fs::rename(&a, &b).map_err(io_err(&a))?;
            }
        }
        Ok(())
    }

    /// 笔记是否已存在（用于重命名前的校验与生成不冲突的名字）。
    pub fn exists(&self, p: &VaultPath) -> bool {
        self.fs_path(p).exists()
    }
}

/// 写临时文件、fsync、rename 覆盖。目标已存在时沿用其权限位。
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), VaultError> {
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
        fs::rename(&tmp, path).map_err(io_err(path))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
