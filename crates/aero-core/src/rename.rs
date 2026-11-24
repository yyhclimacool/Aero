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
    directory: bool,
    files: Vec<VaultPath>,
    folders: Vec<VaultPath>,
    observed: Vec<(VaultPath, FileStamp)>,
    changes: Vec<Change>,
    links_changed: usize,
    source_stamp: Option<FileStamp>,
}

pub struct RenameReport {
    pub from: VaultPath,
    pub to: VaultPath,
    pub directory: bool,
    /// 已修改链接文本的笔记（含重命名后的自身路径）。
    pub updated: Vec<VaultPath>,
    pub links_changed: usize,
}

impl RenamePlan {
    pub fn prepare_title(vault: &Vault, path: VaultPath, title: String) -> Result<Self, RenameError> {
        if title.trim().is_empty() || title.chars().any(char::is_control) {
            return Err(RenameError::Invalid("标题不能为空或包含控制字符".into()));
        }
        Self::prepare_inner(vault, path.clone(), path, Some(title.trim()), false)
    }

    pub fn prepare(vault: &Vault, from: VaultPath, to: VaultPath) -> Result<Self, RenameError> {
        Self::prepare_inner(vault, from, to, None, false)
    }

    /// Attachment moves use the same validation, recovery copies and rollback
    /// as note moves. Link destinations are resolved against both catalogs.
    pub fn prepare_attachment(vault: &Vault, from: VaultPath, to: VaultPath) -> Result<Self, RenameError> {
        if crate::attachments::Kind::for_path(from.as_str()).is_none()
            || crate::attachments::Kind::for_path(from.as_str())
                != crate::attachments::Kind::for_path(to.as_str())
        {
            return Err(RenameError::Invalid("请选择相同类型的附件路径".into()));
        }
        Self::prepare_inner(vault, from, to, None, true)
    }

    fn prepare_inner(
        vault: &Vault,
        from: VaultPath,
        to: VaultPath,
        new_title: Option<&str>,
        attachment: bool,
    ) -> Result<Self, RenameError> {
        if new_title.is_none() {
            vault.validate_rename(&from, &to)?;
        } else {
            vault.validate_local_path(&from)?;
        }
        let directory = vault.fs_path(&from).is_dir();
        if (from == to && new_title.is_none())
            || (directory && new_title.is_some())
            || (!attachment && !directory && (!from.is_note() || !to.is_note()))
            || to
                .as_str()
                .split('/')
                .any(|part| part.starts_with('.') || (!attachment && part.ends_with(".assets")))
        {
            return Err(RenameError::Invalid("请选择不同的笔记路径或文件夹，不能移入隐藏或附件目录".into()));
        }
        let paths = vault.notes()?;
        if !attachment && !directory && !paths.contains(&from) {
            return Err(RenameError::Invalid("被忽略的笔记不能参与全库链接更新".into()));
        }
        let source_stamp = if attachment { Some(vault.read_bytes(&from)?.1) } else { None };
        let files = file_catalog(vault)?;
        if attachment && !files.contains(&from) {
            return Err(RenameError::Invalid("被忽略的附件不能参与链接更新".into()));
        }
        let folders = vault.folders()?;
        if directory && !folders.contains(&from) {
            return Err(RenameError::Invalid("被忽略的目录不能移动".into()));
        }
        let mut before = Resolver::new();
        let mut after = Resolver::new();
        let mut observed = Vec::with_capacity(paths.len());
        for path in paths {
            let (bytes, stamp) = vault.read_bytes(&path)?;
            let doc =
                Document::from_bytes(&bytes).map_err(|_| VaultError::Utf8 { path: vault.fs_path(&path) })?;
            let title = doc.front_matter_title().map(|(_, title)| title);
            let aliases = doc.front_matter_aliases();
            before.insert(path.clone(), title.as_deref(), &aliases);
            let after_title = if path == from { new_title.or(title.as_deref()) } else { title.as_deref() };
            after.insert(relocated(&path, &from, &to, directory), after_title, &aliases);
            observed.push((path, stamp));
        }
        let after_files = files.iter().map(|p| relocated(p, &from, &to, directory)).collect::<Vec<_>>();
        let context = RewriteContext {
            from: &from,
            to: &to,
            directory,
            new_title,
            before: &before,
            after: &after,
            files: &files,
            after_files: &after_files,
            attachments: crate::attachments::Catalog::new(files.clone()),
            after_attachments: crate::attachments::Catalog::new(after_files.clone()),
        };
        let mut changes = Vec::new();
        let mut links_changed = 0;
        for (path, stamp) in &observed {
            let (bytes, current) = vault.read_bytes(path)?;
            if current.hash != stamp.hash {
                return Err(RenameError::Changed(path.to_string()));
            }
            let (mut rewritten, count) = context.rewrite(&bytes, path)?;
            if path == &from
                && let Some(title) = new_title
            {
                rewritten = retitled(&rewritten, title)?;
            }
            if count > 0 || relocated(path, &from, &to, directory) != *path || rewritten != bytes {
                changes.push(Change { path: path.clone(), before: bytes, after: rewritten });
                links_changed += count;
            }
        }
        Ok(Self {
            vault: vault.clone(),
            from,
            to,
            directory,
            files,
            folders,
            observed,
            changes,
            links_changed,
            source_stamp,
        })
    }

