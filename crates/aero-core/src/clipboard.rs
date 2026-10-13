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
        for path in vault.notes().map_err(|e| e.to_string())? {
            let (bytes, _) = vault.read_bytes(&path).map_err(|e| e.to_string())?;
            let doc = Document::from_bytes(&bytes).map_err(|e| e.to_string())?;
            crate::identity::read(&doc)?;
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
        };
        result.source = result.rebase(vault, destination)?;
        if moving {
            result.source = result.rewrite(&result.source, &result.origin);
            for i in 0..result.notes.len() {
                let note = &result.notes[i];
                let doc = Document::from_bytes(note.after.as_ref().unwrap()).map_err(|e| e.to_string())?;
                let text = result.rewrite(&doc.to_string(), &note.path);
                let mut rewritten = doc;
                rewritten.edit(Edit::replace(0..rewritten.len(), text)).map_err(|e| e.to_string())?;
                result.notes[i].after = Some(rewritten.to_bytes());
            }
        } else {
            let destination = result.destination.clone();
            result.destination = origin.document.clone().unwrap_or_else(|| document.into());
            result.source = result.rewrite(&result.source, &result.origin);
            let mut copy_origin = origin.clone();
            copy_origin.document = Some(result.destination.clone());
            result.destination = destination;
            result.source = copy_tree(&result.source, &copy_origin, document)?;
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
    let mut doc = Document::from_text(source);
    let mut mapping = HashMap::new();
    let mut edits = Vec::new();
    for (_, block) in doc.blocks().iter().filter(|(_, b)| b.kind != crate::BlockKind::Marker) {
        let Some(old) = block.persisted_id.as_deref() else { continue };
        if mapping.contains_key(old) {
            return Err(format!("复制内容含重复块 ID：{old}"));
        }
        let new = crate::identity::new_block_id();
        mapping.insert(old.to_owned(), new.clone());
        for alias in crate::block_refs::aliases(block) {
            if mapping.insert(alias.to_owned(), new.clone()).is_some_and(|previous| previous != new) {
                return Err(format!("复制内容含歧义块身份：{alias}"));
            }
        }
        let mut attrs = block.attrs.clone();
        crate::properties::set(&mut attrs, "aero-aliases", None);
        if let Some(edit) = crate::properties::edit(&doc, block.id, &attrs, Some(&new))? {
            edits.push(edit);
        }
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.range.start));
    doc.edit_group(edits).map_err(|e| e.to_string())?;
    doc.reparse();
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
        let local = span.target.is_empty()
            || origin.document.as_ref().is_some_and(|id| span.target == format!("id:{id}"))
            || span.target == origin.path
            || span.target.strip_suffix(".md") == origin.path.strip_suffix(".md");
        if !local {
            continue;
        }
        let Some(new) = mapping.get(old) else { continue };
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
