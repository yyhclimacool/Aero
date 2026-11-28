//! Regenerate tree identities on each paste, never on clipboard ownership changes.

use crate::{Document, Edit};
use crate::{
    Vault, VaultPath,
    links::{Resolution, Resolver},
    transaction::FileChange,
};
use std::collections::{BTreeMap, HashMap};

/// A paste snapshots every managed note before resolving references. Unchanged
/// observations take part in validation but do not enter user-visible history.
pub struct Relocation {
    pub source: String,
    pub notes: Vec<FileChange>,
    pub resources: Vec<FileChange>,
    moved: HashMap<String, String>,
    origin: VaultPath,
    destination: String,
    resolver: Resolver,
}

impl Relocation {
    pub fn prepare(
        vault: &Vault,
        source: &str,
        origin: &Payload,
        destination: &VaultPath,
        document: &str,
        moving: bool,
    ) -> Result<Self, String> {
        let snapshot = crate::block_refs::vault_snapshot(vault)?;
        Self::prepare_from_snapshot(vault, source, origin, destination, document, moving, snapshot)
    }

    /// Plan a paste against a complete snapshot that includes current editor buffers.
    ///
    /// # Errors
    ///
    /// Returns an error if identities or links cannot be preserved.
    pub fn prepare_from_snapshot(
        vault: &Vault,
        source: &str,
        origin: &Payload,
        destination: &VaultPath,
        document: &str,
        moving: bool,
        snapshot: (Resolver, Vec<FileChange>),
    ) -> Result<Self, String> {
        let origin_path = VaultPath::new(&origin.path).map_err(|e| e.to_string())?;
        let (resolver, notes) = snapshot;
        if origin.vault != vault.id() {
            if moving {
                return Err("跨库剪切只能作为副本粘贴".into());
            }
            return Ok(Self {
                source: copy_tree(source, origin, document)?,
                notes,
                resources: Vec::new(),
                moved: HashMap::new(),
                origin: origin_path,
                destination: document.into(),
                resolver,
            });
        }
        if !moving {
            let text = copy_tree_resolved(source, origin, document, |span| {
                if span.target.is_empty() {
                    return true;
                }
                match resolver.resolve(&span.target, span.fragment.as_deref(), origin_path.parent()) {
                    Resolution::Resolved { path, .. } => path == origin_path,
                    Resolution::Unresolved => explicitly_local(&span.target, origin),
                    Resolution::Ambiguous { .. } => false,
                }
            })?;
            let mut result = Self {
                source: text,
                notes,
                resources: Vec::new(),
                moved: HashMap::new(),
                origin: origin_path,
                destination: document.into(),
                resolver,
            };
            result.source = result.rebase(vault, destination)?;
            return Ok(result);
        }
        let mut tree = Document::from_text(source);
        let mut moved = HashMap::new();
        let mut changes = Vec::new();
        for (_, block) in tree.blocks().iter().filter(|(_, b)| b.kind != crate::BlockKind::Marker) {
            let Some(old) = block.persisted_id.as_deref() else { continue };
            if moved.contains_key(old) {
                return Err(format!("块 ID 重复：{old}"));
            }
            let new =
                if crate::identity::is_block_id(old) { old.into() } else { crate::identity::new_block_id() };
            moved.insert(old.into(), new.clone());
            let mut aliases: Vec<_> = crate::block_refs::aliases(block).map(str::to_owned).collect();
            for alias in &aliases {
                if moved.insert(alias.clone(), new.clone()).is_some_and(|previous| previous != new) {
                    return Err(format!("块身份存在歧义：{alias}"));
                }
            }
            let original_aliases = aliases.clone();
            for alias in &mut aliases {
                if !crate::identity::is_block_id(alias) && !alias.contains('#') {
                    let parent = origin.document.as_deref().ok_or("剪切来源缺少文档身份")?;
                    *alias = format!("{parent}#{alias}");
                }
            }
            if new != old {
                let parent = origin.document.as_deref().ok_or("剪切来源缺少文档身份")?;
                aliases.push(format!("{parent}#{old}"));
            }
            if new != old || aliases != original_aliases {
                let mut attrs = block.attrs.clone();
                crate::properties::set(&mut attrs, "aero-aliases", Some(&aliases.join(" ")));
                changes.extend(crate::properties::edit(&tree, block.id, &attrs, Some(&new))?);
            }
        }
        changes.sort_by_key(|e| std::cmp::Reverse(e.range.start));
        tree.edit_group(changes).map_err(|e| e.to_string())?;
        tree.reparse();
        {
            let identities: std::collections::HashSet<_> = tree
                .blocks()
                .iter()
                .filter(|(_, b)| b.kind != crate::BlockKind::Marker)
                .flat_map(|(_, b)| b.persisted_id.as_deref().into_iter().chain(crate::block_refs::aliases(b)))
                .filter(|id| crate::identity::is_block_id(id) || id.contains('#'))
                .collect();
            for note in &notes {
                let doc = Document::from_bytes(note.before.as_ref().unwrap()).map_err(|e| e.to_string())?;
                if note.path != origin_path
                    && crate::identity::read(&doc)?.id == origin.document
                    && origin.document.is_some()
                {
                    return Err("剪切来源的文档身份重复，请先解决冲突".into());
                }
                if doc
                    .blocks()
                    .iter()
                    .filter(|(_, b)| b.kind != crate::BlockKind::Marker)
                    .flat_map(|(_, b)| {
                        b.persisted_id.as_deref().into_iter().chain(crate::block_refs::aliases(b))
                    })
                    .any(|id| identities.contains(id))
                {
                    return Err(format!("移动身份与 {} 冲突，请先解决重复块或别名", note.path));
                }
            }
        }
        let mut result = Self {
            source: tree.to_string(),
            notes,
            resources: Vec::new(),
            moved,
            origin: origin_path,
            destination: document.into(),
            resolver,
        };
        result.source = result.rebase(vault, destination)?;
        result.source = result.rewrite(&result.source, &result.origin);
        for i in 0..result.notes.len() {
            let note = &result.notes[i];
            let doc = Document::from_bytes(note.after.as_ref().unwrap()).map_err(|e| e.to_string())?;
            let text = result.rewrite(&doc.to_string(), &note.path);
            let mut rewritten = doc;
            rewritten.edit(Edit::replace(0..rewritten.len(), text)).map_err(|e| e.to_string())?;
            result.notes[i].after = Some(rewritten.to_bytes());
        }
        Ok(result)
    }

