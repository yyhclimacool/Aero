//! Container boundaries and lossless wrapping plans.
use crate::{Block, BlockId, BlockKind, Document, Edit};

pub fn ancestors(doc: &Document, id: BlockId) -> Vec<&Block> {
    fn walk<'a>(blocks: &'a [Block], id: BlockId, path: &mut Vec<&'a Block>) -> bool {
        for block in blocks {
            if block.id == id {
                return true;
            }
            path.push(block);
            if walk(&block.children, id, path) {
                return true;
            }
            path.pop();
        }
        false
    }
    let mut path = Vec::new();
    walk(&doc.blocks().roots, id, &mut path);
    path
}

pub fn is_editable(kind: &BlockKind) -> bool {
    matches!(kind, BlockKind::BlockQuote)
        || matches!(kind, BlockKind::Directive { name } if name == "highlight")
}

/// Source prefix for another line in the same direct container (quote/list nesting).
pub fn continuation(doc: &Document, id: BlockId) -> String {
    let Some(block) = doc.blocks().find(id) else { return String::new() };
    let source = doc.to_string();
    let start = doc.range_of(block).start;
    let line = source[..start].rfind('\n').map_or(0, |i| i + 1);
    source[line..start].chars().map(|c| if c == '>' || c == '\t' { c } else { ' ' }).collect()
}

pub fn placeholder(doc: &Document, block: &Block) -> usize {
    let range = doc.range_of(block);
    let source = doc.slice(range.clone()).to_string();
    if matches!(block.kind, BlockKind::Directive { .. }) {
        range.start + source.find('\n').map_or(source.len(), |n| n + 1)
    } else {
        range.end
    }
}

