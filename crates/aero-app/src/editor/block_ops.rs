//! Source operations on explicitly selected tree nodes.

use std::{collections::HashSet, ops::Range};

use aero_core::{BlockId, BlockKind, Document, Edit, containers, properties};

use super::text_ops::{line_start, strip_continuation};

/// Resolve targets in document order and let a selected ancestor own its subtree.
pub(super) fn targets(doc: &Document, ids: impl IntoIterator<Item = BlockId>) -> Vec<BlockId> {
    let mut ids: HashSet<_> = ids.into_iter().collect();
    let mut selected = Vec::new();
    let mut selected_depth = None;
    for (depth, block) in doc.blocks().iter() {
        if selected_depth.is_some_and(|ancestor| depth > ancestor) {
            continue;
        }
        selected_depth = None;
        if block.kind != BlockKind::Marker && ids.remove(&block.id) {
            selected.push(block.id);
            selected_depth = Some(depth);
            if ids.is_empty() {
                break;
            }
        }
    }
    selected
}

pub(super) fn source(doc: &Document, text: &str, ids: &[BlockId]) -> String {
    ids.iter()
        .filter_map(|&id| {
            let range = properties::owned_range(doc, id)?;
            let (prefix, task_header) = continuation(doc, text, id, range.start);
            let aligned_prefix = task_header.then(|| {
                let line = line_start(text, range.start);
                text[line..range.start]
                    .chars()
                    .map(|c| if matches!(c, '>' | '\t') { c } else { ' ' })
                    .collect::<String>()
            });
            Some(
                text[range]
                    .split('\n')
                    .enumerate()
                    .map(|(i, line)| {
                        if i == 0 {
                            return line;
                        }
                        let marker = if task_header {
                            strip_task_continuation(line, &prefix)
                        } else {
                            strip_continuation(line, &prefix)
                        };
                        let Some(prefix) = &aligned_prefix else { return marker };
                        // Task continuations may align to either the list marker or the text column.
                        let aligned = strip_task_continuation(line, prefix);
                        if aligned.len() < marker.len() { aligned } else { marker }
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn strip_task_continuation<'a>(line: &'a str, prefix: &str) -> &'a str {
    let quoted = strip_continuation(line, prefix);
    let Some((_, lazy_prefix)) = prefix.rsplit_once('>') else { return quoted };
    // A lazy quote continuation omits `>` but retains the list indentation.
    let lazy_prefix = lazy_prefix.strip_prefix([' ', '\t']).unwrap_or(lazy_prefix);
    let lazy = strip_continuation(line, lazy_prefix);
    if lazy.len() < quoted.len() { lazy } else { quoted }
}

/// A task's checkbox sits on the item's first line. Continuations use the
/// width of the list marker alone.
pub(super) fn continuation(doc: &Document, text: &str, id: BlockId, start: usize) -> (String, bool) {
    let line = line_start(text, start);
    let mut prefix_end = start;
    if let Some(parent) = containers::ancestors(doc, id).last()
        && matches!(parent.kind, BlockKind::ListItem { task: Some(_) })
    {
        let item_start = doc.range_of(parent).start;
        if item_start >= line
            && let Some(checkbox) = text[item_start..start].find('[')
        {
            prefix_end = item_start + checkbox;
        }
    }
    let prefix = text[line..prefix_end].chars().map(|c| if matches!(c, '>' | '\t') { c } else { ' ' });
    (prefix.collect(), prefix_end != start)
}

fn removal_range(doc: &Document, text: &str, id: BlockId) -> Option<Range<usize>> {
    let mut range = properties::owned_range(doc, id)?;
    let line = line_start(text, range.start);
    let path = containers::ancestors(doc, id);
    if let Some(parent) = path.last()
        && doc.range_of(parent).start >= line
        && matches!(parent.kind, BlockKind::ListItem { .. } | BlockKind::BlockQuote)
    {
        // This line also contains the parent's opening marker. A list item
        // with an empty opening line needs its next child on the next line.
        if matches!(parent.kind, BlockKind::ListItem { .. }) {
            let (prefix, _) = continuation(doc, text, id, range.start);
            let parent_end = doc.range_of(parent).end;
            while let Some(rest) = text[range.end..].strip_prefix('\n')
                && let Some((next, _)) = rest.split_once('\n')
                && range.end + 1 + next.len() < parent_end
                && strip_continuation(next, &prefix).trim().is_empty()
            {
                range.end += next.len() + 1;
            }
        }
        return Some(range);
    }
    range.start = line;
    if text[range.end..].starts_with('\n') {
        range.end += 1;
    }
    if path.is_empty() {
        range.end += text[range.end..].bytes().take_while(|&byte| byte == b'\n').count();
        if range.end == text.len() {
            while text[..range.start].ends_with("\n\n") {
                range.start -= 1;
            }
        }
    }
    Some(range)
}

/// Delete one continuous range, including blank lines between adjacent siblings.
pub(super) fn deletion_span(doc: &Document, text: &str, ids: &[BlockId]) -> Option<Range<usize>> {
    let first = ids.first().and_then(|&id| removal_range(doc, text, id))?;
    let last = ids.last().and_then(|&id| removal_range(doc, text, id))?;
    Some(first.start..last.end)
}

/// Remove the selected nodes in one undo group, retaining unselected syntax
/// between them. Coordinates are ordered from the end of the document.
pub(super) fn deletions(doc: &Document, text: &str, ids: &[BlockId]) -> Vec<Edit> {
    let mut ranges: Vec<_> = ids.iter().filter_map(|&id| removal_range(doc, text, id)).collect();
    ranges.sort_unstable_by_key(|range| range.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        if let Some(previous) = merged.last_mut().filter(|previous| range.start <= previous.end) {
            previous.end = previous.end.max(range.end);
        } else if !range.is_empty() {
            merged.push(range);
        }
    }
    merged.into_iter().rev().map(Edit::delete).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_targets_distinguish_the_list_item_and_each_body() {
        let markdown = "- first ^first\n\n  continuation ^continuation\n\n  - nested ^nested\n- sibling\n";
        let doc = Document::from_text(markdown);
        let list = &doc.blocks().roots[0];
        let item = &list.children[0];
        let first = item.children[0].id;
        let continuation = item.children[1].id;
        for (ids, copied, remaining) in [
            (
                vec![first],
                "first ^first",
                "- \n  continuation ^continuation\n\n  - nested ^nested\n- sibling\n",
            ),
            (
                vec![continuation],
                "continuation ^continuation",
                "- first ^first\n\n\n  - nested ^nested\n- sibling\n",
            ),
            (
                vec![continuation, item.id, first],
                "- first ^first\n\n  continuation ^continuation\n\n  - nested ^nested",
                "- sibling\n",
            ),
            (vec![item.id, list.id, first], markdown.trim_end(), ""),
        ] {
            let ids = targets(&doc, ids);
            assert_eq!(source(&doc, markdown, &ids).trim_end_matches('\n'), copied);
            let mut out = doc.clone();
            out.edit_group(deletions(&doc, markdown, &ids)).unwrap();
            out.reparse();
            assert_eq!(out.to_string(), remaining);
            if ids == [first] || ids == [continuation] {
                let list = &out.blocks().roots[0];
                assert_eq!(list.children.len(), 2);
                assert_eq!(list.children[0].children.len(), 2);
                assert!(matches!(list.children[0].children[1].kind, BlockKind::List { .. }));
            }
            out.undo().unwrap();
            assert_eq!(out.to_string(), markdown);
        }
    }

    #[test]
    fn copying_task_body_strips_either_continuation_indent() {
        for markdown in [
            "- [ ] first\n  continued\n\n  - nested ^nested\n- sibling\n",
            "- [ ] first\n      continued\n\n  - nested ^nested\n- sibling\n",
        ] {
            let doc = Document::from_text(markdown);
            let first = doc.blocks().roots[0].children[0].children[0].id;
            let ids = targets(&doc, [first]);
            assert_eq!(source(&doc, markdown, &ids), "first\ncontinued", "input {markdown:?}");
        }
    }

    #[test]
    fn copying_task_body_inside_quote_strips_container_prefixes() {
        let markdown = "> - [ ] first\n>       continued\n>\n>   - nested ^nested\n> - sibling\n";
        let doc = Document::from_text(markdown);
        let first = doc.blocks().roots[0].children[0].children[0].children[0].id;
        let ids = targets(&doc, [first]);
        assert_eq!(source(&doc, markdown, &ids), "first\ncontinued");
    }

    #[test]
    fn copying_task_body_inside_quote_strips_lazy_continuation_indent() {
        for markdown in [
            "> - [ ] first\n  continued\n>\n>   - nested ^nested\n> - sibling\n",
            "> - [ ] first\n      continued\n>\n>   - nested ^nested\n> - sibling\n",
        ] {
            let doc = Document::from_text(markdown);
            let first = doc.blocks().roots[0].children[0].children[0].children[0].id;
            let ids = targets(&doc, [first]);
            assert_eq!(source(&doc, markdown, &ids), "first\ncontinued", "input {markdown:?}");
        }
    }

    #[test]
    fn cross_container_deletion_keeps_an_empty_quote_and_highlight() {
        let markdown = ":::highlight\ninside ^first\n:::\n\n> quoted ^second\n\noutside\n";
        let mut doc = Document::from_text(markdown);
        let ids = [doc.blocks().roots[0].children[0].id, doc.blocks().roots[1].children[0].id];
        assert_eq!(source(&doc, markdown, &ids), "inside ^first\n\nquoted ^second");
        doc.edit_group(deletions(&doc, markdown, &ids)).unwrap();
        doc.reparse();
        assert_eq!(doc.to_string(), ":::highlight\n:::\n\n> \n\noutside\n");
        assert!(
            doc.blocks().roots[..2].iter().all(|b| containers::is_editable(&b.kind) && b.children.is_empty())
        );
        doc.undo().unwrap();
        assert_eq!(doc.to_string(), markdown);
    }
}
