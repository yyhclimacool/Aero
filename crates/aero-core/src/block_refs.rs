//! 块引用候选与按需 ID 写入。写入前重解析验证块结构，磁盘提交校验菜单快照。

use std::collections::HashSet;
use std::ops::Range;

use crate::{Block, BlockId, BlockKind, Document, Edit, Vault, VaultPath};

#[derive(Clone, Debug)]
pub struct Candidate {
    pub block: BlockId,
    pub range: Range<usize>,
    pub kind: BlockKind,
    pub excerpt: String,
    pub persisted_id: Option<String>,
}

pub fn candidates(doc: &Document) -> Vec<Candidate> {
    doc.blocks()
        .iter()
        .filter_map(|(_, b)| {
            if matches!(b.kind, BlockKind::FrontMatter | BlockKind::Marker | BlockKind::Raw) {
                return None;
            }
            let source = doc.slice(doc.content_range(b)).to_string();
            let plain = crate::index::plain_text(&source, &b.kind);
            let excerpt = plain.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(60).collect();
            Some(Candidate {
                block: b.id,
                range: doc.range_of(b),
                kind: b.kind.clone(),
                excerpt,
                persisted_id: b.persisted_id.as_deref().map(str::to_owned),
            })
        })
        .collect()
}

/// Marker 节点也携带前一个块的 ID；不能将它误算成重复定义。
pub fn resolve(doc: &Document, id: &str) -> Result<BlockId, String> {
    let scoped =
        crate::identity::read(doc).ok().and_then(|m| m.id).map(|document| format!("{document}#{id}"));
    let mut matches = doc.blocks().iter().filter(|(_, b)| {
        b.kind != BlockKind::Marker
            && (owns_id(b, id) || scoped.as_ref().is_some_and(|id| aliases(b).any(|alias| alias == id)))
    });
    let Some((_, first)) = matches.next() else { return Err(format!("找不到块 ID：^{id}")) };
    if matches.next().is_some() {
        return Err(format!("块 ID 重复：^{id}，请先消歧"));
    }
    Ok(first.id)
}

pub fn aliases(block: &Block) -> impl Iterator<Item = &str> {
    block
        .attrs
        .pairs
        .iter()
        .filter(|(key, _)| key == "aero-aliases")
        .flat_map(|(_, value)| value.split_whitespace())
}

pub fn owns_id(block: &Block, id: &str) -> bool {
    block.persisted_id.as_deref() == Some(id) || aliases(block).any(|alias| alias == id)
}

pub struct Assignment {
    pub id: String,
    pub edit: Option<Edit>,
}

pub struct PersistentAssignment {
    pub document: String,
    pub id: String,
    pub edits: Vec<Edit>,
}

/// A portable reference upgrades a legacy short ID only at this explicit boundary.
pub fn permanent(doc: &Document, block: BlockId) -> Result<PersistentAssignment, String> {
    let (document, mut edits) = crate::identity::ensure(doc)?;
    let b = doc.blocks().find(block).ok_or("目标块已不存在")?;
    for alias in aliases(b) {
        let scoped = format!("{document}#");
        resolve(doc, alias.strip_prefix(&scoped).unwrap_or(alias))?;
    }
    let id = match b.persisted_id.as_deref() {
        Some(old) if crate::identity::is_block_id(old) => {
            resolve(doc, old)?;
            old.to_owned()
        }
        Some(old) => {
            resolve(doc, old)?;
            let id = crate::identity::new_block_id();
            let mut attrs = b.attrs.clone();
            let mut aliases: Vec<_> = aliases(b).map(str::to_owned).collect();
            aliases.push(format!("{document}#{old}"));
            aliases.sort();
            aliases.dedup();
            crate::properties::set(&mut attrs, "aero-aliases", Some(&aliases.join(" ")));
            edits.extend(crate::properties::edit(doc, block, &attrs, Some(&id))?);
            id
        }
        None if matches!(b.kind, BlockKind::ListItem { .. }) => {
            let id = crate::identity::new_block_id();
            edits.extend(crate::properties::edit(doc, block, &b.attrs, Some(&id))?);
            id
        }
        None => {
            let assignment = prepare(doc, block)?;
            edits.extend(assignment.edit);
            assignment.id
        }
    };
    edits.sort_by_key(|e| std::cmp::Reverse(e.range.start));
    Ok(PersistentAssignment { document, id, edits })
}