    /// Redirect only references to identities carried by this move. Short IDs
    /// are scoped to the original document; formal block IDs remain global.
    pub fn rewrite(&self, text: &str, note: &VaultPath) -> String {
        let mut edits = Vec::new();
        for span in crate::links::scan_for_rename(text) {
            let Some(old) = span.fragment.as_deref().and_then(|f| f.strip_prefix('^')) else { continue };
            let Some(new) = self.moved.get(old).or_else(|| {
                span.target.strip_prefix("id:").and_then(|id| self.moved.get(&format!("{id}#{old}")))
            }) else {
                continue;
            };
            let owner = if span.target.is_empty() {
                Some(note.clone())
            } else {
                match self.resolver.resolve(&span.target, None, note.parent()) {
                    Resolution::Resolved { path, .. } => Some(path),
                    _ => None,
                }
            };
            if owner.as_ref() != Some(&self.origin) && !crate::identity::is_block_id(old) {
                continue;
            }
            edits.push(Edit::replace(
                crate::links::head_range(text, &span),
                format!("id:{}#^{new}", self.destination),
            ));
        }
        replace(text, edits)
    }

    fn rebase(&mut self, vault: &Vault, destination: &VaultPath) -> Result<String, String> {
        let text = self.source.clone();
        let attachments = crate::attachments::Catalog::read(vault).map_err(|e| e.to_string())?;
        let files = crate::rename::file_catalog(vault).map_err(|e| e.to_string())?;
        let attachment_target = |target: &str| -> Result<Option<String>, String> {
            let bare = target.split(['#', '?']).next().unwrap_or(target);
            let Resolution::Resolved { path, .. } = attachments.resolve(bare, &self.origin) else {
                return Ok(None);
            };
            Ok(Some(format!("{path}{}", &target[bare.len()..])))
        };
        let mut edits = Vec::new();
        for span in crate::links::scan_for_rename(&text) {
            if crate::links::identity_target(&span.target).is_some() {
                continue;
            }
            if let Some(target) = attachment_target(&span.target)? {
                edits.push(Edit::replace(
                    crate::links::target_range(&text, &span),
                    crate::links::escape_target(&target).ok_or("附件路径无法编码")?,
                ));
                continue;
            }
            // Preserve internal links for copy_tree's identity remapping.
            let local = span.target.is_empty() || origin_target(&span.target, &self.origin, &self.resolver);
            if local
                && span
                    .fragment
                    .as_deref()
                    .and_then(|f| f.strip_prefix('^'))
                    .is_some_and(|id| self.moved.contains_key(id))
            {
                continue;
            }
            let path = if span.target.is_empty() {
                self.origin.clone()
            } else {
                match self.resolver.resolve(&span.target, None, self.origin.parent()) {
                    Resolution::Resolved { path, .. } => path,
                    _ => continue,
                }
            };
            let target = crate::links::escape_target(path.as_str()).ok_or("笔记路径无法编码")?;
            if !matches!(self.resolver.resolve(&target, None, destination.parent()), Resolution::Resolved { path: p, .. } if p == path)
            {
                return Err(format!("粘贴后的链接 {target} 存在歧义，请先为来源添加文档身份"));
            }
            edits.push(Edit::replace(crate::links::target_range(&text, &span), target));
        }
        for link in crate::markdown_links::destinations(&text)? {
            let url = &link.url;
            if url.is_empty()
                || url.starts_with(['#', '?'])
                || url.starts_with("//")
                || url::Url::parse(url).is_ok()
            {
                continue;
            }
            let split = url.find(['#', '?']).unwrap_or(url.len());
            let Some(decoded) = crate::rename::decode_path(&url[..split]) else { continue };
            let Some(path) = crate::rename::resolve_file(&decoded, self.origin.parent(), &files) else {
                continue;
            };
            let target = if decoded.starts_with('/') {
                format!("/{path}")
            } else {
                relative(destination.parent(), path.as_str())
            };
            if crate::rename::resolve_file(&target, destination.parent(), &files).as_ref() != Some(&path) {
                return Err(format!("无法保留资源链接：{url}"));
            }
            edits.push(Edit::replace(
                link.range,
                format!("{}{}", crate::rename::encode_path(&target), &url[split..]),
            ));
        }
        Ok(replace(&text, edits))
    }
}