    pub fn other_files_changed(&self) -> usize {
        self.changes.iter().filter(|c| c.path != self.from && c.before != c.after).count()
    }

    pub fn execute(self) -> Result<RenameReport, RenameError> {
        self.execute_with(|_| Ok(()))
    }

    fn validate(&self) -> Result<(), RenameError> {
        if self.from != self.to {
            self.vault.validate_rename(&self.from, &self.to)?;
        } else {
            self.vault.validate_local_path(&self.from)?;
        }
        if let Some(stamp) = self.source_stamp
            && self.vault.read_bytes(&self.from)?.1.hash != stamp.hash
        {
            return Err(RenameError::Changed(self.from.to_string()));
        }
        let paths: Vec<_> = self.observed.iter().map(|(p, _)| p.clone()).collect();
        self.vault.validate_local_path(&VaultPath::new(".aero")?)?;
        if self.vault.notes()? != paths
            || file_catalog(&self.vault)? != self.files
            || self.vault.folders()? != self.folders
        {
            return Err(RenameError::Changed("笔记或附件列表".into()));
        }
        // 名字或标题可能改变解析结果；未改写笔记也检查元数据。
        for (path, stamp) in &self.observed {
            self.vault.validate_local_path(path)?;
            if self.vault.read_bytes(path)?.1.hash != stamp.hash {
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
        relocated(&change.path, &self.from, &self.to, self.directory)
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
            if self.from != self.to {
                self.vault.rename(&self.from, &self.to)?;
                renamed = true;
                let mut expected: Vec<_> = self
                    .observed
                    .iter()
                    .map(|(path, _)| relocated(path, &self.from, &self.to, self.directory))
                    .collect();
                expected.sort();
                let mut expected_files: Vec<_> = self
                    .files
                    .iter()
                    .map(|path| relocated(path, &self.from, &self.to, self.directory))
                    .collect();
                expected_files.sort();
                if self.vault.notes()? != expected
                    || file_catalog(&self.vault)? != expected_files
                    || (self.directory && !self.vault.folders()?.contains(&self.to))
                {
                    return Err(RenameError::Invalid(
                        "目标目录的忽略规则改变了笔记可见性，已停止移动".into(),
                    ));
                }
            }
            for (i, change) in self.changes.iter().enumerate() {
                if change.before == change.after {
                    continue;
                }
                let path = self.actual_path(change);
                before_write(&path)?;
                self.vault.validate_local_path(&path)?;
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
            if self.from != self.to && !renamed && self.vault.exists(&self.to) {
                failures.push("新路径仍然存在，请检查新旧笔记及附件目录".into());
            }
            for &i in written.iter().rev() {
                let change = &self.changes[i];
                let path = self.actual_path(change);
                let restored = (|| {
                    self.vault.validate_local_path(&path)?;
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
        Ok(RenameReport {
            from: self.from,
            to: self.to,
            directory: self.directory,
            updated,
            links_changed: self.links_changed,
        })
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
    let preferred = name.to_owned();
    let candidates = if target.contains('/') {
        [qualified.to_owned(), relative_path(dir, qualified), name.to_owned(), to.to_string()]
    } else {
        [preferred, name.to_owned(), qualified.to_owned(), to.to_string()]
    };
    for candidate in candidates {
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

fn retitled(bytes: &[u8], title: &str) -> Result<Vec<u8>, RenameError> {
    let doc = Document::from_bytes(bytes).map_err(|e| RenameError::Invalid(e.to_string()))?;
    if doc.front_matter_title().is_none() {
        return Err(RenameError::Invalid("笔记没有可编辑的 front matter title".into()));
    }
    let bom = if bytes.starts_with(b"\xEF\xBB\xBF") { 3 } else { 0 };
    let text = std::str::from_utf8(&bytes[bom..]).expect("document is UTF-8");
    let (mut range, _) = crate::document::front_matter_title(text).expect("title checked above");
    if range.start > 0 && matches!(text.as_bytes()[range.start - 1], b'"' | b'\'') {
        range.start -= 1;
        range.end += 1;
    }
    let escaped = format!("\"{}\"", title.replace('\\', "\\\\").replace('"', "\\\""));
    let mut result = bytes.to_vec();
    result.splice(bom + range.start..bom + range.end, escaped.bytes());
    if Document::from_bytes(&result).ok().and_then(|d| d.front_matter_title()).map(|(_, t)| t).as_deref()
        != Some(title)
    {
        return Err(RenameError::Invalid("无法无损写入标题".into()));
    }
    Ok(result)
}

fn relocated(path: &VaultPath, from: &VaultPath, to: &VaultPath, directory: bool) -> VaultPath {
    if path == from {
        return to.clone();
    }
    let roots =
        if directory { Some((from.clone(), to.clone())) } else { from.assets_dir().zip(to.assets_dir()) };
    if let Some((old, new)) = roots
        && let Some(rest) = path.as_str().strip_prefix(&format!("{old}/"))
    {
        return VaultPath::new(&format!("{new}/{rest}")).expect("mapped vault path");
    }
    path.clone()
}

pub(crate) fn file_catalog(vault: &Vault) -> Result<Vec<VaultPath>, RenameError> {
    let mut walk = ignore::WalkBuilder::new(vault.root());
    walk.hidden(true).git_ignore(true).require_git(false).follow_links(false);
    let ignore = vault.root().join(".aero/ignore");
    if ignore.is_file()
        && let Some(error) = walk.add_ignore(ignore)
    {
        return Err(RenameError::Invalid(error.to_string()));
    }
    let mut files = Vec::new();
    for entry in walk.build() {
        let entry = entry.map_err(|e| RenameError::Invalid(e.to_string()))?;
        if entry.file_type().is_some_and(|t| t.is_file()) {
            files.push(VaultPath::from_fs(vault.root(), entry.path())?);
        }
    }
    files.sort();
    Ok(files)
}

struct RewriteContext<'a> {
    from: &'a VaultPath,
    to: &'a VaultPath,
    directory: bool,
    new_title: Option<&'a str>,
    before: &'a Resolver,
    after: &'a Resolver,
    files: &'a [VaultPath],
    after_files: &'a [VaultPath],
    attachments: crate::attachments::Catalog,
    after_attachments: crate::attachments::Catalog,
}

impl RewriteContext<'_> {
    fn rewrite(&self, bytes: &[u8], source: &VaultPath) -> Result<(Vec<u8>, usize), RenameError> {
        let bom = if bytes.starts_with(b"\xEF\xBB\xBF") { 3 } else { 0 };
        let text = std::str::from_utf8(&bytes[bom..])
            .map_err(|_| RenameError::Invalid(format!("{source}: 非 UTF-8")))?;
        let new_source = relocated(source, self.from, self.to, self.directory);
        let mut edits = Vec::new();
        for span in links::scan_for_rename(text) {
            if span.target.is_empty() {
                continue;
            }
            let embed = span.range.start > 0 && text.as_bytes()[span.range.start - 1] == b'!';
            let attachment = embed && crate::attachments::Kind::for_path(&span.target).is_some();
            let resolved = if attachment {
                self.attachments.resolve(&span.target, source)
            } else {
                self.before.resolve(&span.target, None, source.parent())
            };
            let Resolution::Resolved { path, .. } = resolved else { continue };
            let desired = relocated(&path, self.from, self.to, self.directory);
            let replacement = if attachment {
                if matches!(self.after_attachments.resolve(&span.target, &new_source), Resolution::Resolved { path, .. } if path == desired)
                {
                    continue;
                }
                let relative = relative_path(new_source.parent(), desired.as_str());
                let mut candidates = vec![relative, desired.to_string(), desired.file_name().into()];
                if !span.target.contains('/') {
                    candidates.reverse();
                }
                candidates.into_iter().find_map(|candidate| {
                    if matches!(self.after_attachments.resolve(&candidate, &new_source), Resolution::Resolved { path, .. } if path == desired) {
                        links::escape_target(&candidate)
                    } else { None }
                }).ok_or_else(|| RenameError::Invalid(format!("无法写出附件链接：{desired}")))?
            } else {
                let explicit_path_moved = span.target.contains('/')
                    && resolve_note_path(&span.target, source.parent(), self.files).as_ref() == Some(&path)
                    && resolve_note_path(&span.target, new_source.parent(), self.after_files).as_ref()
                        != Some(&desired);
                if !explicit_path_moved
                    && resolves_to(self.after, &span.target, new_source.parent(), &desired)
                {
                    continue;
                }
                if desired == *self.from
                    && let Some(title) = self.new_title
                    && !(embed && crate::attachments::Kind::for_path(title).is_some())
                    && resolves_to(self.after, title, new_source.parent(), &desired)
                    && let Some(escaped) = links::escape_target(title)
                {
                    escaped
                } else {
                    replacement(&span.target, new_source.parent(), &desired, self.after, embed)?
                }
            };
            let range = links::target_range(text, &span);
            edits.push((bom + range.start..bom + range.end, replacement));
        }
        // 先用一次解析拿到的地址看有没有要改的：多数笔记一个都没有，省掉每个链接一次的整篇重解析。
        // 判断条件与下面循环里的跳过条件相同。
        let needs_rewrite = |url: &str| {
            if url.is_empty()
                || url.starts_with(['#', '?'])
                || url.starts_with("//")
                || url::Url::parse(url).is_ok()
            {
                return false;
            }
            let split = url.find(['#', '?']).unwrap_or(url.len());
            let Some(decoded) = decode_path(&url[..split]) else { return false };
            let Some(path) = resolve_file(&decoded, source.parent(), self.files) else { return false };
            let desired = relocated(&path, self.from, self.to, self.directory);
            resolve_file(&decoded, new_source.parent(), self.after_files).as_ref() != Some(&desired)
        };
        let destinations = if crate::markdown_links::link_targets(text).iter().any(|url| needs_rewrite(url)) {
            crate::markdown_links::destinations(text).map_err(RenameError::Invalid)?
        } else {
            Vec::new()
        };
        for destination in destinations {
            let url = &destination.url;
            if url.is_empty()
                || url.starts_with(['#', '?'])
                || url.starts_with("//")
                || url::Url::parse(url).is_ok()
            {
                continue;
            }
            let split = url.find(['#', '?']).unwrap_or(url.len());
            let Some(decoded) = decode_path(&url[..split]) else { continue };
            let Some(path) = resolve_file(&decoded, source.parent(), self.files) else { continue };
            let desired = relocated(&path, self.from, self.to, self.directory);
            if resolve_file(&decoded, new_source.parent(), self.after_files).as_ref() == Some(&desired) {
                continue;
            }
            let raw = &text[destination.range.clone()];
            let suffix = if split == url.len() {
                ""
            } else {
                raw.char_indices().filter(|(_, ch)| matches!(ch, '#' | '?' | '&' | '\\'))
                    .map(|(at, _)| &raw[at..])
                    .find(|suffix| {
                        let probe = format!("[x](<x{suffix}>)");
                        let arena = comrak::Arena::new();
                        let root = comrak::parse_document(&arena, &probe, &crate::parse::comrak_options());
                        root.descendants().any(|node| matches!(&node.data().value, comrak::nodes::NodeValue::Link(link) if link.url == format!("x{}", &url[split..])))
                    }).ok_or_else(|| RenameError::Invalid(format!("无法保留链接片段：{raw}")))?
            };
            let mut target = if decoded.starts_with('/') {
                format!("/{desired}")
            } else {
                relative_path(new_source.parent(), desired.as_str())
            };
            if decoded.starts_with("./") && !target.starts_with('.') {
                target = format!("./{target}");
            }
            if resolve_file(&target, new_source.parent(), self.after_files).as_ref() != Some(&desired) {
                return Err(RenameError::Invalid(format!("无法写出链接到 {desired}")));
            }
            let replacement = format!("{}{suffix}", encode_path(&target));
            edits.push((bom + destination.range.start..bom + destination.range.end, replacement));
        }
        edits.sort_by_key(|(range, _)| range.start);
        if edits.windows(2).any(|pair| pair[0].0.end > pair[1].0.start) {
            return Err(RenameError::Invalid(format!("链接源区间重叠：{source}")));
        }
        let count = edits.len();
        let mut out = bytes.to_vec();
        for (range, replacement) in edits.into_iter().rev() {
            out.splice(range, replacement.bytes());
        }
        Ok((out, count))
    }
}

fn relative_path(directory: &str, target: &str) -> String {
    let base: Vec<_> = directory.split('/').filter(|s| !s.is_empty()).collect();
    let parts: Vec<_> = target.split('/').collect();
    let common = base.iter().zip(&parts).take_while(|(a, b)| a == b).count();
    std::iter::repeat_n("..", base.len() - common)
        .chain(parts[common..].iter().copied())
        .collect::<Vec<_>>()
        .join("/")
}

fn resolve_note_path(target: &str, directory: &str, files: &[VaultPath]) -> Option<VaultPath> {
    for dir in [directory, ""] {
        for candidate in [target.to_owned(), format!("{target}.md")] {
            if let Some(path) = resolve_file(&candidate, dir, files).filter(VaultPath::is_note) {
                return Some(path);
            }
        }
    }
    None
}

pub(crate) fn resolve_file(target: &str, directory: &str, files: &[VaultPath]) -> Option<VaultPath> {
    let mut parts: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        directory.split('/').filter(|s| !s.is_empty()).collect()
    };
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part if part.contains('\\') || part.chars().any(char::is_control) => return None,
            part => parts.push(part),
        }
    }
    let normalized = parts.join("/");
    let mut matches = files.iter().filter(|p| p.as_str().to_lowercase() == normalized.to_lowercase());
    let found = matches.next()?.clone();
    matches.next().is_none().then_some(found)
}

pub(crate) fn decode_path(path: &str) -> Option<String> {
    let mut out = Vec::new();
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let hi = (bytes.next()? as char).to_digit(16)?;
            let lo = (bytes.next()? as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
        } else {
            out.push(byte);
        }
    }
    String::from_utf8(out).ok()
}

pub(crate) fn encode_path(path: &str) -> String {
    let mut encoded = String::new();
    for ch in path.chars() {
        if ch.is_ascii() && (!ch.is_ascii_alphanumeric() && !"/-._~".contains(ch)) {
            use std::fmt::Write;
            write!(encoded, "%{:02X}", ch as u8).expect("string write");
        } else {
            encoded.push(ch);
        }
    }
    encoded
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
    fn move_refuses_ignored_destination_and_stale_directory_contents() {
        let (dir, vault) = setup(&[("group/a.md", "body\n"), (".gitignore", "ignored/\n")]);
        std::fs::create_dir(dir.path().join("ignored")).unwrap();
        let result =
            RenamePlan::prepare(&vault, path("group"), path("ignored/group")).and_then(RenamePlan::execute);
        assert!(result.is_err());
        assert!(dir.path().join("group/a.md").is_file());
        let plan = RenamePlan::prepare(&vault, path("group"), path("new")).unwrap();
        std::fs::create_dir(dir.path().join("group/new-empty")).unwrap();
        assert!(plan.execute().is_err(), "changed directories must invalidate the plan");
    }

    #[test]
    fn rename_preserves_entity_encoded_fragment_and_query() {
        let source = "[a](old.md&#35;Heading) [b](old.md?x=1&amp;y=2#part)\n";
        let (dir, vault) = setup(&[("old.md", "body\n"), ("source.md", source)]);
        RenamePlan::prepare(&vault, path("old.md"), path("new.md")).unwrap().execute().unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("source.md")).unwrap(),
            source.replace("old.md", "new.md")
        );
    }

    #[cfg(unix)]
    #[test]
    fn move_refuses_symlinked_recovery_directory_and_changed_link_source() {
        use std::os::unix::fs::symlink;
        let (dir, vault) = setup(&[("a.md", "body\n"), ("b.md", "[[a]]\n")]);
        let outside = tempfile::tempdir().unwrap();
        std::fs::rename(dir.path().join(".aero"), dir.path().join("parked-config")).unwrap();
        symlink(outside.path(), dir.path().join(".aero")).unwrap();
        assert!(
            RenamePlan::prepare(&vault, path("a.md"), path("new.md")).and_then(RenamePlan::execute).is_err()
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn move_preserves_absolute_parent_and_encoded_paths_and_only_counts_rewrites() {
        let (dir, vault) = setup(&[
            ("group/a.md", "[root](/target.md#p) [outside](../target.md)\n"),
            ("group/b.md", "unchanged\n"),
            ("target.md", "[back](group/a.md?raw=1)\n"),
        ]);
        let plan = RenamePlan::prepare(&vault, path("group"), path("moved")).unwrap();
        assert_eq!(plan.other_files_changed(), 1, "unchanged moved files do not count as link rewrites");
        plan.execute().unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("moved/a.md")).unwrap(),
            "[root](/target.md#p) [outside](../target.md)\n"
        );
    }

    #[test]
    fn move_refuses_newly_ignored_attachment_paths() {
        let (dir, vault) =
            setup(&[("a.md", "![image](a.assets/x.png)\n"), (".gitignore", "dest/*.assets/\n")]);
        std::fs::create_dir(dir.path().join("dest")).unwrap();
        std::fs::create_dir(dir.path().join("a.assets")).unwrap();
        std::fs::write(dir.path().join("a.assets/x.png"), b"image").unwrap();
        assert!(
            RenamePlan::prepare(&vault, path("a.md"), path("dest/a.md"))
                .and_then(RenamePlan::execute)
                .is_err()
        );
        assert!(dir.path().join("a.assets/x.png").exists());
        assert!(dir.path().join("a.md").exists());
    }

    #[test]
    fn move_empty_directory_cannot_disappear_into_ignored_tree() {
        let (dir, vault) = setup(&[(".gitignore", "ignored/\n")]);
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        std::fs::create_dir(dir.path().join("ignored")).unwrap();
        assert!(
            RenamePlan::prepare(&vault, path("empty"), path("ignored/empty"))
                .and_then(RenamePlan::execute)
                .is_err()
        );
        assert!(dir.path().join("empty").is_dir());
    }

    #[test]
    fn retitle_does_not_turn_note_embeds_into_attachment_embeds() {
        let (dir, vault) = setup(&[("note.md", "---\ntitle: Old\n---\nbody\n"), ("source.md", "![[Old]]\n")]);
        std::fs::write(dir.path().join("Photo.png"), b"image").unwrap();
        RenamePlan::prepare_title(&vault, path("note.md"), "Photo.png".into()).unwrap().execute().unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("source.md")).unwrap(), "![[note]]\n");
    }

    #[test]
    fn retitle_updates_only_lost_title_targets_and_preserves_yaml_bytes() {
        let original = "\u{feff}---\r\ntitle: 'Old title' # keep\r\naliases: [Stable]\ncustom: untouched\r\n---\r\n\r\nbody\n";
        let source = "[[Old title#Heading|label]] ![[Old title#^id]] [[Stable]] [[note]] [md](note.md)\n";
        let (dir, vault) = setup(&[("note.md", original), ("source.md", source)]);
        RenamePlan::prepare_title(&vault, path("note.md"), "New title".into()).unwrap().execute().unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.md")).unwrap(),
            original.replace("'Old title'", "\"New title\"")
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("source.md")).unwrap(),
            source.replace("Old title", "New title")
        );
        assert!(!dir.path().join("New title.md").exists());
    }

    #[test]
    fn retitle_collision_uses_safe_path_and_rolls_back_title_on_failure() {
        let (dir, vault) = setup(&[
            ("note.md", "---\ntitle: Old title\n---\nbody\n"),
            ("other.md", "---\ntitle: New title\n---\n"),
            ("source.md", "[[Old title]]\n"),
        ]);
        let plan = RenamePlan::prepare_title(&vault, path("note.md"), "New title".into()).unwrap();
        let result = plan.execute_with(|p| {
            if p.as_str() == "source.md" {
                Err(RenameError::Invalid("simulated failure".into()))
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err(RenameError::Commit { .. })));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.md")).unwrap(),
            "---\ntitle: Old title\n---\nbody\n"
        );
        RenamePlan::prepare_title(&vault, path("note.md"), "New title".into()).unwrap().execute().unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("source.md")).unwrap(), "[[note]]\n");
    }

    #[test]
    fn move_note_rebases_incoming_outgoing_and_attachment_paths() {
        let original = "\u{feff}[[peer]] [peer](peer.md#part) ![image](A.assets/x%20y.png)\r\n![[A.assets/x y.png|240]]\n";
        let (dir, vault) = setup(&[
            ("old/A.md", original),
            ("old/peer.md", "old peer\n"),
            ("dest/peer.md", "different peer\n"),
            (
                "source.md",
                "[note](old/A.md#part \"title\") ![image](old/A.assets/x%20y.png) [[old/A#part|alias]]\n",
            ),
        ]);
        std::fs::create_dir(dir.path().join("old/A.assets")).unwrap();
        std::fs::write(dir.path().join("old/A.assets/x y.png"), b"image").unwrap();
        RenamePlan::prepare(&vault, path("old/A.md"), path("dest/B.md")).unwrap().execute().unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dest/B.md")).unwrap(),
            "\u{feff}[[old/peer]] [peer](../old/peer.md#part) ![image](B.assets/x%20y.png)\r\n![[B.assets/x y.png|240]]\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("source.md")).unwrap(),
            "[note](dest/B.md#part \"title\") ![image](dest/B.assets/x%20y.png) [[dest/B#part|alias]]\n"
        );
        assert!(dir.path().join("dest/B.assets/x y.png").is_file());
        assert!(!dir.path().join("old/A.md").exists());
    }

    #[test]
    fn move_directory_preserves_internal_paths_and_rebases_external_paths() {
        let (dir, vault) = setup(&[
            ("group/a.md", "[b](b.md) [outside](../outside.md) [[group/b]]\n"),
            ("group/b.md", "body\n"),
            ("outside.md", "[a](group/a.md) [[group/b]]\n"),
        ]);
        std::fs::create_dir_all(dir.path().join("dest")).unwrap();
        std::fs::create_dir_all(dir.path().join("group/empty")).unwrap();
        std::fs::write(dir.path().join("group/blob.bin"), b"unchanged").unwrap();
        RenamePlan::prepare(&vault, path("group"), path("dest/moved")).unwrap().execute().unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dest/moved/a.md")).unwrap(),
            "[b](b.md) [outside](../../outside.md) [[dest/moved/b]]\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("outside.md")).unwrap(),
            "[a](dest/moved/a.md) [[dest/moved/b]]\n"
        );
        assert_eq!(std::fs::read(dir.path().join("dest/moved/blob.bin")).unwrap(), b"unchanged");
        assert!(dir.path().join("dest/moved/empty").is_dir());
        assert!(!dir.path().join("group").exists());
    }

    #[test]
    fn rename_markdown_links_preserve_labels_titles_and_unrelated_bytes() {
        let source = "\u{feff}[中文](old.md#标题 \"留着\") ![图片](old.assets/photo.png)\r\n[ref][n]\n\n[n]: <old.md#part> 'title'\r\n\n`[no](old.md)` [remote](https://example.com/old.md) [missing](missing.md)\n";
        let (dir, vault) = setup(&[("old.md", "body\n"), ("source.md", source)]);
        std::fs::create_dir(dir.path().join("old.assets")).unwrap();
        std::fs::write(dir.path().join("old.assets/photo.png"), b"photo").unwrap();
        RenamePlan::prepare(&vault, path("old.md"), path("new.md")).unwrap().execute().unwrap();
        let expected = source
            .replace("(old.md#", "(new.md#")
            .replace("(old.assets/", "(new.assets/")
            .replace("<old.md#", "<new.md#");
        assert_eq!(std::fs::read(dir.path().join("source.md")).unwrap(), expected.as_bytes());
    }

    #[test]
    fn directory_move_rolls_back_all_rewrites_and_preserves_external_changes() {
        let (dir, vault) =
            setup(&[("group/a.md", "[b](../b.md)\n"), ("b.md", "[[group/a]]\n"), ("c.md", "[[group/a]]\n")]);
        let plan = RenamePlan::prepare(&vault, path("group"), path("moved")).unwrap();
        let result = plan.execute_with(|p| {
            if p.as_str() == "c.md" {
                std::fs::write(dir.path().join("b.md"), "external\n").unwrap();
                return Err(RenameError::Invalid("simulated failure".into()));
            }
            Ok(())
        });
        assert!(matches!(result, Err(RenameError::Commit { .. })));
        assert_eq!(std::fs::read_to_string(dir.path().join("b.md")).unwrap(), "external\n");
        assert!(dir.path().join("group/a.md").is_file());
        assert!(!dir.path().join("moved").exists());
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

    #[test]
    fn attachment_rename_preserves_binary_and_rewrites_both_link_forms() {
        let original = "![[old.png]] [image](a.assets/old.png#part)\n";
        let (dir, vault) = setup(&[("a.md", original)]);
        std::fs::create_dir(dir.path().join("a.assets")).unwrap();
        std::fs::write(dir.path().join("a.assets/old.png"), [0, 255, 128, 9]).unwrap();
        let report =
            RenamePlan::prepare_attachment(&vault, path("a.assets/old.png"), path("a.assets/new.png"))
                .unwrap()
                .execute()
                .unwrap();
        assert_eq!(report.updated, [path("a.md")]);
        assert_eq!(std::fs::read(dir.path().join("a.assets/new.png")).unwrap(), [0, 255, 128, 9]);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.md")).unwrap(),
            original.replace("old.png", "new.png")
        );
        assert!(!vault.exists(&path("a.assets/old.png")));
    }

    #[test]
    fn attachment_rename_rejects_changed_source_and_rolls_back_write_failure() {
        let (dir, vault) = setup(&[("a.md", "![[old.png]]\n"), ("b.md", "[image](old.png)\n")]);
        std::fs::write(dir.path().join("old.png"), b"original").unwrap();
        let plan = RenamePlan::prepare_attachment(&vault, path("old.png"), path("new.png")).unwrap();
        std::fs::write(dir.path().join("old.png"), b"external change").unwrap();
        assert!(matches!(plan.execute(), Err(RenameError::Changed(_))));
        assert!(!vault.exists(&path("new.png")));
        let plan = RenamePlan::prepare_attachment(&vault, path("old.png"), path("new.png")).unwrap();
        let result = plan.execute_with(|path| {
            if path.as_str() == "b.md" {
                Err(RenameError::Invalid("injected failure".into()))
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err(RenameError::Commit { .. })));
        assert!(vault.exists(&path("old.png")));
        assert!(!vault.exists(&path("new.png")));
        assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), "![[old.png]]\n");
    }
}
