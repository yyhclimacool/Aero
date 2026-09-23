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

    /// 单个文件的增删（导入、保存、外部改动），不为一次变化重走整库。
    /// 非附件后缀一律忽略；重复插入不会产生第二条。
    pub fn update(&mut self, path: &VaultPath, exists: bool) {
        let Some(kind) = Kind::for_path(path.as_str()) else { return };
        match (self.entries.binary_search_by(|e| e.path.cmp(path)), exists) {
            (Ok(_), true) => {}
            (Ok(at), false) => {
                self.entries.remove(at);
            }
            (Err(at), true) => self.entries.insert(at, Entry { path: path.clone(), kind }),
            (Err(_), false) => {}
        }
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

/// 附件引用报告（方案 5.4 / 10.2）：每个附件被引用几次，没人引用的就是孤儿。
///
/// 只数**实际生效**的引用：`![[附件]]` 嵌入、普通 Markdown 图片 / 链接、生效的引用式定义；
/// 代码、数学、转义里的写法不算（与改名事务共用同一套扫描）。按出现次数计，与反链一致。
#[derive(Debug, Default, Clone)]
pub struct References {
    counts: std::collections::BTreeMap<VaultPath, usize>,
}

impl References {
    pub fn from_catalog(catalog: &Catalog) -> Self {
        let counts = catalog.entries.iter().map(|entry| (entry.path.clone(), 0)).collect();
        Self { counts }
    }

    pub fn increment(&mut self, path: &VaultPath) {
        *self.counts.entry(path.clone()).or_insert(0) += 1;
    }

    /// 扫描整库：枚举附件 + 读每篇笔记的正文。会读盘，放后台跑。
    /// 个别笔记读不出来（权限、非 UTF-8）只跳过它，不让一篇坏文件毁掉整份报告。
    pub fn read(vault: &Vault) -> Result<References, VaultError> {
        let catalog = Catalog::read(vault)?;
        let mut counts: std::collections::BTreeMap<_, _> =
            catalog.entries.iter().map(|e| (e.path.clone(), 0)).collect();
        for note in vault.notes()? {
            let Ok((bytes, _)) = vault.read_bytes(&note) else { continue };
            let Ok(text) = String::from_utf8(bytes) else { continue };
            let embeds = links::scan_embeds(&text).into_iter().map(|e| e.target);
            let markdown = crate::markdown_links::link_targets(&text);
            for target in embeds.chain(markdown) {
                // 普通 Markdown 地址可能带锚点 / 查询，也可能写成 `../`
                let target = target.split(['#', '?']).next().unwrap_or(&target);
                let Some(target) = rebase(note.parent(), target) else { continue };
                if let Resolution::Resolved { path, .. } = catalog.resolve(&target, &note) {
                    *counts.entry(path).or_insert(0) += 1;
                }
            }
        }
        Ok(References { counts })
    }

    pub fn count(&self, path: &VaultPath) -> usize {
        self.counts.get(path).copied().unwrap_or(0)
    }

    /// 一次都没被引用的附件，按路径排序。
    pub fn orphans(&self) -> Vec<&VaultPath> {
        self.counts.iter().filter(|(_, n)| **n == 0).map(|(p, _)| p).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }
}

/// 一篇笔记正文里指向附件的目标（嵌入和普通链接），原样保留，解析留给索引查询。
pub fn referenced_targets(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for embed in links::scan_embeds(text) {
        let bare = embed.target.split(['#', '?']).next().unwrap_or("");
        if Kind::for_path(bare).is_some() {
            out.push(embed.target);
        }
    }
    for url in crate::markdown_links::link_targets(text) {
        let bare = url.split(['#', '?']).next().unwrap_or("");
        if Kind::for_path(bare).is_some() {
            out.push(url);
        }
    }
    out
}

/// 把指向 `from` 的附件链接改成 `to` 的文件名。文件本身要由调用方改名。
pub fn rewrite_references(
    vault: &Vault,
    from: &VaultPath,
    to: &VaultPath,
) -> Result<Vec<VaultPath>, VaultError> {
    let catalog = Catalog::read(vault)?;
    let new_name = to.file_name();
    let mut updated = Vec::new();
    for note in vault.notes()? {
        let (bytes, _) = vault.read_bytes(&note)?;
        let Ok(mut text) = String::from_utf8(bytes) else { continue };
        let mut edits = Vec::new();
        for embed in links::scan_embeds(&text) {
            if matches!(catalog.resolve(&embed.target, &note), Resolution::Resolved { ref path, .. } if path == from)
                && let Some(rel) = text[embed.range.clone()].find(&embed.target)
            {
                let start = embed.range.start + rel;
                edits.push(start..start + embed.target.len());
            }
        }
        // 先用一次解析看这篇有没有指向它的普通链接，没有就不做逐个链接的定位
        let points_here = |url: &String| {
            let bare = url.split(['#', '?']).next().unwrap_or("");
            matches!(catalog.resolve(bare, &note), Resolution::Resolved { ref path, .. } if path == from)
        };
        if crate::markdown_links::link_targets(&text).iter().any(points_here)
            && let Ok(destinations) = crate::markdown_links::destinations(&text)
        {
            for destination in destinations {
                let bare = destination.url.split(['#', '?']).next().unwrap_or("");
                if matches!(catalog.resolve(bare, &note), Resolution::Resolved { ref path, .. } if path == from)
                {
                    let url_at = text[destination.range.clone()]
                        .find(&destination.url)
                        .map(|rel| destination.range.start + rel);
                    if let Some(start) = url_at {
                        let split = destination.url.find(['#', '?']).unwrap_or(destination.url.len());
                        edits.push(start..start + split);
                    }
                }
            }
        }
        if edits.is_empty() {
            continue;
        }
        edits.sort_by_key(|range| std::cmp::Reverse(range.start));
        edits.dedup();
        for range in edits {
            text.replace_range(range, &new_name);
        }
        vault.write_bytes(&note, text.as_bytes())?;
        updated.push(note);
    }
    Ok(updated)
}

/// 把 `./` 与 `../` 归到 vault 内的路径；越过根目录就当解析不了。
/// 不带 `.` 段的目标原样返回，仍由 [`Catalog::resolve`] 按"本篇 `.assets/` → 同级 → 唯一文件名"解析。
fn rebase(dir: &str, target: &str) -> Option<String> {
    if !target.split('/').any(|seg| seg == "." || seg == "..") {
        return Some(target.to_owned());
    }
    let mut segments: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for segment in target.split('/') {
        match segment {
            "." | "" => {}
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    Some(segments.join("/"))
}

/// 为一次导入分配附件路径（方案 5.4：附件放在 `<note>.assets/`）。
/// 有原文件名就沿用（只取文件名、去掉危险字符），重名追加 `-2`、`-3`…；
/// 剪贴板位图没有名字，用 ULID 后 8 位。分配到的路径此刻在磁盘上一定不存在。
pub fn import_path(
    vault: &Vault,
    note: &VaultPath,
    name: Option<&str>,
    extension: &str,
) -> Option<VaultPath> {
    let dir = note.assets_dir()?;
    let extension = extension.trim_start_matches('.').to_ascii_lowercase();
    let extension: String = extension.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if extension.is_empty() {
        return None;
    }
    let stem = name.and_then(safe_stem).unwrap_or_else(|| {
        let ulid = ulid::Ulid::generate().to_string();
        ulid[ulid.len() - 8..].to_ascii_lowercase()
    });
    (1..100).find_map(|n| {
        let file = if n == 1 { format!("{stem}.{extension}") } else { format!("{stem}-{n}.{extension}") };
        let path = VaultPath::new(&format!("{dir}/{file}")).ok()?;
        // 用 symlink_metadata：符号链接占位也算占用，导入不会顺着它写到 vault 外
        std::fs::symlink_metadata(vault.fs_path(&path)).is_err().then_some(path)
    })
}

/// 取文件名主干：丢掉目录、扩展名与首尾的点 / 空白；只剩空串就当作没名字。
fn safe_stem(name: &str) -> Option<String> {
    let name = name.rsplit(['/', '\\']).next()?;
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let cleaned: String =
        stem.chars().map(|c| if c.is_control() || "/\\:|?*<>\"[]^#".contains(c) { '-' } else { c }).collect();
    let cleaned = cleaned.trim().trim_matches('.').trim().to_owned();
    (!cleaned.is_empty()).then_some(cleaned)
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

    /// 导入一张图、删掉一张图都只动一条记录，解析结果跟着变，不用重扫整库。
    #[test]
    fn attachment_catalog_updates_single_entries_and_ignores_other_files() {
        let source = path("notes/a.md");
        let mut c = catalog(&["notes/a.assets/old.png"]);
        c.update(&path("notes/a.assets/new.png"), true);
        c.update(&path("notes/a.assets/new.png"), true);
        c.update(&path("notes/a.md"), true);
        c.update(&path("notes/a.assets/note.txt"), true);
        assert_eq!(
            c.entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            ["notes/a.assets/new.png", "notes/a.assets/old.png"]
        );
        assert_eq!(
            c.resolve("new.png", &source),
            Resolution::Resolved { path: path("notes/a.assets/new.png"), fragment: None }
        );
        c.update(&path("notes/a.assets/old.png"), false);
        c.update(&path("notes/a.assets/missing.png"), false);
        assert_eq!(c.entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["notes/a.assets/new.png"]);
        assert_eq!(c.resolve("old.png", &source), Resolution::Unresolved);
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

    /// 引用计数与孤儿检测：嵌入、普通 Markdown 图片 / 链接、引用式定义都算，代码里的不算。
    #[test]
    fn attachment_references_count_real_uses_and_report_orphans() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        for file in [
            "notes/a.assets/used.png",
            "notes/a.assets/twice.png",
            "notes/a.assets/orphan.png",
            "assets/shared.png",
            "assets/never.pdf",
        ] {
            let full = dir.path().join(file);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, b"bytes").unwrap();
        }
        std::fs::create_dir_all(dir.path().join("notes")).unwrap();
        std::fs::write(
            dir.path().join("notes/a.md"),
            // 嵌入、带宽度参数的嵌入、普通图片、引用式定义、相对路径，以及代码块里的假引用
            "![[used.png]]\n\n![[twice.png|200]]\n\n![图](twice.png)\n\n![共享](../assets/shared.png)\n\n\
             [def]: <used.png> \"标题\"\n\n见 [def]\n\n```\n![[orphan.png]]\n```\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("notes/b.md"), "别处引用 ![[shared.png]]\n").unwrap();

        let report = References::read(&vault).unwrap();
        let count = |p: &str| report.count(&path(p));
        assert_eq!(count("notes/a.assets/used.png"), 2, "嵌入 + 生效的引用式定义");
        assert_eq!(count("notes/a.assets/twice.png"), 2, "嵌入 + 普通图片");
        assert_eq!(count("assets/shared.png"), 2, "相对路径 + 别处的唯一文件名");
        assert_eq!(count("notes/a.assets/orphan.png"), 0, "代码块里的写法不算引用");
        assert_eq!(
            report.orphans().iter().map(|p| p.as_str()).collect::<Vec<_>>(),
            ["assets/never.pdf", "notes/a.assets/orphan.png"]
        );
        assert_eq!(report.count(&path("notes/a.assets/不存在.png")), 0);
        assert!(!report.is_empty());
    }

    #[test]
    fn relative_targets_are_rebased_and_cannot_escape_the_vault() {
        assert_eq!(rebase("notes", "../assets/x.png").as_deref(), Some("assets/x.png"));
        assert_eq!(rebase("notes/deep", "./x.png").as_deref(), Some("notes/deep/x.png"));
        assert_eq!(rebase("", "x.png").as_deref(), Some("x.png"), "没有 . 段就原样交给解析器");
        assert_eq!(rebase("notes", "../../outside.png"), None);
    }

    /// 导入命名：沿用原名、重名不覆盖、无名用 ULID、危险字符不出 `.assets/`。
    #[test]
    fn attachment_import_paths_keep_names_and_never_collide() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let note = path("notes/a.md");
        let import = |name: Option<&str>, ext: &str| import_path(&vault, &note, name, ext).unwrap();
        assert_eq!(import(Some("photo.JPG"), "jpg").as_str(), "notes/a.assets/photo.jpg");
        // 原名带路径 / 危险字符：只取文件名，分隔符与 wikilink 会歧义的字符替换掉
        assert_eq!(import(Some("/tmp/sub/图 1.png"), ".PNG").as_str(), "notes/a.assets/图 1.png");
        assert_eq!(import(Some("a[b]|c.png"), "png").as_str(), "notes/a.assets/a-b--c.png");
        // 重名不覆盖
        std::fs::create_dir_all(dir.path().join("notes/a.assets")).unwrap();
        std::fs::write(dir.path().join("notes/a.assets/photo.jpg"), b"existing").unwrap();
        assert_eq!(import(Some("photo.jpg"), "jpg").as_str(), "notes/a.assets/photo-2.jpg");
        // 没有名字（剪贴板位图）：ULID 后 8 位
        let generated = import(None, "png");
        let name = generated.file_name();
        assert!(name.ends_with(".png") && name.len() == 12, "{name}");
        assert_ne!(import(None, "png").as_str(), generated.as_str(), "两次导入不同名");
        // 扩展名不合法、以及不能有附件目录的路径
        assert!(import_path(&vault, &note, None, "").is_none());
        assert!(import_path(&vault, &path("notes"), None, "png").is_none());
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
