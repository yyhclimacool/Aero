//! 块引用候选与按需 ID 写入。写入前重解析验证块结构，磁盘提交校验菜单快照。

use std::collections::HashSet;
use std::ops::Range;

use crate::links::Resolver;
use crate::transaction::FileChange;
use crate::{Block, BlockId, BlockKind, Document, Edit, Vault, VaultPath};

/// Read managed notes from disk for identity validation and transactional observations.
///
/// # Errors
///
/// Returns an error if a managed note or its identity metadata cannot be read.
pub fn vault_snapshot(vault: &Vault) -> Result<(Resolver, Vec<FileChange>), String> {
    let mut resolver = Resolver::new();
    let mut notes = Vec::new();
    for path in vault.notes().map_err(|e| e.to_string())? {
        let (bytes, _) = vault.read_bytes(&path).map_err(|e| format!("{path}：{e}"))?;
        let doc = Document::from_bytes(&bytes).map_err(|e| format!("{path}：{e}"))?;
        crate::identity::read(&doc).map_err(|e| format!("{path}：{e}"))?;
        resolver.insert(path.clone(), Some(&doc.title_or(path.stem())), &doc.front_matter_aliases());
        resolver.register_identity(&path, &doc);
        notes.push(FileChange { path, before: Some(bytes.clone()), after: Some(bytes) });
    }
    Ok((resolver, notes))
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub block: BlockId,
    pub range: Range<usize>,
    pub kind: BlockKind,
    pub excerpt: String,
    pub persisted_id: Option<String>,
}

pub fn candidates(doc: &Document) -> Vec<Candidate> {
    candidates_matching(doc, |_| true)
}

/// Build source picker rows for blocks that pass the caller's eligibility check.
pub fn candidates_matching(doc: &Document, mut eligible: impl FnMut(&Block) -> bool) -> Vec<Candidate> {
    doc.blocks()
        .iter()
        .filter_map(|(_, b)| {
            if matches!(b.kind, BlockKind::FrontMatter | BlockKind::Marker | BlockKind::Raw) || !eligible(b) {
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

#[derive(Debug)]
pub struct PersistentAssignment {
    pub document: String,
    pub id: String,
    pub edits: Vec<Edit>,
}

/// Prepare a portable reference whose source and proposed identities are unique in the vault.
///
/// # Errors
///
/// Returns an error if the block changed, cannot hold a reference, or has a conflicting identity.
pub fn permanent_in(
    resolver: &mut Resolver,
    path: &VaultPath,
    doc: &Document,
    block: BlockId,
) -> Result<PersistentAssignment, String> {
    let document = crate::identity::read(doc)?.id;
    if let Some(id) = &document {
        resolver.check_document_identity(path, id)?;
    }
    let node = doc.blocks().find(block).ok_or("目标块已不存在")?;
    resolver.check_block_identity(path, document.as_deref(), node)?;
    let assignment = permanent(doc, block)?;
    if !assignment.edits.is_empty() {
        let mut proposed = doc.clone();
        proposed.edit_group(assignment.edits.clone()).map_err(|e| e.to_string())?;
        proposed.reparse();
        // Check the newly claimed IDs and any scoped aliases produced by a short-ID upgrade.
        let validation = (|| {
            resolver.register_identity(path, &proposed);
            let id = resolve(&proposed, &assignment.id)?;
            let node = proposed.blocks().find(id).ok_or("目标块已不存在")?;
            resolver.check_document_identity(path, &assignment.document)?;
            resolver.check_block_identity(path, Some(&assignment.document), node)
        })();
        resolver.register_identity(path, doc);
        validation?;
    }
    Ok(assignment)
}

/// A portable reference upgrades a legacy short ID only at this explicit boundary.
pub fn permanent(doc: &Document, block: BlockId) -> Result<PersistentAssignment, String> {
    let (document, mut edits) = crate::identity::ensure(doc)?;
    let b = doc.blocks().find(block).ok_or("目标块已不存在")?;
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
    let after = insert_bytes(&bytes, &edit)?;
    // 再校验一次，覆盖准备 / 验证期间可检测到的外部改动。
    if vault.read_bytes(path).map_err(|e| e.to_string())?.1.hash != stamp.hash {
        return Err("目标笔记已变化，请重新打开补全".into());
    }
    vault.write_bytes(path, &after).map_err(|e| e.to_string())?;
    Ok((assignment.id, true))
}

fn insert_bytes(bytes: &[u8], edit: &Edit) -> Result<Vec<u8>, String> {
    debug_assert!(edit.range.is_empty());
    let bom = if bytes.starts_with(b"\xef\xbb\xbf") { 3 } else { 0 };
    let mut raw = bom;
    let mut normalized = 0;
    while normalized < edit.range.start && raw < bytes.len() {
        if bytes[raw..].starts_with(b"\r\n") {
            raw += 1
        }
        raw += 1;
        normalized += 1;
    }
    if normalized != edit.range.start {
        return Err("块位置超出文件范围".into());
    }
    let crlf = bytes[bom..].windows(2).any(|pair| pair == b"\r\n");
    let text = if crlf { edit.text.replace('\n', "\r\n") } else { edit.text.clone() };
    let mut after = bytes.to_vec();
    after.splice(raw..raw, text.bytes());
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
