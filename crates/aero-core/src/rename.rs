//! 笔记重命名：从磁盘准备改写计划，逐文件校验后提交，普通失败回滚并保留恢复副本。
//! 不依赖可重建索引；即使索引尚未完成也不会漏改链接。多文件操作不是崩溃原子事务。

use std::path::PathBuf;

use crate::links::{self, Resolution, Resolver};
use crate::{Document, FileStamp, Vault, VaultError, VaultPath};

#[derive(Debug, thiserror::Error)]
pub enum RenameError {
    #[error(transparent)]
    Vault(#[from] VaultError),
    #[error("{0}")]
    Invalid(String),
    #[error("文件在准备重命名后发生变化，请重试：{0}")]
    Changed(String),
    #[error("重命名失败：{cause}。恢复副本：{recovery}。回滚结果：{rollback}")]
    Commit { cause: String, recovery: PathBuf, rollback: String },
}

struct Change {
    path: VaultPath,
    before: Vec<u8>,
    after: Vec<u8>,
}

pub struct RenamePlan {
    vault: Vault,
    from: VaultPath,
    to: VaultPath,
    observed: Vec<(VaultPath, FileStamp)>,
    changes: Vec<Change>,
    links_changed: usize,
}

pub struct RenameReport {
    pub from: VaultPath,
    pub to: VaultPath,
    /// 已修改链接文本的笔记（含重命名后的自身路径）。
    pub updated: Vec<VaultPath>,
    pub links_changed: usize,
}

impl RenamePlan {
    /// 当前入口仅重命名同一目录下的 `.md` 笔记；跨目录移动需另处理所有出链及附件引用。
    pub fn prepare(vault: &Vault, from: VaultPath, to: VaultPath) -> Result<Self, RenameError> {
        if from == to || from.parent() != to.parent() || !from.is_note() || !to.is_note() {
            return Err(RenameError::Invalid("请选择同一目录下不同的 Markdown 文件名".into()));
        }
        vault.validate_rename(&from, &to)?;
        let paths = vault.notes()?;
        if !paths.contains(&from) {
            return Err(RenameError::Invalid("被忽略的笔记不能参与全库链接更新".into()));
        }
        let mut before = Resolver::new();
        let mut observed = Vec::with_capacity(paths.len());
        let mut title = None;
        let mut aliases = Vec::new();
        // 先建完整解析表，避免扫描顺序把同名笔记误认成唯一目标。
        for path in paths {
            let (bytes, stamp) = vault.read_bytes(&path)?;
            let doc =
                Document::from_bytes(&bytes).map_err(|_| VaultError::Utf8 { path: vault.fs_path(&path) })?;
            let note_title = doc.front_matter_title().map(|(_, title)| title);
            let note_aliases = doc.front_matter_aliases();
            before.insert(path.clone(), note_title.as_deref(), &note_aliases);
            if path == from {
                title = note_title;
                aliases = note_aliases;
            }
            observed.push((path, stamp));
        }
        let mut after = before.clone();
        after.remove(&from);
        after.insert(to.clone(), title.as_deref(), &aliases);
        let mut changes = Vec::new();
        let mut links_changed = 0;
        for (path, stamp) in &observed {
            let (bytes, current) = vault.read_bytes(path)?;
            if current.hash != stamp.hash {
                return Err(RenameError::Changed(path.to_string()));
            }
            let (rewritten, count) = rewrite(&bytes, path, &from, &to, &before, &after)?;
            if count > 0 || path == &from {
                changes.push(Change { path: path.clone(), before: bytes, after: rewritten });
                links_changed += count;
            }
        }
        Ok(Self { vault: vault.clone(), from, to, observed, changes, links_changed })
    }

    pub fn other_files_changed(&self) -> usize {
        self.changes.iter().filter(|c| c.path != self.from).count()
    }

    pub fn execute(self) -> Result<RenameReport, RenameError> {
        self.execute_with(|_| Ok(()))
    }

    fn validate(&self) -> Result<(), RenameError> {
        self.vault.validate_rename(&self.from, &self.to)?;
        let paths: Vec<_> = self.observed.iter().map(|(p, _)| p.clone()).collect();
        if self.vault.notes()? != paths {
            return Err(RenameError::Changed("笔记列表".into()));
        }
        // 名字或标题可能改变解析结果；未改写笔记也检查元数据。
        for (path, stamp) in &self.observed {
            if self.vault.stat(path)? != (stamp.mtime, stamp.size) {
                return Err(RenameError::Changed(path.to_string()));
            }
        }
        for change in &self.changes {
            if self.vault.read_bytes(&change.path)?.0 != change.before {
                return Err(RenameError::Changed(change.path.to_string()));
            }
        }
        Ok(())
    }