fn relative(directory: &str, target: &str) -> String {
    let base: Vec<_> = directory.split('/').filter(|p| !p.is_empty()).collect();
    let parts: Vec<_> = target.split('/').collect();
    let common = base.iter().zip(&parts).take_while(|(a, b)| a == b).count();
    std::iter::repeat_n("..", base.len() - common)
        .chain(parts[common..].iter().copied())
        .collect::<Vec<_>>()
        .join("/")
}

fn origin_target(target: &str, origin: &VaultPath, resolver: &Resolver) -> bool {
    matches!(resolver.resolve(target, None, origin.parent()), Resolution::Resolved { path, .. } if path == *origin)
}

fn replace(text: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|e| std::cmp::Reverse(e.range.start));
    edits.dedup_by(|a, b| a.range == b.range);
    let mut result = text.to_owned();
    for edit in edits {
        result.replace_range(edit.range, &edit.text);
    }
    result
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Payload {
    pub version: u8,
    pub vault: String,
    pub path: String,
    pub document: Option<String>,
    pub cut: Option<String>,
}

pub fn copy_tree(source: &str, origin: &Payload, destination: &str) -> Result<String, String> {
    copy_tree_resolved(source, origin, destination, |span| explicitly_local(&span.target, origin))
}

fn explicitly_local(target: &str, origin: &Payload) -> bool {
    if let Some(id) = crate::links::identity_target(target) {
        return origin.document.as_deref() == Some(id);
    }
    target.is_empty()
        || target.strip_suffix(".md").unwrap_or(target)
            == origin.path.strip_suffix(".md").unwrap_or(&origin.path)
}

fn copy_tree_resolved(
    source: &str,
    origin: &Payload,
    destination: &str,
    is_local: impl Fn(&crate::links::LinkSpan) -> bool,
) -> Result<String, String> {
    let mut doc = Document::from_text(source);
    let mapping = regenerate_block_ids(&mut doc, origin.document.as_deref(), crate::identity::new_block_id)?;
    let source = doc.to_string();
    let mut replacements = Vec::new();
    for span in crate::links::scan_for_rename(&source) {
        let Some(old) = span.fragment.as_deref().and_then(|f| f.strip_prefix('^')) else { continue };
        if !is_local(&span) {
            continue;
        }
        let document = crate::links::identity_target(&span.target).or(origin.document.as_deref());
        let key = crate::links::block_identity_key(document, old);
        let Some(new) = mapping.get(key.as_deref().unwrap_or(old)) else { continue };
        replacements.push(Edit::replace(
            crate::links::head_range(&source, &span),
            format!("id:{destination}#^{new}"),
        ));
    }
    Ok(replace(&source, replacements))
}