/// Retain the resolver's observations for transaction validation after checking the proposed identity.
pub fn permanent_in(
    doc: &Document,
    block: BlockId,
    path: &VaultPath,
    resolver: &crate::links::Resolver,
) -> Result<PersistentAssignment, String> {
    let assignment = permanent(doc, block)?;
    let mut probe = doc.clone();
    probe.edit_group(assignment.edits.clone()).map_err(|e| e.to_string())?;
    probe.reparse();
    let mut resolver = resolver.clone();
    resolver.register_identity(path, &probe);
    let target = format!("id:{}", assignment.document);
    if !matches!(resolver.resolve(&target, None, path.parent()), crate::links::Resolution::Resolved { path: p, .. } if p == *path)
    {
        return Err("来源文档身份重复，请先解决冲突".into());
    }
    let assigned = resolve(&probe, &assignment.id)?;
    let source = probe.blocks().find(assigned).ok_or("目标块已不存在")?;
    for id in std::iter::once(assignment.id.as_str()).chain(aliases(source)) {
        let (document, id) = id.split_once('#').unwrap_or((&assignment.document, id));
        let target = format!("id:{document}");
        let fragment = format!("^{id}");
        if !matches!(
            resolver.resolve(&target, Some(&fragment), path.parent()),
            crate::links::Resolution::Resolved { path: p, fragment: Some(f) }
                if p == *path && f == format!("^{}", assignment.id)
        ) {
            return Err(format!("来源块身份重复或无法唯一解析：{target}#{fragment}，请先解决冲突"));
        }
    }
    Ok(assignment)
}

/// 仅准备一次插入，不修改文档。已有且唯一的 ID 原样复用。
pub fn prepare(doc: &Document, block: BlockId) -> Result<Assignment, String> {
    let b = doc
        .blocks()
        .iter()
        .find(|(_, b)| b.id == block)
        .map(|(_, b)| b)
        .ok_or("目标块已变化，请重新打开补全")?;
    if let Some(id) = b.persisted_id.as_deref() {
        resolve(doc, id)?;
        return Ok(Assignment { id: id.into(), edit: None });
    }
    let ids: HashSet<_> = doc.blocks().iter().filter_map(|(_, b)| b.persisted_id.as_deref()).collect();
    let id = loop {
        let id = crate::identity::new_block_id();
        if !ids.contains(id.as_str()) {
            break id;
        }
    };
    let source = doc.to_string();
    let range = doc.range_of(b);
    let edit = if b.kind == BlockKind::Paragraph {
        let at = range.start + source[range.clone()].trim_end().len();
        Edit::insert(at, format!(" ^{id}"))
    } else if let Some(marker) = following_marker(&doc.blocks().roots, block) {
        let r = doc.range_of(marker);
        Edit::insert(r.start + source[r.clone()].trim_end().len(), format!(" ^{id}"))
    } else {
        // 续行保持所在引用 / 列表的上下文；根列表本身的前缀为空。
        let line_start = source[..range.start].rfind('\n').map_or(0, |i| i + 1);
        let mut prefix = continuation_prefix(&source[line_start..range.start]);
        if b.kind == BlockKind::CodeBlock {
            // sourcepos 跳过了缩进代码自己的四列；独立标记应退回其容器层级。
            for _ in 0..4 {
                match prefix.chars().last() {
                    Some(' ') => {
                        prefix.pop();
                    }
                    Some('\t') => {
                        prefix.pop();
                        break;
                    }
                    _ => break,
                }
            }
        }
        let at = range.start + source[range.clone()].trim_end_matches('\n').len();
        Edit::insert(at, format!("\n{prefix}\n{prefix}^{id}\n{prefix}"))
    };
    let mut probe = Document::from_text(&source);
    probe.edit(edit.clone()).map_err(|e| e.to_string())?;
    probe.reparse();
    let assigned = resolve(&probe, &id)?;
    let before: Vec<_> = shape(doc, None);
    let after = shape(&probe, Some(&id));
    // 增加标记不能拆分 / 吞掉原块或把 ID 挂到另一个块。
    let ordinal =
        doc.blocks().iter().filter(|(_, b)| b.kind != BlockKind::Marker).position(|(_, b)| b.id == block);
    let assigned_ordinal = probe
        .blocks()
        .iter()
        .filter(|(_, b)| b.kind != BlockKind::Marker)
        .position(|(_, b)| b.id == assigned);
    if before != after || ordinal != assigned_ordinal {
        return Err("无法在此块安全写入 ID，请先补全 Markdown 块结构".into());
    }
    Ok(Assignment { id, edit: Some(edit) })
}

