//! 嵌入补全的附件目录与路径解析；只枚举图片 / PDF，不读取内容，不进入全文索引。

use crate::links::{self, Resolution};
use crate::{Vault, VaultError, VaultPath};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Image,
    Pdf,
}

impl Kind {
    pub fn for_path(path: &str) -> Option<Self> {
        match path.rsplit_once('.')?.1.to_ascii_lowercase().as_str() {
            "pdf" => Some(Self::Pdf),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "tif" | "tiff" | "avif" | "heic"
            | "heif" | "ico" => Some(Self::Image),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: VaultPath,
    pub kind: Kind,
}

#[derive(Default)]
pub struct Catalog {
    pub entries: Vec<Entry>,
}

impl Catalog {
    pub fn new(paths: Vec<VaultPath>) -> Self {
        let mut entries: Vec<_> = paths
            .into_iter()
            .filter_map(|path| Kind::for_path(path.as_str()).map(|kind| Entry { path, kind }))
            .collect();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        entries.dedup_by(|a, b| a.path == b.path);
        Self { entries }
    }

    /// 与笔记枚举相同的忽略规则，但进入 `.assets`；不跟随符号链接。
    pub fn read(vault: &Vault) -> Result<Self, VaultError> {
        let mut walk = ignore::WalkBuilder::new(vault.root());
        walk.hidden(true).git_ignore(true).require_git(false).follow_links(false);
        let custom = vault.root().join(".aero/ignore");
        if custom.is_file()
            && let Some(err) = walk.add_ignore(custom)
        {
            return Err(VaultError::Walk(err.to_string()));
        }
        let mut paths = Vec::new();
        for entry in walk.build() {
            let entry = entry.map_err(|e| VaultError::Walk(e.to_string()))?;
            if entry.file_type().is_some_and(|t| t.is_file())
                && entry.path().to_str().and_then(Kind::for_path).is_some()
            {
                paths.push(VaultPath::from_fs(vault.root(), entry.path())?);
            }
        }
        Ok(Self::new(paths))
    }

    /// 本篇附件目录 → 笔记相对路径 → 显式 vault 路径 → 唯一文件名。
    /// 限定路径不存在时不退化为另一个同名文件；拒绝绝对路径与 `..`。
    pub fn resolve(&self, target: &str, source: &VaultPath) -> Resolution {
        let Ok(target) = VaultPath::new(target.trim()) else { return Resolution::Unresolved };
        if Kind::for_path(target.as_str()).is_none() {
            return Resolution::Unresolved;
        }
        let mut paths = Vec::new();
        if let Some(assets) = source.assets_dir() {
            paths.push(format!("{assets}/{target}"));
        }
        paths.push(if source.parent().is_empty() {
            target.to_string()
        } else {
            format!("{}/{target}", source.parent())
        });
        if target.as_str().contains('/') {
            paths.push(target.to_string());
        }
        for path in paths {
            let found: Vec<_> = self
                .entries
                .iter()
                .filter(|e| e.path.as_str().to_lowercase() == path.to_lowercase())
                .map(|e| e.path.clone())
                .collect();
            if !found.is_empty() {
                return resolution(found);
            }
        }
        if target.as_str().contains('/') {
            return Resolution::Unresolved;
        }
        resolution(
            self.entries
                .iter()
                .filter(|e| e.path.file_name().to_lowercase() == target.as_str().to_lowercase())
                .map(|e| e.path.clone())
                .collect(),
        )
    }

    /// 优先短文件名；有冲突则限定路径，并以同一解析器验证目标。
    pub fn target(&self, index: usize, source: &VaultPath) -> Option<String> {
        let path = &self.entries.get(index)?.path;
        let prefix = format!("{}/", source.parent());
        let relative = path.as_str().strip_prefix(&prefix).unwrap_or(path.as_str());
        for candidate in [path.file_name(), relative, path.as_str()] {
            if matches!(self.resolve(candidate, source), Resolution::Resolved { path: p, .. } if p == *path)
                && let Some(escaped) = links::escape_target(candidate)
                && links::scan_embeds(&format!("![[{escaped}]]"))
                    .first()
                    .is_some_and(|s| s.target == candidate && s.fragment.is_none())
            {
                return Some(escaped);
            }
        }
        None
    }
}

fn resolution(paths: Vec<VaultPath>) -> Resolution {
    match paths.as_slice() {
        [] => Resolution::Unresolved,
        [path] => Resolution::Resolved { path: path.clone(), fragment: None },
        _ => Resolution::Ambiguous { candidates: paths },
    }
}

/// 确认前检查文件仍存在且位于 vault 内；不把符号链接或目录当成候选文件。
pub fn available(vault: &Vault, path: &VaultPath) -> bool {
    let file = vault.fs_path(path);
    std::fs::symlink_metadata(&file).is_ok_and(|m| m.file_type().is_file())
        && file.canonicalize().is_ok_and(|p| p.starts_with(vault.root()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> VaultPath {
        VaultPath::new(s).unwrap()
    }
    fn catalog(paths: &[&str]) -> Catalog {
        Catalog::new(paths.iter().map(|p| path(p)).collect())
    }

    #[test]
    fn attachment_resolution_prioritizes_owned_and_relative_files_then_unique_names() {
        let source = path("notes/a.md");
        let c = catalog(&[
            "notes/a.assets/photo.png",
            "notes/photo.png",
            "x/photo.png",
            "y/photo.png",
            "docs/paper.PDF",
        ]);
        let resolved = |s: &str| Resolution::Resolved { path: path(s), fragment: None };
        assert_eq!(c.resolve("photo.png", &source), resolved("notes/a.assets/photo.png"));
        assert_eq!(c.resolve("photo.png", &path("notes/b.md")), resolved("notes/photo.png"));
        assert_eq!(c.resolve("paper.pdf", &source), resolved("docs/paper.PDF"));
        assert_eq!(c.resolve("x/photo.png", &source), resolved("x/photo.png"));
        assert!(matches!(c.resolve("photo.png", &path("z/a.md")), Resolution::Ambiguous { .. }));
        for query in ["../x/photo.png", "/x/photo.png", "missing/photo.png", "x//photo.png", "paper.exe"] {
            assert_eq!(c.resolve(query, &source), Resolution::Unresolved, "{query}");
        }
    }

    #[test]
    fn attachment_targets_disambiguate_escape_and_refuse_shadowed_paths() {
        let source = path("notes/a.md");
        let c = catalog(&["notes/a.assets/photo.png", "x/photo.png", "images/C#.png"]);
        for (i, entry) in c.entries.iter().enumerate() {
            let target = c.target(i, &source).unwrap();
            let embedded = links::scan_embeds(&format!("![[{target}]]"));
            assert_eq!(embedded.len(), 1);
            assert!(embedded[0].fragment.is_none());
            assert_eq!(
                c.resolve(&embedded[0].target, &source),
                Resolution::Resolved { path: entry.path.clone(), fragment: None }
            );
        }
        assert_eq!(c.target(0, &source).as_deref(), Some(r"C\#.png"));
        let c = catalog(&["assets/photo.png", "notes/assets/photo.png"]);
        assert!(c.target(0, &source).is_none());
        assert_eq!(c.target(1, &source).as_deref(), Some("assets/photo.png"));
        assert!(catalog(&["a|b.png"]).target(0, &source).is_none());
    }

    #[test]
    fn attachment_catalog_reads_assets_but_obeys_hidden_and_ignore_rules() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        for file in [
            "note.assets/图.PNG",
            "assets/paper.pdf",
            "other/photo.webp",
            ".secret/a.png",
            "ignored.pdf",
            "private.jpg",
            "program.exe",
            "note.md",
        ] {
            let full = dir.path().join(file);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, b"not decoded by the catalog").unwrap();
        }
        std::fs::write(dir.path().join(".gitignore"), "ignored.pdf\n").unwrap();
        std::fs::write(dir.path().join(".aero/ignore"), "private.jpg\n").unwrap();
        let c = Catalog::read(&vault).unwrap();
        assert_eq!(
            c.entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            ["assets/paper.pdf", "note.assets/图.PNG", "other/photo.webp"]
        );
        assert_eq!(c.entries[0].kind, Kind::Pdf);
        assert_eq!(c.entries[1].kind, Kind::Image);
        assert!(available(&vault, &c.entries[0].path));
        std::fs::remove_file(vault.fs_path(&c.entries[0].path)).unwrap();
        assert!(!available(&vault, &c.entries[0].path));
        assert!(!available(&vault, &path("note.assets")));
    }

    #[cfg(unix)]
    #[test]
    fn attachment_catalog_does_not_follow_symlink_files_or_directories() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("image.png"), b"image").unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        symlink(outside.path(), dir.path().join("linked.assets")).unwrap();
        symlink(outside.path().join("image.png"), dir.path().join("linked.png")).unwrap();
        assert!(Catalog::read(&vault).unwrap().entries.is_empty());
        assert!(!available(&vault, &path("linked.png")));
        assert!(!available(&vault, &path("linked.assets/image.png")));
    }
}