/// Prepare a standalone Markdown copy and remap references resolved within the source document.
///
/// # Errors
///
/// Returns an error if the source contains ambiguous block identities or its metadata cannot be edited safely.
pub fn copy_document(
    mut doc: Document,
    origin: &VaultPath,
    mut resolver: Resolver,
) -> Result<Document, String> {
    use crate::{identity, links};

    let previous = identity::read(&doc)?.id;
    let id = std::iter::repeat_with(identity::new_document_id)
        .find(|id| {
            previous.as_ref() != Some(id)
                && matches!(resolver.resolve(&format!("id:{id}"), None, ""), Resolution::Unresolved)
        })
        .expect("document ID space is not exhausted");
    resolver.insert(origin.clone(), Some(&doc.title_or(origin.stem())), &doc.front_matter_aliases());
    resolver.register_identity(origin, &doc);
    let mut allocate = || {
        std::iter::repeat_with(identity::new_block_id)
            .find(|block| {
                matches!(
                    resolver.resolve(&format!("id:{id}"), Some(&format!("^{block}")), ""),
                    Resolution::Unresolved
                )
            })
            .expect("block ID space is not exhausted")
    };
    let mapping = regenerate_block_ids(&mut doc, previous.as_deref(), &mut allocate)?;
    let source = doc.to_string();
    let mut edits = Vec::new();
    for span in links::scan_for_rename(&source) {
        if span.pointer.is_some() {
            continue;
        }
        let target_id = links::identity_target(&span.target);
        let is_local = span.target.is_empty()
            || (target_id.is_some() && target_id == previous.as_deref())
            || matches!(resolver.resolve(&span.target, span.fragment.as_deref(), origin.parent()),
                Resolution::Resolved { path, .. } if &path == origin);
        if !is_local {
            continue;
        }
        let head = match span.fragment.as_deref() {
            Some(fragment) if fragment.starts_with('^') => {
                let old = &fragment[1..];
                let key = links::block_identity_key(target_id.or(previous.as_deref()), old);
                let Some(new) = mapping.get(key.as_deref().unwrap_or(old)) else { continue };
                format!("id:{id}#^{new}")
            }
            Some(_) if span.target.is_empty() => continue,
            Some(_) => {
                // Keep the user's original fragment spelling and optional alias.
                let target = links::target_range(&source, &span);
                edits.push(Edit::replace(target, format!("id:{id}")));
                continue;
            }
            None => format!("id:{id}"),
        };
        edits.push(Edit::replace(links::head_range(&source, &span), head));
    }
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
    doc.edit_group(edits).map_err(|e| e.to_string())?;
    doc.reparse();
    identity::reset_for_copy(&mut doc, id)?;
    Ok(doc)
}