fn shape(doc: &Document, added: Option<&str>) -> Vec<(usize, BlockKind, crate::Attrs, Option<String>)> {
    doc.blocks()
        .iter()
        .filter(|(_, b)| b.kind != BlockKind::Marker)
        .map(|(depth, b)| {
            let id = b.persisted_id.as_deref().filter(|id| Some(*id) != added).map(str::to_owned);
            (depth, b.kind.clone(), b.attrs.clone(), id)
        })
        .collect()
}

fn following_marker(blocks: &[Block], id: BlockId) -> Option<&Block> {
    for (i, b) in blocks.iter().enumerate() {
        if b.id == id {
            return blocks.get(i + 1).filter(|m| m.kind == BlockKind::Marker && m.persisted_id.is_none());
        }
        if let Some(found) = following_marker(&b.children, id) {
            return Some(found);
        }
    }
    None
}

fn continuation_prefix(prefix: &str) -> String {
    prefix.chars().map(|c| if c == '>' || c == '\t' { c } else { ' ' }).collect()
}

/// 比较快照后写入单个 ID。原文件的 BOM、混合行尾和其余字节保持不变。
/// 与普通保存一致：校验后原子替换，不是跨进程文件锁。
pub fn assign_on_disk(
    vault: &Vault,
    path: &VaultPath,
    expected_hash: u64,
    block: BlockId,
) -> Result<(String, bool), String> {
    let (bytes, stamp) = vault.read_bytes(path).map_err(|e| e.to_string())?;
    if stamp.hash != expected_hash {
        return Err("目标笔记已变化，请重新打开补全".into());
    }
    let doc = Document::from_bytes(&bytes).map_err(|e| e.to_string())?;
    let assignment = prepare(&doc, block)?;
    let Some(edit) = assignment.edit else { return Ok((assignment.id, false)) };
    let after = edit_bytes(&bytes, &edit)?;
    // 再校验一次，覆盖准备 / 验证期间可检测到的外部改动。
    if vault.read_bytes(path).map_err(|e| e.to_string())?.1.hash != stamp.hash {
        return Err("目标笔记已变化，请重新打开补全".into());
    }
    vault.write_bytes(path, &after).map_err(|e| e.to_string())?;
    Ok((assignment.id, true))
}

