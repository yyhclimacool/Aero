//! Regenerate tree identities on each paste, never on clipboard ownership changes.

use crate::{Document, Edit};
use crate::{
    Vault, VaultPath,
    links::{Resolution, Resolver},
    transaction::FileChange,
};
use std::collections::HashMap;

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
    documents: HashMap<VaultPath, String>,
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
        let origin_path = VaultPath::new(&origin.path).map_err(|e| e.to_string())?;
        let mut resolver = Resolver::new();
        let mut notes = Vec::new();
        let mut documents = HashMap::new();
        if let Some(id) = &origin.document {
            documents.insert(origin_path.clone(), id.clone());
        }
        for path in vault.notes().map_err(|e| e.to_string())? {
            let (bytes, _) = vault.read_bytes(&path).map_err(|e| e.to_string())?;
            let doc = Document::from_bytes(&bytes).map_err(|e| e.to_string())?;
            if let Some(id) = crate::identity::read(&doc)?.id {
                documents.insert(path.clone(), id);
            }
            resolver.insert(path.clone(), Some(&doc.title_or(path.stem())), &doc.front_matter_aliases());
            resolver.register_identity(&path, &doc);
            notes.push(FileChange { path, before: Some(bytes.clone()), after: Some(bytes) });
        }
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
                documents,
            });
        }
        let mut tree = Document::from_text(source);
        let mut moved = HashMap::new();
        let mut changes = Vec::new();
        for (_, block) in tree.blocks().iter().filter(|(_, b)| b.kind != crate::BlockKind::Marker) {
            let Some(old) = block.persisted_id.as_deref() else { continue };
            if moved.contains_key(old) {
                return Err(format!("块 ID 重复：{old}"));
            }
            let new = if !moving || crate::identity::is_block_id(old) {
                old.into()
            } else {
                crate::identity::new_block_id()
            };
            moved.insert(old.into(), new.clone());
            let mut aliases: Vec<_> = crate::block_refs::aliases(block).map(str::to_owned).collect();
            for alias in &aliases {
                if moved.insert(alias.clone(), new.clone()).is_some_and(|previous| previous != new) {
                    return Err(format!("块身份存在歧义：{alias}"));
                }
            }
            let original_aliases = aliases.clone();
            if moving {
                for alias in &mut aliases {
                    if !crate::identity::is_block_id(alias) && !alias.contains('#') {
                        let parent = origin.document.as_deref().ok_or("剪切来源缺少文档身份")?;
                        *alias = format!("{parent}#{alias}");
                    }
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
        if moving {
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
            documents,
        };
        if moving {
            // Resolve moved references before rebasing can change their path context.
            result.source = result.rewrite(&result.source, &result.origin);
        } else {
            // Resolve copied references in their original context, before path rebasing.
            result.source =
                copy_tree_with_resolver(&result.source, origin, document, Some(&result.resolver))?;
        }
        result.source = result.rebase(vault, destination)?;
        if moving {
            for i in 0..result.notes.len() {
                let note = &result.notes[i];
                let doc = Document::from_bytes(note.after.as_ref().unwrap()).map_err(|e| e.to_string())?;
                let text = result.rewrite(&doc.to_string(), &note.path);
                let mut rewritten = doc;
                rewritten.edit(Edit::replace(0..rewritten.len(), text)).map_err(|e| e.to_string())?;
                result.notes[i].after = Some(rewritten.to_bytes());
            }
        }
        Ok(result)
    }

    /// Redirect only references to identities carried by this move. Short IDs
    /// are scoped to the referenced document; formal block IDs remain global.
    pub fn rewrite(&self, text: &str, note: &VaultPath) -> String {
        let mut edits = Vec::new();
        for span in crate::links::scan_for_rename(text) {
            let Some(old) = span.fragment.as_deref().and_then(|f| f.strip_prefix('^')) else { continue };
            let mut owner = if span.target.is_empty() {
                Some(note.clone())
            } else {
                match self.resolver.resolve(&span.target, None, note.parent()) {
                    Resolution::Resolved { path, .. } => Some(path),
                    _ => None,
                }
            };
            let mut scope = span
                .target
                .strip_prefix("id:")
                .filter(|id| crate::identity::is_document_id(id))
                .map(str::to_owned)
                .or_else(|| owner.as_ref().and_then(|path| self.documents.get(path)).cloned());
            let mut identity = old.to_owned();
            if let Some(document) = &scope {
                // A historical address can resolve to a block outside the moved subtree.
                match self.resolver.resolve(
                    &format!("id:{document}"),
                    span.fragment.as_deref(),
                    note.parent(),
                ) {
                    Resolution::Resolved { path, fragment } => {
                        let Some(primary) = fragment.as_deref().and_then(|f| f.strip_prefix('^')) else {
                            continue;
                        };
                        identity = primary.to_owned();
                        scope = self.documents.get(&path).cloned();
                        owner = Some(path);
                    }
                    Resolution::Ambiguous { .. } => continue,
                    // A cut leaves the catalog, so match carried aliases only within their scope.
                    Resolution::Unresolved => {}
                }
            }
            let Some(new) =
                scope.as_ref().and_then(|id| self.moved.get(&format!("{id}#{identity}"))).or_else(|| {
                    (owner.as_ref() == Some(&self.origin) || crate::identity::is_block_id(&identity))
                        .then(|| self.moved.get(&identity))
                        .flatten()
                })
            else {
                continue;
            };
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
            // Rebase paths without downgrading location-independent identity addresses.
            if span.target.strip_prefix("id:").is_some_and(crate::identity::is_document_id) {
                continue;
            }
            if let Some(target) = attachment_target(&span.target)? {
                edits.push(Edit::replace(
                    crate::links::target_range(&text, &span),
                    crate::links::escape_target(&target).ok_or("附件路径无法编码")?,
                ));
                continue;
            }
            // Preserve internal links for a move's identity rewriting.
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
    copy_tree_with_resolver(source, origin, destination, None)
}

fn copy_tree_with_resolver(
    source: &str,
    origin: &Payload,
    destination: &str,
    resolver: Option<&Resolver>,
) -> Result<String, String> {
    let mut doc = Document::from_text(source);
    let mut mapping = HashMap::new();
    let mut assignments = std::collections::BTreeMap::<usize, Vec<_>>::new();
    let shape = doc
        .blocks()
        .iter()
        .filter(|(_, b)| b.kind != crate::BlockKind::Marker)
        .map(|(depth, b)| (depth, b.kind.clone()))
        .collect::<Vec<_>>();
    for (index, (depth, block)) in
        doc.blocks().iter().filter(|(_, b)| b.kind != crate::BlockKind::Marker).enumerate()
    {
        let old = block.persisted_id.as_deref();
        if old.is_none() && !block.attrs.pairs.iter().any(|(key, _)| key == "aero-aliases") {
            continue;
        }
        if let Some(old) = old
            && mapping.contains_key(old)
        {
            return Err(format!("复制内容含重复块 ID：{old}"));
        }
        // Empty alias attributes do not justify allocating a persistent identity.
        let new = (old.is_some() || crate::block_refs::aliases(block).next().is_some())
            .then(crate::identity::new_block_id);
        if let Some(new) = &new {
            for id in old.into_iter().chain(crate::block_refs::aliases(block)) {
                if mapping.insert(id.to_owned(), new.clone()).is_some_and(|previous| previous != *new) {
                    return Err(format!("复制内容含歧义块身份：{id}"));
                }
            }
        }
        let mut attrs = block.attrs.clone();
        crate::properties::set(&mut attrs, "aero-aliases", None);
        assignments.entry(depth).or_default().push((index, attrs, new));
    }
    // Parent metadata edits can cover a subtree, so apply non-overlapping depth groups bottom-up.
    for (_, assignments) in assignments.into_iter().rev() {
        let nodes = doc
            .blocks()
            .iter()
            .filter(|(_, b)| b.kind != crate::BlockKind::Marker)
            .map(|(_, b)| b)
            .collect::<Vec<_>>();
        let mut edits = Vec::new();
        for (index, attrs, id) in assignments {
            if let Some(edit) = crate::properties::edit(&doc, nodes[index].id, &attrs, id.as_deref())? {
                edits.push(edit);
            }
        }
        edits.sort_by_key(|e| std::cmp::Reverse(e.range.start));
        doc.edit_group(edits).map_err(|e| e.to_string())?;
        doc.reparse();
        let current = doc
            .blocks()
            .iter()
            .filter(|(_, b)| b.kind != crate::BlockKind::Marker)
            .map(|(depth, b)| (depth, b.kind.clone()))
            .collect::<Vec<_>>();
        if current != shape {
            return Err("复制身份时块结构发生变化，已取消复制".into());
        }
    }
    let source = doc.to_string();
    let mut replacements = Vec::new();
    let spans = crate::links::scan(&source).into_iter().chain(
        crate::links::scan_embeds(&source).into_iter().map(|e| crate::links::LinkSpan {
            range: e.range.start + 1..e.range.end,
            target: e.target,
            fragment: e.fragment,
            alias: e.parameter,
            pointer: None,
        }),
    );
    for span in spans {
        let Some(old) = span.fragment.as_deref().and_then(|f| f.strip_prefix('^')) else { continue };
        let mut local = span.target.is_empty()
            || origin.document.as_ref().is_some_and(|id| span.target == format!("id:{id}"))
            || span.target.strip_suffix(".md").unwrap_or(&span.target)
                == origin.path.strip_suffix(".md").unwrap_or(&origin.path);
        let mut canonical = None;
        if let Some(resolver) = resolver
            && !span.target.is_empty()
        {
            let path = VaultPath::new(&origin.path).map_err(|e| e.to_string())?;
            // Resolve historical fragments before checking document ownership.
            match resolver.resolve(&span.target, span.fragment.as_deref(), path.parent()) {
                Resolution::Resolved { path, fragment } if path.as_str() == origin.path => {
                    local = true;
                    if span.target.strip_prefix("id:").is_some_and(crate::identity::is_document_id) {
                        canonical = fragment;
                    } else if let Some(document) = &origin.document {
                        match resolver.resolve(&format!("id:{document}"), fragment.as_deref(), path.parent()) {
                            Resolution::Resolved { path, fragment } if path.as_str() == origin.path => canonical = fragment,
                            Resolution::Resolved { .. } | Resolution::Ambiguous { .. } => continue,
                            Resolution::Unresolved => {}
                        }
                    }
                }
                Resolution::Resolved { .. } | Resolution::Ambiguous { .. } => continue,
                Resolution::Unresolved => {
                    // Alias-only blocks need a verified document owner before fallback matching.
                    local = matches!(
                        resolver.resolve(&span.target, None, path.parent()),
                        Resolution::Resolved { path, .. } if path.as_str() == origin.path
                    );
                }
            }
        }
        // Without a catalog, short aliases require an explicit or known origin scope.
        let scope = span
            .target
            .strip_prefix("id:")
            .filter(|id| crate::identity::is_document_id(id))
            .or_else(|| local.then_some(origin.document.as_deref()).flatten());
        let scoped = scope.map(|id| format!("{id}#{old}"));
        let new = if let Some(canonical) = canonical.as_deref() {
            canonical.strip_prefix('^').and_then(|id| mapping.get(id))
        } else {
            scoped.as_ref().and_then(|id| mapping.get(id))
                .or_else(|| local.then(|| mapping.get(old)).flatten())
        };
        let Some(new) = new else { continue };
        replacements.push(Edit::replace(
            crate::links::head_range(&source, &span),
            format!("id:{destination}#^{new}"),
        ));
    }
    replacements.sort_by_key(|e| std::cmp::Reverse(e.range.start));
    replacements.dedup_by(|a, b| a.range == b.range);
    let mut text = source;
    for edit in replacements {
        text.replace_range(edit.range, &edit.text);
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copying_parent_and_child_identities_keeps_tree_and_internal_references() {
        let origin = Payload {
            version: 1,
            vault: "v".into(),
            path: "Source.md".into(),
            document: Some(crate::identity::new_document_id()),
            cut: None,
        };
        let destination = crate::identity::new_document_id();
        for source in [
            ":::highlight {.bg-blue}\nchild {.red} ^child\n\n![[#^child]]\n:::\n\n^box\n",
            "- {aero-owner=item .bg-blue} ^box\n\n  child {.red} ^child\n\n  ![[#^child]]\n",
        ] {
            let before = Document::from_text(source);
            let shape = before
                .blocks()
                .iter()
                .filter(|(_, b)| b.kind != crate::BlockKind::Marker)
                .map(|(depth, b)| (depth, b.kind.clone()))
                .collect::<Vec<_>>();
            let copied = copy_tree(source, &origin, &destination).unwrap();
            let after = Document::from_text(&copied);
            assert_eq!(
                after
                    .blocks()
                    .iter()
                    .filter(|(_, b)| b.kind != crate::BlockKind::Marker)
                    .map(|(depth, b)| (depth, b.kind.clone()))
                    .collect::<Vec<_>>(),
                shape,
                "{copied}"
            );
            let ids = after
                .blocks()
                .iter()
                .filter(|(_, b)| b.kind != crate::BlockKind::Marker)
                .filter_map(|(_, b)| b.persisted_id.as_deref())
                .collect::<Vec<_>>();
            assert_eq!(ids.len(), 2, "{copied}");
            assert!(ids.iter().all(|id| crate::identity::is_block_id(id)));
            let child =
                after.blocks().iter().find(|(_, b)| b.attrs.classes.iter().any(|c| c == "red")).unwrap().1;
            let child_id = child.persisted_id.as_deref().unwrap();
            assert_eq!(after.slice(after.content_range(child)).to_string(), "child");
            assert!(copied.contains(&format!("![[id:{destination}#^{child_id}]]")), "{copied}");
        }
    }

    #[test]
    fn alias_only_copies_get_fresh_identity_without_changing_body_or_other_attributes() {
        let origin = Payload {
            version: 1,
            vault: "v".into(),
            path: "Source.md".into(),
            document: Some(crate::identity::new_document_id()),
            cut: None,
        };
        let destination = crate::identity::new_document_id();
        let code = "```text\n![[#^old]] {aero-aliases=literal} ^literal\n```";
        let source = format!(
            "body **unchanged** {{aero-aliases=old .red custom=keep}}\n\nordinary {{custom=untouched}}\n\n![[#^old]]\n\n[[Else#^old|external]]\n\n`[[#^old]]`\n\n{code}\n"
        );
        let mut ids = Vec::new();
        for _ in 0..2 {
            let copy = copy_tree(&source, &origin, &destination).unwrap();
            let doc = Document::from_text(&copy);
            let block = &doc.blocks().roots[0];
            let id = block.persisted_id.as_deref().expect("alias-only block needs a fresh primary ID");
            assert!(crate::identity::is_block_id(id));
            assert!(crate::block_refs::aliases(block).next().is_none());
            assert_eq!(block.attrs.classes, ["red"]);
            assert_eq!(block.attrs.pairs, [("custom".into(), "keep".into())]);
            assert!(copy.starts_with("body **unchanged** "));
            assert!(copy.contains("ordinary {custom=untouched}"));
            assert_eq!(doc.blocks().roots[1].persisted_id, None);
            assert!(copy.contains(&format!("![[id:{destination}#^{id}]]")), "{copy}");
            assert!(copy.contains("[[Else#^old|external]]"));
            assert!(copy.contains("`[[#^old]]`"));
            assert!(copy.contains(code));
            ids.push(id.to_owned());
        }
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn relocation_copy_remaps_alias_only_identity_references() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let source_id = crate::identity::new_document_id();
        let formal_alias = crate::identity::new_block_id();
        let destination = crate::identity::new_document_id();
        let source = format!(
            "body {{aero-aliases=\"old {source_id}#scoped {formal_alias}\"}}\n\n![[#^old]]\n\n[[id:{source_id}#^old|legacy]]\n\n![[id:{source_id}#^scoped]]\n\n![[id:{source_id}#^{formal_alias}]]\n"
        );
        std::fs::write(
            dir.path().join("Source.md"),
            format!("---\naero:\n  id: {source_id}\n---\n\n{source}"),
        )
        .unwrap();
        let origin = Payload {
            version: 1,
            vault: vault.id().into(),
            path: "Source.md".into(),
            document: Some(source_id),
            cut: None,
        };
        let plan = Relocation::prepare(
            &vault,
            &source,
            &origin,
            &VaultPath::new("Dest.md").unwrap(),
            &destination,
            false,
        )
        .unwrap();
        let doc = Document::from_text(&plan.source);
        let id = doc.blocks().roots[0].persisted_id.as_deref().unwrap();
        assert_ne!(id, formal_alias);
        assert!(!plan.source.contains("aero-aliases"));
        let spans = crate::links::scan_for_rename(&plan.source);
        assert_eq!(spans.len(), 4);
        assert!(
            spans
                .iter()
                .all(|span| span.target == format!("id:{destination}")
                    && span.fragment == Some(format!("^{id}"))),
            "{}",
            plan.source
        );
    }

    #[test]
    fn standalone_copy_scopes_legacy_aliases_only_to_known_owners() {
        let source_id = crate::identity::new_document_id();
        let previous_id = crate::identity::new_document_id();
        let outside_id = crate::identity::new_document_id();
        let main_id = crate::identity::new_block_id();
        let destination = crate::identity::new_document_id();
        let origin = Payload {
            version: 1,
            vault: "v".into(),
            path: "Source.md".into(),
            document: Some(source_id.clone()),
            cut: None,
        };
        let source = format!(
            "body {{aero-aliases=\"{source_id}#old {previous_id}#old\"}} ^{main_id}\n\n![[#^old]]\n\n[[#^old|local]]\n\n[[Source#^old|path]]\n\n![[Source.md#^old]]\n\n[[id:{source_id}#^old|identity]]\n\n![[id:{previous_id}#^old]]\n\n[[Else#^old|external]]\n\n![[id:{outside_id}#^old]]\n"
        );
        let copy = copy_tree(&source, &origin, &destination).unwrap();
        let doc = Document::from_text(&copy);
        let new_id = doc.blocks().roots[0].persisted_id.as_deref().unwrap();
        assert_ne!(new_id, main_id);
        assert!(!copy.contains("aero-aliases"));
        let internal = crate::links::scan_for_rename(&copy)
            .into_iter()
            .filter(|span| {
                span.target == format!("id:{destination}") && span.fragment == Some(format!("^{new_id}"))
            })
            .count();
        assert_eq!(internal, 6, "{copy}");
        assert!(copy.contains("[[Else#^old|external]]"));
        assert!(copy.contains(&format!("![[id:{outside_id}#^old]]")));

        let mut unknown_origin = origin;
        unknown_origin.document = None;
        let copy = copy_tree(&source, &unknown_origin, &destination).unwrap();
        assert!(copy.contains("![[#^old]]"), "a short ID must not be guessed globally: {copy}");
        assert!(copy.contains("[[Source#^old|path]]"));
    }

    #[test]
    fn relocation_copy_resolves_scoped_alias_references_before_remapping() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let source_id = crate::identity::new_document_id();
        let previous_id = crate::identity::new_document_id();
        let outside_id = crate::identity::new_document_id();
        let main_id = crate::identity::new_block_id();
        let destination = crate::identity::new_document_id();
        let source = format!(
            "body {{aero-aliases=\"{source_id}#old {previous_id}#old\" custom=keep}} ^{main_id}\n\n![[#^old]]\n\n[[#^old|local]]\n\n[[Source#^old|path]]\n\n![[Source.md#^old]]\n\n[[id:{source_id}#^old|identity]]\n\n![[id:{previous_id}#^old]]\n\n[[Else#^old|external]]\n\n![[id:{outside_id}#^old]]\n\n![[id:{source_id}#^uncopied]]\n"
        );
        for (path, id, body) in [
            ("Source.md", &source_id, format!("{source}\nuncopied ^uncopied\n")),
            ("Previous.md", &previous_id, "remaining\n".into()),
            ("Else.md", &outside_id, "external ^old\n".into()),
        ] {
            std::fs::write(dir.path().join(path), format!("---\naero:\n  id: {id}\n---\n\n{body}")).unwrap();
        }
        let origin = Payload {
            version: 1,
            vault: vault.id().into(),
            path: "Source.md".into(),
            document: Some(source_id.clone()),
            cut: None,
        };
        let mut ids = Vec::new();
        for _ in 0..2 {
            let plan = Relocation::prepare(
                &vault,
                &source,
                &origin,
                &VaultPath::new("Dest.md").unwrap(),
                &destination,
                false,
            )
            .unwrap();
            let doc = Document::from_text(&plan.source);
            let new_id = doc.blocks().roots[0].persisted_id.as_deref().unwrap();
            assert_ne!(new_id, main_id);
            assert!(!plan.source.contains("aero-aliases"));
            assert!(plan.source.contains("custom=keep"));
            let internal = crate::links::scan_for_rename(&plan.source)
                .into_iter()
                .filter(|span| {
                    span.target == format!("id:{destination}") && span.fragment == Some(format!("^{new_id}"))
                })
                .count();
            assert_eq!(internal, 6, "{}", plan.source);
            assert!(plan.source.contains("[[Else.md#^old|external]]"));
            assert!(plan.source.contains(&format!("![[id:{outside_id}#^old]]")));
            assert!(plan.source.contains(&format!("![[id:{source_id}#^uncopied]]")));
            assert!(plan.notes.iter().all(|note| note.before == note.after));
            ids.push(new_id.to_owned());
        }
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn copied_reference_to_uncopied_canonical_target_never_falls_back_to_a_short_id() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let source_id = crate::identity::new_document_id();
        let historical_id = crate::identity::new_document_id();
        let destination_id = crate::identity::new_document_id();
        let reference = format!("![[id:{historical_id}#^old]]");
        let copied = format!("copied ^old\n\n{reference}\n");
        std::fs::write(dir.path().join("Source.md"), format!(
            "---\naero:\n  id: {source_id}\n---\n\n{copied}\nuncopied {{aero-aliases=\"{historical_id}#old\"}} ^other\n"
        )).unwrap();
        let origin = Payload {
            version: 1, vault: vault.id().into(), path: "Source.md".into(),
            document: Some(source_id), cut: None,
        };
        let plan = Relocation::prepare(&vault, &copied, &origin, &VaultPath::new("Dest.md").unwrap(), &destination_id, false).unwrap();
        assert!(plan.source.contains(&reference), "{}", plan.source);
        let result = Document::from_text(&plan.source);
        assert_ne!(result.blocks().roots[0].persisted_id.as_deref(), Some("old"));
    }

    #[test]
    fn relocation_copy_resolves_aliases_in_the_original_relative_context() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let source_id = crate::identity::new_document_id();
        let destination = crate::identity::new_document_id();
        let source = format!(
            "body {{aero-aliases=\"{source_id}#old\"}} ^{}\n\n![[#^old]]\n\n[[Source#^old|relative]]\n",
            crate::identity::new_block_id()
        );
        std::fs::create_dir_all(dir.path().join("from/from")).unwrap();
        std::fs::write(
            dir.path().join("from/Source.md"),
            format!("---\naero:\n  id: {source_id}\n---\n\n{source}"),
        )
        .unwrap();
        std::fs::write(dir.path().join("from/from/Source.md"), "different target ^old\n").unwrap();
        let origin = Payload {
            version: 1,
            vault: vault.id().into(),
            path: "from/Source.md".into(),
            document: Some(source_id),
            cut: None,
        };
        let plan = Relocation::prepare(
            &vault,
            &source,
            &origin,
            &VaultPath::new("Dest.md").unwrap(),
            &destination,
            false,
        )
        .unwrap();
        let doc = Document::from_text(&plan.source);
        let new_id = doc.blocks().roots[0].persisted_id.as_deref().unwrap();
        assert!(plan.source.contains(&format!("![[id:{destination}#^{new_id}]]")), "{}", plan.source);
        assert!(plan.source.contains(&format!("[[id:{destination}#^{new_id}|relative]]")), "{}", plan.source);
    }

    #[test]
    fn relocation_preserves_external_identity_targets_for_copies_and_moves() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let source_id = crate::identity::new_document_id();
        let target_id = crate::identity::new_document_id();
        let block_id = crate::identity::new_block_id();
        std::fs::write(
            dir.path().join("Source.md"),
            format!("---\naero:\n  id: {source_id}\n---\n\nremaining\n"),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("Target.md"),
            format!("---\naero:\n  id: {target_id}\n---\n\ntarget ^{block_id}\n"),
        )
        .unwrap();
        let origin = Payload {
            version: 1,
            vault: vault.id().into(),
            path: "Source.md".into(),
            document: Some(source_id),
            cut: None,
        };
        let source = format!(
            "![[id:{target_id}#^{block_id}]]\n\n[[id:{target_id}#^{block_id}|external]]\n\n[[id:{target_id}|document]]\n"
        );
        for moving in [false, true] {
            let plan = Relocation::prepare(
                &vault,
                &source,
                &origin,
                &VaultPath::new("Dest.md").unwrap(),
                &crate::identity::new_document_id(),
                moving,
            )
            .unwrap();
            assert_eq!(plan.source, source, "moving={moving}");
        }
    }

    #[test]
    fn copied_subtree_remaps_internal_identity_but_keeps_uncopied_source_identity() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let source_id = crate::identity::new_document_id();
        let destination_id = crate::identity::new_document_id();
        let copied_id = crate::identity::new_block_id();
        let external_id = crate::identity::new_block_id();
        let source = format!(
            "copied ^{copied_id}\n\n![[id:{source_id}#^{copied_id}]]\n\n[[#^{copied_id}|local]]\n\n![[id:{source_id}#^{external_id}]]\n"
        );
        std::fs::write(
            dir.path().join("Source.md"),
            format!("---\naero:\n  id: {source_id}\n---\n\n{source}\nuncopied ^{external_id}\n"),
        )
        .unwrap();
        let origin = Payload {
            version: 1,
            vault: vault.id().into(),
            path: "Source.md".into(),
            document: Some(source_id.clone()),
            cut: None,
        };
        let plan = Relocation::prepare(
            &vault,
            &source,
            &origin,
            &VaultPath::new("Dest.md").unwrap(),
            &destination_id,
            false,
        )
        .unwrap();
        let doc = Document::from_text(&plan.source);
        let new_id = doc.blocks().roots[0].persisted_id.as_deref().unwrap();
        assert_ne!(new_id, copied_id);
        assert!(plan.source.contains(&format!("![[id:{destination_id}#^{new_id}]]")));
        assert!(plan.source.contains(&format!("[[id:{destination_id}#^{new_id}|local]]")));
        assert!(plan.source.contains(&format!("![[id:{source_id}#^{external_id}]]")), "{}", plan.source);
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
    fn relocation_move_maintains_scoped_alias_references_before_rebasing() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let source_id = format!("d-{:026}", 1);
        let previous_id = format!("d-{:026}", 2);
        let outside_id = format!("d-{:026}", 3);
        let destination_id = format!("d-{:026}", 4);
        let block_id = format!("b-{:026}", 1);
        let address = format!("id:{destination_id}#^{block_id}");
        let body = format!(
            "body **unchanged** {{aero-aliases=\"{source_id}#old {previous_id}#old\" custom=keep .red}} ^{block_id}\n\n"
        );
        let references = format!(
            "![[#^old]]\n\n[[Source#^old|relative]]\n\n![[Source.md#^old]]\n\n[[id:{source_id}#^old|identity]]\n\n[[Previous#^old|historical path]]\n\n![[id:{previous_id}#^old]]\n\n"
        );
        let maintained = format!(
            "![[{address}]]\n\n[[{address}|relative]]\n\n![[{address}]]\n\n[[{address}|identity]]\n\n[[{address}|historical path]]\n\n![[{address}]]\n\n"
        );
        let unchanged = format!(
            "[[Else.md#^old|external]]\n\n![[id:{outside_id}#^old]]\n\n[[from/from/Source.md#^old|shadow]]\n\n![[id:{source_id}#^stay]]\n\n`![[#^old]]`\n\n```text\n[[Source#^old]] {{custom=literal}}\n```\n"
        );
        let source = format!("{body}{references}{unchanged}\n![a](Source.assets/a.png)\n");
        let expected = format!("{body}{maintained}{unchanged}\n![a](../from/Source.assets/a.png)\n");
        let note =
            |id: &str, text: &str| format!("---\naero:\n  id: {id}\nextra: keep # comment\n---\n\n{text}");
        // Application cuts remove the subtree before prepare snapshots managed notes.
        let remaining = note(&source_id, "remaining ^stay\n\n![[#^old]]\n");
        let previous = note(&previous_id, "previous\n\n![[#^old]]\n");
        let back = format!("{references}{unchanged}");
        let outside = note(&outside_id, "external ^old\n\n![[#^old]]\n");
        let shadow = "different target ^old\n\n[[#^old|unrelated]]\n".to_owned();
        let destination = note(&destination_id, "destination\n");
        let observations = [
            ("from/Source.md", remaining, note(&source_id, &format!("remaining ^stay\n\n![[{address}]]\n"))),
            ("Previous.md", previous, note(&previous_id, &format!("previous\n\n![[{address}]]\n"))),
            // A local reference in Back is not a reference to the moved source.
            ("from/Back.md", back, format!("{}{}", maintained.replacen(&address, "#^old", 1), unchanged)),
            ("Else.md", outside.clone(), outside),
            ("from/from/Source.md", shadow.clone(), shadow),
            ("to/Dest.md", destination.clone(), destination),
        ];
        std::fs::create_dir_all(dir.path().join("from/from")).unwrap();
        std::fs::create_dir_all(dir.path().join("from/Source.assets")).unwrap();
        std::fs::create_dir_all(dir.path().join("to")).unwrap();
        std::fs::write(dir.path().join("from/Source.assets/a.png"), [0, 255, 8, 0]).unwrap();
        for (path, before, _) in &observations {
            std::fs::write(dir.path().join(path), before).unwrap();
        }
        let payload = Payload {
            version: 1,
            vault: vault.id().into(),
            path: "from/Source.md".into(),
            document: Some(source_id),
            cut: Some("token".into()),
        };
        let plan = Relocation::prepare(
            &vault,
            &source,
            &payload,
            &VaultPath::new("to/Dest.md").unwrap(),
            &destination_id,
            true,
        )
        .unwrap();
        assert_eq!(plan.source, expected);
        assert_eq!(plan.notes.len(), observations.len());
        for (path, before, after) in observations {
            let change = plan.notes.iter().find(|n| n.path.as_str() == path).unwrap();
            assert_eq!(change.before.as_deref(), Some(before.as_bytes()), "{path}");
            assert_eq!(change.after.as_deref(), Some(after.as_bytes()), "{path}");
            assert_eq!(std::fs::read(dir.path().join(path)).unwrap(), before.as_bytes(), "{path}");
        }
        assert!(plan.resources.is_empty());
        assert_eq!(std::fs::read(dir.path().join("from/Source.assets/a.png")).unwrap(), [0, 255, 8, 0]);
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