    fn actual_path(&self, change: &Change) -> VaultPath {
        if change.path == self.from { self.to.clone() } else { change.path.clone() }
    }

    fn execute_with(
        self,
        mut before_write: impl FnMut(&VaultPath) -> Result<(), RenameError>,
    ) -> Result<RenameReport, RenameError> {
        self.validate()?;
        // 所有原件先落盘：普通 I/O 错误可回滚，进程中止后也能按 manifest 手工恢复。
        let recovery = format!(".aero/rename-{}", ulid::Ulid::generate());
        let recovery_path = self.vault.fs_path(&VaultPath::new(&recovery)?);
        let mut directory = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            directory.mode(0o700);
        }
        directory.create(&recovery_path).map_err(|source| VaultError::Io { path: recovery_path, source })?;
        #[derive(serde::Serialize)]
        struct Manifest<'a> {
            from: &'a str,
            to: &'a str,
            files: Vec<RecoveryFile<'a>>,
        }
        #[derive(serde::Serialize)]
        struct RecoveryFile<'a> {
            original: &'a str,
            backup: String,
        }
        let mut files = Vec::new();
        for (i, change) in self.changes.iter().enumerate() {
            self.vault.create_bytes(&VaultPath::new(&format!("{recovery}/{i}.md"))?, &change.before)?;
            files.push(RecoveryFile { original: change.path.as_str(), backup: format!("{i}.md") });
        }
        let manifest = toml::to_string(&Manifest { from: self.from.as_str(), to: self.to.as_str(), files })
            .map_err(|e| RenameError::Invalid(e.to_string()))?;
        self.vault
            .create_bytes(&VaultPath::new(&format!("{recovery}/manifest.toml"))?, manifest.as_bytes())?;
        let recovery = self.vault.fs_path(&VaultPath::new(&recovery)?);
        // 备份期间仍可能被外部程序改动，再校验一次才开始修改。
        self.validate()?;
        let mut renamed = false;
        let mut written = Vec::new();
        let result = (|| {
            self.vault.rename(&self.from, &self.to)?;
            renamed = true;
            for (i, change) in self.changes.iter().enumerate() {
                if change.before == change.after {
                    continue;
                }
                let path = self.actual_path(change);
                before_write(&path)?;
                if self.vault.read_bytes(&path)?.0 != change.before {
                    return Err(RenameError::Changed(path.to_string()));
                }
                // 即使写后 stat 失败，文件也可能已经发布；回滚要涵盖本次尝试。
                written.push(i);
                self.vault.write_bytes(&path, &change.after)?;
            }
            Ok::<_, RenameError>(())
        })();
        if let Err(error) = result {
            let mut failures = Vec::new();
            if !renamed && self.vault.exists(&self.to) {
                failures.push("新路径仍然存在，请检查新旧笔记及附件目录".into());
            }
            for &i in written.iter().rev() {
                let change = &self.changes[i];
                let path = self.actual_path(change);
                let restored = (|| {
                    let (bytes, _) = self.vault.read_bytes(&path)?;
                    if bytes == change.before {
                        return Ok(());
                    }
                    if bytes != change.after {
                        return Err(RenameError::Changed(path.to_string()));
                    }
                    self.vault.write_bytes(&path, &change.before)?;
                    Ok::<_, RenameError>(())
                })();
                if let Err(e) = restored {
                    failures.push(e.to_string());
                }
            }
            if renamed && let Err(e) = self.vault.rename(&self.to, &self.from) {
                failures.push(e.to_string());
            }
            return Err(RenameError::Commit {
                cause: error.to_string(),
                recovery,
                rollback: if failures.is_empty() {
                    "已撤回本次改动".into()
                } else {
                    failures.join("；")
                },
            });
        }
        let updated =
            self.changes.iter().filter(|c| c.before != c.after).map(|c| self.actual_path(c)).collect();
        let _ = std::fs::remove_dir_all(recovery);
        Ok(RenameReport { from: self.from, to: self.to, updated, links_changed: self.links_changed })
    }
}

fn resolves_to(resolver: &Resolver, target: &str, dir: &str, path: &VaultPath) -> bool {
    matches!(resolver.resolve(target, None, dir), Resolution::Resolved { path: p, .. } if &p == path)
}