/// Preserve untouched file bytes when applying a normalized document edit.
pub fn edit_bytes(bytes: &[u8], edit: &Edit) -> Result<Vec<u8>, String> {
    let source = crate::text::SourceText::decode(bytes).map_err(|e| e.to_string())?;
    if source.text.get(edit.range.clone()).is_none() {
        return Err("块位置超出文件范围或字符边界".into());
    }
    let bom = if source.bom { 3 } else { 0 };
    let mut raw = bom;
    let mut normalized = 0;
    let mut range = 0..0;
    for (offset, position) in [(edit.range.start, &mut range.start), (edit.range.end, &mut range.end)] {
        while normalized < offset {
            if bytes[raw..].starts_with(b"\r\n") {
                raw += 1;
            }
            raw += 1;
            normalized += 1;
        }
        *position = raw;
    }
    let text = crate::text::SourceText::encode(&edit.text, false, source.line_ending);
    let mut after = bytes.to_vec();
    after.splice(range, text);
    Ok(after)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assign(source: &str, kind: &str) -> (Document, String) {
        let mut doc = Document::from_text(source);
        let candidate = candidates(&doc).into_iter().find(|c| c.kind.tag() == kind).unwrap();
        let plan = prepare(&doc, candidate.block).unwrap_or_else(|e| panic!("{kind}: {source:?}: {e}"));
        assert!(crate::identity::is_block_id(&plan.id));
        doc.edit(plan.edit.unwrap()).unwrap();
        doc.reparse();
        let block = resolve(&doc, &plan.id).unwrap();
        assert_eq!(doc.blocks().iter().find(|(_, b)| b.id == block).unwrap().1.kind.tag(), kind);
        assert!(prepare(&doc, block).unwrap().edit.is_none());
        (doc, plan.id)
    }

    #[test]
    fn permanent_in_preserves_legacy_alias_without_confusing_other_document_short_ids() {
        let document = crate::identity::new_document_id();
        let other_document = crate::identity::new_document_id();
        let source = VaultPath::new("source.md").unwrap();
        let other = VaultPath::new("other.md").unwrap();
        let mut doc = Document::from_text(&format!("---\naero:\n  id: {document}\n---\n\nsource ^old\n"));
        let other_doc =
            Document::from_text(&format!("---\naero:\n  id: {other_document}\n---\n\nother ^old\n"));
        let mut resolver = crate::links::Resolver::new();
        resolver.register_identity(&source, &doc);
        resolver.register_identity(&other, &other_doc);
        let assignment = permanent_in(&doc, candidates(&doc)[0].block, &source, &resolver).unwrap();
        assert_eq!(assignment.document, document);
        assert!(crate::identity::is_block_id(&assignment.id));
        doc.edit_group(assignment.edits).unwrap();
        doc.reparse();
        assert!(doc.to_string().contains(&format!("{document}#old")));
        resolver.register_identity(&source, &doc);
        assert_eq!(
            resolver.resolve(&format!("id:{document}"), Some("^old"), ""),
            crate::links::Resolution::Resolved {
                path: source.clone(),
                fragment: Some(format!("^{}", assignment.id))
            },
        );
        let reused = permanent_in(&doc, resolve(&doc, &assignment.id).unwrap(), &source, &resolver).unwrap();
        assert_eq!(reused.id, assignment.id);
        assert!(reused.edits.is_empty());
    }

    #[test]
    fn permanent_in_rejects_alias_only_owners_in_other_documents() {
        let document = crate::identity::new_document_id();
        let other_document = crate::identity::new_document_id();
        let block = crate::identity::new_block_id();
        let source = VaultPath::new("source.md").unwrap();
        let other = VaultPath::new("other.md").unwrap();
        let doc = Document::from_text(&format!("---\naero:\n  id: {document}\n---\n\nsource ^{block}\n"));
        let other_doc = Document::from_text(&format!(
            "---\naero:\n  id: {other_document}\n---\n\nother {{aero-aliases=\"{block}\"}}\n"
        ));
        let mut resolver = crate::links::Resolver::new();
        resolver.register_identity(&source, &doc);
        resolver.register_identity(&other, &other_doc);
        assert!(permanent_in(&doc, candidates(&doc)[0].block, &source, &resolver).is_err());
    }

    #[test]
    fn permanent_rejects_ambiguous_source_aliases_before_assigning_identity() {
        let document = crate::identity::new_document_id();
        let block = crate::identity::new_block_id();
        let other = crate::identity::new_block_id();
        for source in [
            format!("source {{aero-aliases=\"old\"}} ^{block}\n\nother ^old\n"),
            format!("source {{aero-aliases=\"{document}#old\"}} ^{block}\n\nother ^old\n"),
            format!("source {{aero-aliases=\"old\"}}\n\nother {{aero-aliases=\"old\"}} ^{other}\n"),
        ] {
            let doc = Document::from_text(&format!("---\naero:\n  id: {document}\n---\n\n{source}"));
            let block = candidates(&doc)[0].block;
            assert!(permanent(&doc, block).is_err(), "must reject ambiguous alias: {source}");
        }
    }

    #[test]
    fn block_ids_preserve_paragraph_attributes_and_nested_context() {
        for source in ["正文", "正文 {.red}\n", "- 正文\n- 其他\n", "> [!note]- 提示\n> 正文\n>\n> 其他\n"]
        {
            let (doc, id) = assign(source, "paragraph");
            assert!(doc.to_string().contains(&format!(" ^{id}")));
        }
    }

    #[test]
    fn block_ids_standalone_markers_preserve_multiline_blocks() {
        for (source, kind) in [
            ("# Heading\n\nbody\n", "heading"),
            ("- one\n- two\n\nbody\n", "list"),
            ("```rs\nlet x = 1;\n```\n\nbody\n", "code_fence"),
            ("    code\n    code2\n\nbody\n", "code_block"),
            (">     code\n>     code2\n>\n> after\n", "code_block"),
            ("| A |\n| --- |\n| 中文 |\n\nbody\n", "table"),
            ("> 引用\n> 两行\n\nbody\n", "block_quote"),
            ("> [!note]- 提示\n> 正文\n\nbody\n", "callout"),
            ("$$\nx+y\n$$\n\nbody\n", "math_block"),
            ("> ```rs\n> x\n> ```\n>\n> after\n", "code_fence"),
            ("- item\n\n  ```rs\n  x\n  ```\n\n  after\n", "code_fence"),
            ("# Heading\n\n{.red}\n\nbody\n", "heading"),
        ] {
            assign(source, kind);
        }
    }

    #[test]
    fn block_ids_reuse_existing_and_reject_duplicates_or_unclosed_fences() {
        let doc = Document::from_text("text ^Ab-12\n\n```rs\nx\n```\n\n^code-id\n");
        for id in ["Ab-12", "code-id"] {
            let block = resolve(&doc, id).unwrap();
            let plan = prepare(&doc, block).unwrap();
            assert_eq!(plan.id, id);
            assert!(plan.edit.is_none());
        }
        let doc = Document::from_text("first ^dup\n\nsecond ^dup\n");
        assert!(resolve(&doc, "dup").unwrap_err().contains("重复"));
        assert!(prepare(&doc, candidates(&doc)[0].block).is_err());
        let doc = Document::from_text("```rs\nunclosed\n");
        assert!(prepare(&doc, candidates(&doc)[0].block).is_err());
        assert_eq!(doc.to_string(), "```rs\nunclosed\n");
    }

    #[test]
    fn block_candidates_keep_unicode_excerpt_and_exclude_metadata() {
        let doc =
            Document::from_text(&format!("---\ntags: [test]\n---\n\n{} ^existing\n", "中文😀".repeat(30)));
        let list = candidates(&doc);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].excerpt.chars().count(), 60);
        assert_eq!(list[0].persisted_id.as_deref(), Some("existing"));
    }

    #[test]
    fn identity_byte_edits_preserve_unedited_mixed_endings() {
        let bytes = "\u{feff}front\r\n中文 ^old\n\nlast\r\n".as_bytes();
        let after = edit_bytes(bytes, &Edit::replace(14..17, "new")).unwrap();
        assert_eq!(after, "\u{feff}front\r\n中文 ^new\n\nlast\r\n".as_bytes());
    }

    #[test]
    fn block_disk_write_preserves_bom_mixed_line_endings_and_refuses_stale_files() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let path = VaultPath::new("note.md").unwrap();
        let bytes = "\u{feff}# Title\r\n\r\n中文😀\n\nlast\r\n".as_bytes();
        vault.write_bytes(&path, bytes).unwrap();
        let (doc, stamp) = vault.read(&path).unwrap();
        let block = candidates(&doc).into_iter().find(|c| c.excerpt == "中文😀").unwrap().block;
        let (id, changed) = assign_on_disk(&vault, &path, stamp.hash, block).unwrap();
        assert!(changed);
        let expected = format!("\u{feff}# Title\r\n\r\n中文😀 ^{id}\n\nlast\r\n");
        assert_eq!(vault.read_bytes(&path).unwrap().0, expected.as_bytes());
        assert!(assign_on_disk(&vault, &path, stamp.hash, block).unwrap_err().contains("变化"));
        let (doc, stamp) = vault.read(&path).unwrap();
        let block = resolve(&doc, &id).unwrap();
        assert_eq!(assign_on_disk(&vault, &path, stamp.hash, block).unwrap(), (id, false));
        assert_eq!(vault.read_bytes(&path).unwrap().0, expected.as_bytes());
        std::fs::remove_file(vault.fs_path(&path)).unwrap();
        assert!(assign_on_disk(&vault, &path, stamp.hash, block).is_err());
        assert!(!vault.exists(&path));
    }
}