/// A standalone preview has no ancestor quote/list syntax.
pub fn local_source(doc: &Document, id: BlockId) -> Option<String> {
    let block = doc.blocks().find(id)?;
    let source = doc.slice(doc.content_range(block)).to_string();
    let prefix = continuation(doc, id);
    Some(
        source
            .split('\n')
            .enumerate()
            .map(|(i, line)| {
                if i == 0 || prefix.is_empty() {
                    line
                } else {
                    line.strip_prefix(prefix.as_str()).unwrap_or(line)
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Export an operation range without the ancestor's syntax. The first line
/// starts at the selected node; only continuation lines contain that prefix.
pub fn standalone(doc: &Document, range: std::ops::Range<usize>) -> String {
    let prefix = doc
        .blocks()
        .iter()
        .filter(|(_, b)| b.kind != BlockKind::Marker)
        .filter(|(_, b)| doc.range_of(b).start == range.start)
        .min_by_key(|(_, b)| doc.range_of(b).len())
        .map(|(_, b)| continuation(doc, b.id))
        .unwrap_or_default();
    doc.slice(range)
        .to_string()
        .split('\n')
        .enumerate()
        .map(|(i, line)| {
            if i == 0 || prefix.is_empty() {
                line
            } else {
                line.strip_prefix(prefix.as_str()).unwrap_or(line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn prefix_ranges(source: &str, prefix: &str) -> Vec<std::ops::Range<usize>> {
    if prefix.is_empty() {
        return Vec::new();
    }
    let mut offset = 0;
    source
        .split_inclusive('\n')
        .filter_map(|line| {
            let range = line.starts_with(prefix).then_some(offset..offset + prefix.len());
            offset += line.len();
            range
        })
        .collect()
}

/// A native selection can span hidden ancestor prefixes. Reattach them after
/// the text edit without allowing a child editor to remove its container.
pub fn repair_prefixes(
    source: &str,
    prefix: &str,
    first: bool,
    selection: std::ops::Range<usize>,
) -> (String, std::ops::Range<usize>) {
    if prefix.is_empty() {
        return (source.into(), selection);
    }
    let mut result = String::new();
    let mut shift = [0isize; 2];
    let mut offset = 0;
    for (i, line) in source.split_inclusive('\n').enumerate() {
        if (i > 0 || first) && !line.starts_with(prefix) {
            result.push_str(prefix);
            for (delta, at) in shift.iter_mut().zip([selection.start, selection.end]) {
                if offset <= at {
                    *delta += prefix.len() as isize;
                }
            }
        }
        result.push_str(line);
        offset += line.len();
    }
    (result, (selection.start as isize + shift[0]) as usize..(selection.end as isize + shift[1]) as usize)
}

pub fn unwrap(doc: &Document, id: BlockId) -> Result<Edit, String> {
    let block = doc.blocks().find(id).ok_or("目标容器已不存在")?;
    if block.persisted_id.is_some() || crate::block_refs::aliases(block).next().is_some() {
        return Err("此容器有引用身份，请改用透明背景和边框保留身份".into());
    }
    let range = crate::properties::owned_range(doc, id).ok_or("目标容器已不存在")?;
    let source = doc.to_string();
    let body_range = doc.range_of(block);
    let external = continuation(doc, id);
    let local = source[body_range]
        .split('\n')
        .enumerate()
        .map(|(i, line)| if i == 0 { line } else { line.strip_prefix(external.as_str()).unwrap_or(line) })
        .collect::<Vec<_>>()
        .join("\n");
    let src = local.as_str();
    let body = match &block.kind {
        BlockKind::BlockQuote => src
            .lines()
            .map(|line| {
                line.strip_prefix('>')
                    .unwrap_or(line)
                    .strip_prefix(' ')
                    .unwrap_or_else(|| line.strip_prefix('>').unwrap_or(line))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        BlockKind::Directive { name } if name == "highlight" => {
            let start = src.find('\n').ok_or("高亮容器没有结束围栏")? + 1;
            let end = src.rfind('\n').filter(|n| *n >= start).ok_or("高亮容器没有结束围栏")?;
            src[start..end].to_owned()
        }
        _ => return Err("此块不是可解除的容器".into()),
    };
    Ok(Edit::replace(range, body.replace('\n', &format!("\n{external}"))))
}

pub fn wrap(doc: &Document, id: BlockId, highlight: bool) -> Result<Edit, String> {
    let block = doc.blocks().find(id).ok_or("目标块已不存在")?;
    let range = crate::properties::owned_range(doc, id).ok_or("目标块已不存在")?;
    let external = continuation(doc, id);
    let source = doc
        .slice(range.clone())
        .to_string()
        .split('\n')
        .enumerate()
        .map(|(i, line)| if i == 0 { line } else { line.strip_prefix(external.as_str()).unwrap_or(line) })
        .collect::<Vec<_>>()
        .join("\n");
    let mut body = source.clone();
    let mut probe = Document::from_text(&source);
    if let Some(first) = probe.blocks().roots.first() {
        let mut attrs = first.attrs.clone();
        crate::properties::set(&mut attrs, "aero-aliases", None);
        if let Some(edit) = crate::properties::edit(&probe, first.id, &attrs, None)? {
            probe.edit(edit).map_err(|e| e.to_string())?;
            body = probe.to_string();
        }
    }
    let mut outer = crate::Attrs::default();
    for (key, value) in &block.attrs.pairs {
        if key == "aero-aliases" {
            outer.pairs.push((key.clone(), value.clone()));
        }
    }
    let metadata = match (outer.is_empty(), block.persisted_id.as_deref()) {
        (true, None) => String::new(),
        (empty, id) => format!(
            "\n\n{}{}",
            if empty { String::new() } else { format!("{{{}}}", crate::properties::render(&outer)) },
            id.map(|id| format!(" ^{id}")).unwrap_or_default()
        ),
    };
    let wrapped = if highlight {
        let length = body
            .lines()
            .map(|line| line.trim_start().chars().take_while(|c| *c == ':').count())
            .max()
            .unwrap_or(0)
            .max(2)
            + 1;
        let fence = ":".repeat(length);
        format!("{fence}highlight\n{}\n{fence}{metadata}", body.trim_end_matches('\n'))
    } else {
        format!("{}{metadata}", body.lines().map(|line| format!("> {line}")).collect::<Vec<_>>().join("\n"))
    };
    let replacement = wrapped.replace('\n', &format!("\n{external}"));
    let mut edits = vec![Edit::replace(range, replacement)];
    if highlight {
        let mut required =
            wrapped.lines().map(|l| l.chars().take_while(|c| *c == ':').count()).max().unwrap_or(3);
        let text = doc.to_string();
        for ancestor in
            ancestors(doc, id).into_iter().rev().filter(|b| matches!(b.kind, BlockKind::Directive { .. }))
        {
            let span = doc.range_of(ancestor);
            let opening = &text[span.clone()];
            let count = opening.chars().take_while(|c| *c == ':').count();
            if count > required {
                required = count;
                continue;
            }
            required += 1;
            let fence = ":".repeat(required);
            edits.push(Edit::replace(span.start..span.start + count, fence.clone()));
            if let Some(at) = opening.rfind('\n') {
                let closing = span.start + at + 1;
                let start = text[closing..span.end].find(':').ok_or("容器结束围栏不完整")? + closing;
                let length = text[start..span.end].chars().take_while(|c| *c == ':').count();
                edits.push(Edit::replace(start..start + length, fence));
            }
        }
    }
    if edits.len() == 1 {
        return Ok(edits.remove(0));
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.range.start));
    let mut probe = doc.clone();
    probe.edit_group(edits).map_err(|e| e.to_string())?;
    Ok(Edit::replace(0..doc.len(), probe.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exported_children_and_native_multiline_edits_keep_ancestor_syntax_separate() {
        let doc = Document::from_text("> first\n> second\n>\n> third\n");
        let child = &doc.blocks().roots[0].children[0];
        assert_eq!(standalone(&doc, doc.range_of(child)), "first\nsecond");
        assert_eq!(prefix_ranges("first\n> second", "> "), vec![6..8]);
        let (repaired, range) = repair_prefixes("first\nnew\n> last", "> ", false, 9..9);
        assert_eq!(repaired, "first\n> new\n> last");
        assert_eq!(range, 11..11);
    }
    #[test]
    fn nested_wrap_and_unwrap_keep_external_quote_context() {
        let original = "> first\n>\n> second\n> next\n";
        let mut doc = Document::from_text(original);
        let id = doc.blocks().roots[0].children[1].id;
        doc.edit(wrap(&doc, id, true).unwrap()).unwrap();
        doc.reparse();
        assert_eq!(doc.blocks().roots.len(), 1, "{}", doc);
        let quote = &doc.blocks().roots[0];
        assert_eq!(quote.children.len(), 2);
        let container = &quote.children[1];
        assert!(is_editable(&container.kind));
        assert_eq!(local_source(&doc, container.children[0].id).as_deref(), Some("second\nnext"));
        doc.edit(unwrap(&doc, container.id).unwrap()).unwrap();
        doc.reparse();
        assert_eq!(doc.to_string(), original);
    }

    #[test]
    fn wrapping_inside_highlight_grows_outer_fence_and_keeps_identity() {
        let mut doc = Document::from_text(":::highlight\nbody ^old\n:::\n");
        let id = doc.blocks().roots[0].children[0].id;
        doc.edit(wrap(&doc, id, true).unwrap()).unwrap();
        doc.reparse();
        assert_eq!(doc.blocks().roots.len(), 1, "{}", doc);
        let outer = &doc.blocks().roots[0];
        assert_eq!(outer.children.iter().filter(|b| b.kind != BlockKind::Marker).count(), 1);
        assert_eq!(outer.children[0].persisted_id.as_deref(), Some("old"));
        assert!(unwrap(&doc, outer.children[0].id).is_err());
    }
}