fn replacement(
    target: &str,
    dir: &str,
    to: &VaultPath,
    after: &Resolver,
    embed: bool,
) -> Result<String, RenameError> {
    let extension = target.to_ascii_lowercase().ends_with(".md");
    let name = if extension { to.file_name() } else { to.stem() };
    let qualified = if extension { to.as_str() } else { to.as_str().strip_suffix(".md").unwrap() };
    let preferred =
        target.rsplit_once('/').map_or_else(|| name.to_owned(), |(prefix, _)| format!("{prefix}/{name}"));
    for candidate in [preferred, name.to_owned(), qualified.to_owned(), to.to_string()] {
        if !(embed && crate::attachments::Kind::for_path(&candidate).is_some())
            && resolves_to(after, &candidate, dir, to)
            && let Some(escaped) = links::escape_target(&candidate)
        {
            // 使用同一解析器确认新名字可由 wikilink 表达（含转义标点）。
            let probe = links::scan(&format!("[[{escaped}]]"));
            if probe.len() == 1 && probe[0].target == candidate {
                return Ok(escaped);
            }
        }
    }
    Err(RenameError::Invalid(format!("无法从 {dir}/ 写出不歧义的链接到 {to}，请换一个文件名")))
}

fn rewrite(
    bytes: &[u8],
    source: &VaultPath,
    from: &VaultPath,
    to: &VaultPath,
    before: &Resolver,
    after: &Resolver,
) -> Result<(Vec<u8>, usize), RenameError> {
    // 直接在原始字节上补丁，保留 BOM、混合换行、末尾换行和未知语法。
    let bom = if bytes.starts_with(b"\xEF\xBB\xBF") { 3 } else { 0 };
    let text = std::str::from_utf8(&bytes[bom..])
        .map_err(|_| RenameError::Invalid(format!("{source}: 非 UTF-8")))?;
    let mut edits = Vec::new();
    for span in links::scan_for_rename(text) {
        let embed = span.range.start > 0 && text.as_bytes()[span.range.start - 1] == b'!';
        // 图片 / PDF 后缀属于附件命名空间；同名笔记必须带 `.md`。
        if (embed && crate::attachments::Kind::for_path(&span.target).is_some())
            || !resolves_to(before, &span.target, source.parent(), from)
            || resolves_to(after, &span.target, source.parent(), to)
        {
            continue;
        }
        let range = links::target_range(text, &span);
        edits.push((
            bom + range.start..bom + range.end,
            replacement(&span.target, source.parent(), to, after, embed)?,
        ));
    }
    let count = edits.len();
    let mut out = bytes.to_vec();
    for (range, replacement) in edits.into_iter().rev() {
        out.splice(range, replacement.bytes());
    }
    Ok((out, count))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> VaultPath {
        VaultPath::new(s).unwrap()
    }

    fn setup(files: &[(&str, &str)]) -> (tempfile::TempDir, Vault) {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            let p = dir.path().join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        let vault = Vault::open(dir.path()).unwrap();
        (dir, vault)
    }

    #[test]
    fn rename_rewrites_only_resolved_targets_and_preserves_original_bytes() {
        let source = "\u{feff}---\r\ncustom: '留着'\r\n---\r\n\r\n中文 [[ old #标题 | 别名 ]]、[[old.md#^块]]\r\n\n| 列 |\r\n| --- |\r\n| [[old\\|表格别名]] |\r\n\r\n![[old#标题]]\n`[[old]]` \\[[old]] [[missing]] [[#标题]]\n\n```\n[[old]]\n```\n\n$$\n[[old]]\n$$\n";
        let (dir, vault) = setup(&[
            ("notes/old.md", "# 内容\n\n自己 [[old]]\n"),
            ("notes/source.md", source),
            ("else/source.md", "[[notes/old]] [[notes/old.md|alias]]\n"),
            ("else/old.md", "另一个旧名字\n"),
            ("else/local.md", "[[old]]\n"),
            ("else/new.md", "另一个新名字\n"),
            ("third/source.md", "[[old]] 歧义不要改\n"),
        ]);
        std::fs::create_dir(dir.path().join("notes/old.assets")).unwrap();
        std::fs::write(dir.path().join("notes/old.assets/image.png"), b"image").unwrap();
        let plan = RenamePlan::prepare(&vault, path("notes/old.md"), path("notes/new.md")).unwrap();
        assert_eq!(plan.other_files_changed(), 2);
        let report = plan.execute().unwrap();
        assert_eq!(report.updated.len(), 3);
        let expected = source
            .replace("[[ old #", "[[ new #")
            .replace("[[old.md#", "[[new.md#")
            .replace("[[old\\|", "[[new\\|")
            .replace("![[old#", "![[new#");
        assert_eq!(std::fs::read(dir.path().join("notes/source.md")).unwrap(), expected.as_bytes());
        assert_eq!(report.links_changed, 7);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("else/source.md")).unwrap(),
            "[[notes/new]] [[notes/new.md|alias]]\n"
        );
        assert_eq!(std::fs::read_to_string(dir.path().join("else/local.md")).unwrap(), "[[old]]\n");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("third/source.md")).unwrap(),
            "[[old]] 歧义不要改\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("notes/new.md")).unwrap(),
            "# 内容\n\n自己 [[new]]\n"
        );
        assert!(!vault.exists(&path("notes/old.md")));
        assert_eq!(std::fs::read(dir.path().join("notes/new.assets/image.png")).unwrap(), b"image");
        assert!(!dir.path().join("notes/old.assets").exists());
    }

    #[test]
    fn rename_preserves_attachment_embeds_and_disambiguates_note_extensions() {
        for extension in ["png", "PDF"] {
            let source = format!(
                "![[photo.{extension}|400]] ![[photo.{extension}.md#标题]] [[photo.{extension}]] ![[old#标题]]\n"
            );
            let original = format!("photo.{extension}.md");
            let renamed = format!("new.{extension}.md");
            let (dir, vault) = setup(&[(original.as_str(), "# 标题\n"), ("source.md", &source)]);
            std::fs::write(dir.path().join(format!("photo.{extension}")), b"attachment").unwrap();
            let report =
                RenamePlan::prepare(&vault, path(&original), path("old.md")).unwrap().execute().unwrap();
            assert_eq!(report.links_changed, 2);
            assert_eq!(
                std::fs::read_to_string(dir.path().join("source.md")).unwrap(),
                format!("![[photo.{extension}|400]] ![[old.md#标题]] [[old]] ![[old#标题]]\n")
            );
            RenamePlan::prepare(&vault, path("old.md"), path(&renamed)).unwrap().execute().unwrap();
            assert_eq!(
                std::fs::read_to_string(dir.path().join("source.md")).unwrap(),
                format!(
                    "![[photo.{extension}|400]] ![[{renamed}#标题]] [[new.{extension}]] ![[{renamed}#标题]]\n"
                )
            );
            assert_eq!(std::fs::read(dir.path().join(format!("photo.{extension}"))).unwrap(), b"attachment");
        }
    }

    #[test]
    fn rename_preserves_front_matter_aliases_and_ignores_body_title() {
        let original = "\u{feff}---\r\naliases: [old, '雪 山']\r\ncustom: untouched\r\n---\r\n\r\nbody\r\n";
        let source = "\u{feff}[[old]] [[雪 山]] [[old.md]]\r\n";
        let (dir, vault) = setup(&[("old.md", original), ("source.md", source)]);
        let report = RenamePlan::prepare(&vault, path("old.md"), path("new.md")).unwrap().execute().unwrap();
        assert_eq!(report.links_changed, 1);
        assert_eq!(
            std::fs::read(dir.path().join("source.md")).unwrap(),
            source.replace("[[old.md]]", "[[new.md]]").as_bytes()
        );
        assert_eq!(std::fs::read(dir.path().join("new.md")).unwrap(), original.as_bytes());

        let (dir, vault) = setup(&[("old.md", "body\n\ntitle: old\n"), ("source.md", "[[old]]\n")]);
        RenamePlan::prepare(&vault, path("old.md"), path("new.md")).unwrap().execute().unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("source.md")).unwrap(), "[[new]]\n");
    }

    #[test]
    fn rename_preserves_title_links_and_qualifies_new_name_collisions() {
        let (dir, vault) = setup(&[
            ("notes/old.md", "---\ntitle: 稳定标题\n---\n正文\n"),
            ("else/new.md", "另一个新名字\n"),
            ("else/source.md", "[[old]] [[稳定标题]]\n"),
        ]);
        RenamePlan::prepare(&vault, path("notes/old.md"), path("notes/new.md")).unwrap().execute().unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("else/source.md")).unwrap(),
            "[[notes/new]] [[稳定标题]]\n"
        );
    }

    #[test]
    fn rename_escapes_new_target_and_rejects_unrepresentable_names() {
        let (dir, vault) = setup(&[("old.md", "正文\n"), ("source.md", "[[old#标题|别名]]\n")]);
        assert!(RenamePlan::prepare(&vault, path("old.md"), path("a|b.md")).is_err());
        assert!(vault.exists(&path("old.md")));
        RenamePlan::prepare(&vault, path("old.md"), path("新#名字.md")).unwrap().execute().unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("source.md")).unwrap(),
            "[[新\\#名字#标题|别名]]\n"
        );
    }

    #[test]
    fn rename_aborts_on_stale_sources_namespace_and_destination() {
        let (dir, vault) = setup(&[("old.md", "正文\n"), ("source.md", "[[old]]\n")]);
        let plan = RenamePlan::prepare(&vault, path("old.md"), path("new.md")).unwrap();
        let stamp = std::fs::metadata(dir.path().join("source.md")).unwrap().modified().unwrap();
        std::fs::write(dir.path().join("source.md"), "[[外]]\n").unwrap(); // 同字节长度
        std::fs::File::open(dir.path().join("source.md")).unwrap().set_modified(stamp).unwrap();
        assert!(matches!(plan.execute(), Err(RenameError::Changed(_))));
        assert!(vault.exists(&path("old.md")));
        assert!(!vault.exists(&path("new.md")));
        assert_eq!(std::fs::read_to_string(dir.path().join("source.md")).unwrap(), "[[外]]\n");

        let plan = RenamePlan::prepare(&vault, path("old.md"), path("new.md")).unwrap();
        std::fs::write(dir.path().join("added.md"), "新文件\n").unwrap();
        assert!(matches!(plan.execute(), Err(RenameError::Changed(_))));
        let plan = RenamePlan::prepare(&vault, path("old.md"), path("new.md")).unwrap();
        std::fs::write(dir.path().join("new.md"), "不能覆盖\n").unwrap();
        assert!(plan.execute().is_err());
        assert_eq!(std::fs::read_to_string(dir.path().join("new.md")).unwrap(), "不能覆盖\n");
    }

    #[test]
    fn rename_write_failure_rolls_back_note_assets_and_already_written_links() {
        let (dir, vault) = setup(&[("a.md", "[[a]]\n"), ("b.md", "[[a|B]]\n"), ("c.md", "[[a#C]]\n")]);
        std::fs::create_dir(dir.path().join("a.assets")).unwrap();
        std::fs::write(dir.path().join("a.assets/item"), b"asset").unwrap();
        let plan = RenamePlan::prepare(&vault, path("a.md"), path("z.md")).unwrap();
        let result = plan.execute_with(|p| {
            if p.as_str() == "c.md" {
                return Err(RenameError::Invalid("模拟写盘失败".into()));
            }
            Ok(())
        });
        let Err(RenameError::Commit { recovery, rollback, .. }) = result else { panic!("expected failure") };
        assert_eq!(rollback, "已撤回本次改动");
        assert!(recovery.join("manifest.toml").is_file());
        assert_eq!(std::fs::read(recovery.join("0.md")).unwrap(), b"[[a]]\n");
        assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), "[[a]]\n");
        assert_eq!(std::fs::read_to_string(dir.path().join("b.md")).unwrap(), "[[a|B]]\n");
        assert_eq!(std::fs::read_to_string(dir.path().join("c.md")).unwrap(), "[[a#C]]\n");
        assert!(!vault.exists(&path("z.md")));
        assert_eq!(std::fs::read(dir.path().join("a.assets/item")).unwrap(), b"asset");
    }

    #[test]
    fn rename_rollback_never_overwrites_new_external_content() {
        let (dir, vault) = setup(&[("a.md", "正文\n"), ("b.md", "[[a]]\n"), ("c.md", "[[a]]\n")]);
        let plan = RenamePlan::prepare(&vault, path("a.md"), path("z.md")).unwrap();
        let result = plan.execute_with(|p| {
            if p.as_str() == "c.md" {
                std::fs::write(dir.path().join("b.md"), "外部写入，不可覆盖\n").unwrap();
                return Err(RenameError::Invalid("模拟失败".into()));
            }
            Ok(())
        });
        let Err(RenameError::Commit { recovery, rollback, .. }) = result else { panic!("expected failure") };
        assert!(rollback.contains("b.md"));
        assert_eq!(std::fs::read_to_string(dir.path().join("b.md")).unwrap(), "外部写入，不可覆盖\n");
        assert_eq!(std::fs::read(recovery.join("1.md")).unwrap(), b"[[a]]\n");
        assert!(vault.exists(&path("a.md")));
    }

    #[test]
    fn rename_refuses_attachment_collision_before_moving_the_note() {
        let (dir, vault) = setup(&[("a.md", "正文\n")]);
        std::fs::create_dir(dir.path().join("a.assets")).unwrap();
        std::fs::create_dir(dir.path().join("b.assets")).unwrap();
        assert!(vault.rename(&path("a.md"), &path("b.md")).is_err());
        assert!(vault.exists(&path("a.md")));
        assert!(!vault.exists(&path("b.md")));
        assert!(dir.path().join("a.assets").exists());
    }
}