fn regenerate_block_ids(
    doc: &mut Document,
    document: Option<&str>,
    mut new_block: impl FnMut() -> String,
) -> Result<HashMap<String, String>, String> {
    use crate::BlockKind;

    let mut mapping = HashMap::new();
    let mut assigned = std::collections::HashSet::new();
    let mut by_depth = BTreeMap::<_, Vec<_>>::new();
    let mut structure = Vec::new();
    for (depth, block) in doc.blocks().iter().filter(|(_, b)| b.kind != BlockKind::Marker) {
        let ordinal = structure.len();
        structure.push((depth, block.kind.clone()));
        let mut aliases = crate::block_refs::aliases(block).peekable();
        if block.persisted_id.is_none() && aliases.peek().is_none() {
            continue;
        }
        let id = std::iter::repeat_with(&mut new_block)
            .find(|id| assigned.insert(id.clone()))
            .expect("block ID space is not exhausted");
        for old in block.persisted_id.as_deref().into_iter().chain(aliases) {
            let key = crate::links::block_identity_key(document, old).unwrap_or_else(|| old.to_owned());
            if mapping.insert(key, id.clone()).is_some_and(|previous| previous != id) {
                return Err(format!("复制内容含歧义块身份：{old}"));
            }
        }
        by_depth.entry(depth).or_default().push((ordinal, id));
    }
    // A container's property edit can replace its entire source range. Edit
    // descendants first so the container edit uses their updated source.
    for (_, batch) in by_depth.into_iter().rev() {
        let mut edits = {
            let mut edits = Vec::new();
            let mut blocks = doc.blocks().iter().filter(|(_, b)| b.kind != BlockKind::Marker).enumerate();
            for (ordinal, id) in &batch {
                let (_, (_, block)) = blocks.find(|(index, _)| index == ordinal).ok_or("副本块已不存在")?;
                let mut attrs = block.attrs.clone();
                crate::properties::set(&mut attrs, "aero-aliases", None);
                if let Some(edit) = crate::properties::edit(doc, block.id, &attrs, Some(id))? {
                    edits.push(edit);
                }
            }
            edits
        };
        edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
        if edits.windows(2).any(|pair| pair[1].range.end > pair[0].range.start) {
            return Err("副本的块元数据范围重叠".into());
        }
        doc.edit_group(edits).map_err(|e| e.to_string())?;
        doc.reparse();
        let blocks: Vec<_> = doc.blocks().iter().filter(|(_, b)| b.kind != BlockKind::Marker).collect();
        if !blocks.iter().map(|(depth, b)| (*depth, &b.kind)).eq(structure.iter().map(|(d, k)| (*d, k)))
            || batch.iter().any(|(ordinal, id)| {
                let block = blocks[*ordinal].1;
                block.persisted_id.as_deref() != Some(id)
                    || crate::block_refs::aliases(block).next().is_some()
            })
        {
            return Err("无法保持副本的原始块结构和身份归属".into());
        }
    }
    Ok(mapping)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copying_a_document_remaps_nested_ids_links_and_embeds() {
        use crate::{block_refs, identity, links};
        let old_doc = identity::new_document_id();
        let other_doc = identity::new_document_id();
        let old_alias = identity::new_block_id();
        let source = format!(
            "---\ntitle: 原文\naero: {{id: {old_doc}, parent: {other_doc}, order: 9, custom: keep}}\n---\n\n\
             :::highlight {{.bg-yellow}}\ninside {{aero-aliases={old_alias}}} ^inner\n:::\n\n^container\n\n\
             [[原文|copy]] [[#^inner|here]] ![[id:{old_doc}#^{old_alias}|same]]\n\n\
             [[source#^container]] [[id:{other_doc}#^inner]] `[[#^inner]]`\n"
        );
        let mut bytes = b"\xEF\xBB\xBF".to_vec();
        bytes.extend(source.replace('\n', "\r\n").bytes());
        let origin = Document::from_bytes(&bytes).unwrap();
        let original_block_ids: Vec<_> = origin
            .blocks()
            .iter()
            .filter(|(_, b)| b.kind != crate::BlockKind::Marker)
            .filter_map(|(_, b)| b.persisted_id.as_deref().map(str::to_owned))
            .collect();
        assert_eq!(original_block_ids, ["container", "inner"]);
        let doc = copy_document(origin, &VaultPath::new("source.md").unwrap(), Resolver::new()).unwrap();
        let copied = identity::read(&doc).unwrap();
        let id = copied.id.unwrap();
        assert_ne!(id, old_doc);
        assert_eq!(copied.parent, None);
        assert_eq!(copied.order, None);
        let text = doc.to_string();
        assert!(text.contains("custom: keep"));
        assert!(text.contains(":::highlight {.bg-yellow}"));
        assert!(text.contains(&format!("[[id:{other_doc}#^inner]] `[[#^inner]]`")));
        assert!(doc.to_bytes().starts_with(b"\xEF\xBB\xBF---\r\n"));
        let links = links::scan(&text);
        assert_eq!(links[0].target, format!("id:{id}"));
        assert_eq!(links[0].alias.as_deref(), Some("copy"));
        let inner = links[1].fragment.as_deref().unwrap();
        let embed = &crate::links::scan_embeds(&text)[0];
        assert_eq!(embed.fragment.as_deref(), Some(inner));
        assert_eq!(embed.target, format!("id:{id}"));
        let block = block_refs::resolve(&doc, &inner[1..]).unwrap();
        assert!(doc.block_text(doc.blocks().find(block).unwrap()).contains("inside"));
        assert!(block_refs::resolve(&doc, &links[2].fragment.as_deref().unwrap()[1..]).is_ok());
        assert!(block_refs::resolve(&doc, "inner").is_err());
        assert!(block_refs::resolve(&doc, &old_alias).is_err());
    }

    #[test]
    fn copying_a_legacy_child_replaces_parent_only_metadata() {
        let parent = crate::identity::new_document_id();
        for metadata in [format!("aero:\n  parent: {parent}"), format!("aero: {{parent: {parent}}}")] {
            let original = Document::from_text(&format!("---\n{metadata}\n---\n\nOld child\n"));
            let copy = copy_document(original, &VaultPath::new("old.md").unwrap(), Resolver::new()).unwrap();
            let meta = crate::identity::read(&copy).unwrap();
            assert!(meta.id.is_some());
            assert_eq!(meta.parent, None);
            assert!(copy.to_string().ends_with("\nOld child\n"));
        }
    }

    #[test]
    fn copied_subtree_rejects_ambiguous_aliases() {
        let origin =
            Payload { version: 1, vault: "v".into(), path: "source.md".into(), document: None, cut: None };
        assert!(
            copy_tree(
                "first {aero-aliases=\"same\"} ^one\n\nsecond {aero-aliases=\"same\"} ^two",
                &origin,
                &crate::identity::new_document_id()
            )
            .is_err()
        );
    }

    #[test]
    fn markdown_relative_links_keep_original_targets_fragments_and_unicode() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join("from")).unwrap();
        std::fs::create_dir_all(dir.path().join("to")).unwrap();
        std::fs::write(dir.path().join("from/source.md"), "remaining").unwrap();
        std::fs::write(dir.path().join("from/文档 name.md"), "target").unwrap();
        std::fs::write(dir.path().join("to/dest.md"), "destination").unwrap();
        let origin = Payload {
            version: 1,
            vault: vault.id().into(),
            path: "from/source.md".into(),
            document: None,
            cut: None,
        };
        let source = "[note](文档%20name.md#heading)\n\n[web](https://example.com/path?q=1)";
        let plan = Relocation::prepare(
            &vault,
            source,
            &origin,
            &VaultPath::new("to/dest.md").unwrap(),
            &crate::identity::new_document_id(),
            false,
        )
        .unwrap();
        assert!(plan.source.contains("[note](../from/文档%20name.md#heading)"), "{}", plan.source);
        assert!(plan.source.contains("[web](https://example.com/path?q=1)"));
    }

    #[test]
    fn same_vault_copy_preserves_external_identity_addresses_and_remaps_internal_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let origin_id = crate::identity::new_document_id();
        let external_id = crate::identity::new_document_id();
        let destination_id = crate::identity::new_document_id();
        let primary = crate::identity::new_block_id();
        let external = crate::identity::new_block_id();
        let outside = format!("![[id:{external_id}#^{external}|outside]]");
        let text = format!(
            "inside {{aero-aliases=\"{origin_id}#legacy\"}} ^{primary}\n\n![[id:{origin_id}#^legacy|alias]]\n\n[[Custom title#^{primary}|title]]\n\n{outside}\n"
        );
        std::fs::write(
            dir.path().join("source.md"),
            format!("---\ntitle: Custom title\naero:\n  id: {origin_id}\n---\n\n{text}"),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("external.md"),
            format!("---\naero:\n  id: {external_id}\n---\n\nexternal ^{external}\n"),
        )
        .unwrap();
        let origin = Payload {
            version: 1,
            vault: vault.id().into(),
            path: "source.md".into(),
            document: Some(origin_id),
            cut: None,
        };
        let destination = VaultPath::new("destination.md").unwrap();
        let plan = Relocation::prepare(&vault, &text, &origin, &destination, &destination_id, false).unwrap();
        let doc = Document::from_text(&plan.source);
        let id = doc.blocks().roots[0].persisted_id.as_deref().unwrap();
        assert_ne!(id, primary);
        assert!(!plan.source.contains("aero-aliases"));
        assert!(plan.source.contains(&outside), "{}", plan.source);
        assert!(plan.source.contains(&format!("![[id:{destination_id}#^{id}|alias]]")), "{}", plan.source);
        assert!(plan.source.contains(&format!("[[id:{destination_id}#^{id}|title]]")), "{}", plan.source);
        let only_reference =
            Relocation::prepare(&vault, &outside, &origin, &destination, &destination_id, false).unwrap();
        assert_eq!(only_reference.source, outside);
    }

    #[test]
    fn relocation_upgrades_scoped_ids_and_preserves_original_resource_locations() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let origin_id = crate::identity::new_document_id();
        let destination_id = crate::identity::new_document_id();
        std::fs::create_dir_all(dir.path().join("Source.assets")).unwrap();
        std::fs::create_dir_all(dir.path().join("folder")).unwrap();
        std::fs::write(dir.path().join("Source.assets/a.png"), [0, 255, 8, 0]).unwrap();
        std::fs::write(
            dir.path().join("Source.md"),
            format!("---\naero:\n  id: {origin_id}\n---\n\nremaining\n"),
        )
        .unwrap();
        std::fs::write(dir.path().join("folder/Dest.md"), "destination\n").unwrap();
        std::fs::write(dir.path().join("Back.md"), "[[Source#^old|back]]\n").unwrap();
        std::fs::write(dir.path().join("Other.md"), "same short ^old\n\n[[#^old|unrelated]]\n").unwrap();
        let payload = Payload {
            version: 1,
            vault: vault.id().into(),
            path: "Source.md".into(),
            document: Some(origin_id.clone()),
            cut: Some("token".into()),
        };
        let dest = VaultPath::new("folder/Dest.md").unwrap();
        let source = "body ^old\n\n[[#^old|self]]\n\n![[a.png]]\n\n![a](Source.assets/a.png)\n";
        let plan = Relocation::prepare(&vault, source, &payload, &dest, &destination_id, true).unwrap();
        let doc = Document::from_text(&plan.source);
        let id = doc.blocks().roots[0].persisted_id.as_deref().unwrap();
        assert!(crate::identity::is_block_id(id));
        assert!(plan.source.contains(&format!("{origin_id}#old")));
        assert!(plan.source.contains(&format!("id:{destination_id}#^{id}|self")));
        let backlink = plan.notes.iter().find(|n| n.path.as_str() == "Back.md").unwrap();
        assert!(
            String::from_utf8(backlink.after.clone().unwrap())
                .unwrap()
                .contains(&format!("id:{destination_id}#^{id}|back"))
        );
        let unrelated = plan.notes.iter().find(|n| n.path.as_str() == "Other.md").unwrap();
        assert_eq!(unrelated.before, unrelated.after);
        assert!(plan.resources.is_empty(), "a move never copies or relocates resources");
        assert!(plan.source.contains("![a](../Source.assets/a.png)"));
        assert!(plan.source.contains("![[Source.assets/a.png]]"));
        assert_eq!(std::fs::read(dir.path().join("Source.assets/a.png")).unwrap(), [0, 255, 8, 0]);
    }

    #[test]
    fn relocation_rejects_unreadable_or_invalid_managed_notes() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        std::fs::write(dir.path().join("Source.md"), "source").unwrap();
        std::fs::write(dir.path().join("Bad.md"), [0xff, 0xff]).unwrap();
        let payload = Payload {
            version: 1,
            vault: vault.id().into(),
            path: "Source.md".into(),
            document: None,
            cut: None,
        };
        assert!(
            Relocation::prepare(
                &vault,
                "body",
                &payload,
                &VaultPath::new("Source.md").unwrap(),
                &crate::identity::new_document_id(),
                false
            )
            .is_err()
        );
    }

    #[test]
    fn each_copy_has_fresh_ids_and_internal_references_follow_the_copy() {
        let source = "first {aero-aliases=\"old-alias\" .red} ^old\n\n[[#^old|local]]\n\n![[#^old]]\n\n[[Else#^old|external]]";
        let origin =
            Payload { version: 1, vault: "v".into(), path: "Source.md".into(), document: None, cut: None };
        let destination = crate::identity::new_document_id();
        let first = copy_tree(source, &origin, &destination).unwrap();
        let second = copy_tree(source, &origin, &destination).unwrap();
        assert_ne!(first, second);
        assert!(!first.contains("aero-aliases"));
        assert!(first.contains("[[Else#^old|external]]"));
        assert!(first.contains(".red"));
        let doc = Document::from_text(&first);
        let id = doc.blocks().roots[0].persisted_id.as_deref().unwrap();
        assert!(first.contains(&format!("id:{destination}#^{id}|local")), "{first}");
        assert!(first.contains(&format!("![[id:{destination}#^{id}]]")));
    }
}
