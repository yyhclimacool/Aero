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

#[derive(Clone)]
enum SourcePrefix {
    Quote,
    List { opening: std::ops::Range<usize>, width: usize },
    Fence { width: usize },
}

fn opening_indent(doc: &Document, block: &Block, source: &str, prefix: &[SourcePrefix]) -> usize {
    let start = doc.range_of(block).start;
    let line = source[..start].rfind('\n').map_or(0, |i| i + 1);
    let (_, column) = strip_source_prefix(&source[line..start], line, prefix);
    source_column(&source[line..start], 0) - column
}

fn source_prefix(doc: &Document, id: BlockId, source: &str, content: bool) -> Vec<SourcePrefix> {
    let mut prefix = Vec::new();
    for ancestor in ancestors(doc, id).into_iter().chain(doc.blocks().find(id).filter(|_| content)) {
        match ancestor.kind {
            BlockKind::BlockQuote | BlockKind::Callout { .. } => prefix.push(SourcePrefix::Quote),
            BlockKind::Directive { .. } => {
                let width = opening_indent(doc, ancestor, source, &prefix);
                prefix.push(SourcePrefix::Fence { width });
            }
            BlockKind::ListItem { .. } => {
                let start = doc.range_of(ancestor).start;
                let line = source[..start].rfind('\n').map_or(0, |i| i + 1);
                let marker_column = source_column(&source[line..start], 0);
                let indent = opening_indent(doc, ancestor, source, &prefix);
                let rest = &source[start..];
                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                let marker = if digits == 0 { 1 } else { digits + 1 };
                let spaces = rest[marker..].bytes().take_while(|b| matches!(b, b' ' | b'\t')).count();
                let width = source_column(&rest[marker..marker + spaces], marker_column + marker)
                    - marker_column
                    - marker;
                let (padding, bytes) = if (1..=4).contains(&width)
                    && !matches!(rest.as_bytes().get(marker + spaces), None | Some(b'\n' | b'\r'))
                {
                    (width, spaces)
                } else {
                    (1, usize::from(spaces > 0))
                };
                let mut end = start + marker + bytes;
                if matches!(ancestor.kind, BlockKind::ListItem { task: Some(_) }) {
                    end += 4;
                }
                prefix.push(SourcePrefix::List { opening: start..end, width: indent + marker + padding });
            }
            _ => {}
        }
    }
    prefix
}

fn source_column(source: &str, start: usize) -> usize {
    source.bytes().fold(start, |column, b| column + if b == b'\t' { 4 - column % 4 } else { 1 })
}

fn strip_source_prefix<'a>(
    line: &'a str,
    at: usize,
    prefix: &[SourcePrefix],
) -> (std::borrow::Cow<'a, str>, usize) {
    let (mut used, mut column, mut pending) = (0, 0, 0);
    for part in prefix {
        let rest = &line[used..];
        let spaces = rest.bytes().take_while(|b| matches!(b, b' ' | b'\t')).count();
        let indent = source_column(&rest[..spaces], column + pending) - column;
        let mut consume = match part {
            SourcePrefix::Quote => {
                if indent > 3 || rest.as_bytes().get(spaces) != Some(&b'>') {
                    break;
                }
                used += spaces + 1;
                column += indent + 1;
                pending = 0;
                usize::from(matches!(line.as_bytes().get(used), Some(b' ' | b'\t')))
            }
            SourcePrefix::List { opening, width } => {
                if at + used <= opening.start && opening.start < at + line.len() {
                    let end = opening.end.min(at + line.len()) - at;
                    column = source_column(&line[used..end], column + pending);
                    used = end;
                    pending = 0;
                    continue;
                }
                if indent < *width && !rest.trim().is_empty() {
                    break;
                }
                indent.min(*width)
            }
            SourcePrefix::Fence { width } => indent.min(*width),
        };
        while consume > 0 {
            let width = if pending > 0 {
                pending
            } else {
                let width = if line.as_bytes()[used] == b'\t' { 4 - column % 4 } else { 1 };
                used += 1;
                width
            };
            let consumed = consume.min(width);
            column += consumed;
            consume -= consumed;
            pending = width - consumed;
        }
    }
    let rest = if pending == 0 {
        std::borrow::Cow::Borrowed(&line[used..])
    } else {
        std::borrow::Cow::Owned(format!("{}{}", " ".repeat(pending), &line[used..]))
    };
    (rest, column)
}

/// Source prefix for another line in the same direct container.
pub fn continuation(doc: &Document, id: BlockId) -> String {
    render_prefix(&source_prefix(doc, id, &doc.to_string(), false))
}

pub fn content_continuation(doc: &Document, id: BlockId) -> String {
    render_prefix(&source_prefix(doc, id, &doc.to_string(), true))
}

pub fn opening_continuation(doc: &Document, id: BlockId) -> String {
    let source = doc.to_string();
    let prefix = source_prefix(doc, id, &source, false);
    let width = doc.blocks().find(id).map_or(0, |b| opening_indent(doc, b, &source, &prefix));
    format!("{}{}", render_prefix(&prefix), " ".repeat(width))
}

fn render_prefix(prefix: &[SourcePrefix]) -> String {
    prefix
        .iter()
        .map(|part| match part {
            SourcePrefix::Quote => "> ".into(),
            SourcePrefix::List { width, .. } | SourcePrefix::Fence { width } => " ".repeat(*width),
        })
        .collect()
}

fn local_range(doc: &Document, id: BlockId, range: std::ops::Range<usize>) -> String {
    let source = doc.to_string();
    let prefix = source_prefix(doc, id, &source, false);
    let mut body_prefix = prefix.clone();
    let block = doc.blocks().find(id);
    let code_indent = block
        .filter(|b| b.kind == BlockKind::CodeBlock)
        .map_or(0, |b| opening_indent(doc, b, &source, &prefix));
    let body_end =
        block.filter(|b| b.kind.is_container() || matches!(b.kind, BlockKind::CodeFence { .. })).map(|b| {
            let width = opening_indent(doc, b, &source, &prefix);
            body_prefix.push(SourcePrefix::Fence { width });
            doc.range_of(b).end
        });
    let mut at = range.start;
    source[range]
        .split_inclusive('\n')
        .enumerate()
        .map(|(i, line)| {
            let prefix = if body_end.is_some_and(|end| at < end) { &body_prefix } else { &prefix };
            let local = if i == 0 && code_indent > 0 {
                format!("{}{line}", " ".repeat(code_indent)).into()
            } else if i == 0 {
                line.into()
            } else {
                strip_source_prefix(line, at, prefix).0
            };
            at += line.len();
            local
        })
        .collect()
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
    Some(local_range(doc, id, doc.content_range(block)))
}

/// Export an operation range without the ancestor's syntax. The first line
/// starts at the selected node; only continuation lines contain that prefix.
pub fn standalone(doc: &Document, range: std::ops::Range<usize>) -> String {
    let id = doc
        .blocks()
        .iter()
        .filter(|(_, b)| b.kind != BlockKind::Marker)
        .filter(|(_, b)| doc.range_of(b).start == range.start)
        .min_by_key(|(_, b)| doc.range_of(b).len())
        .map(|(_, b)| b.id);
    id.map_or_else(|| doc.slice(range.clone()).to_string(), |id| local_range(doc, id, range.clone()))
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
    let body_range = doc.range_of(block);
    let external = opening_continuation(doc, id);
    let local = local_range(doc, id, body_range);
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
    let external = opening_continuation(doc, id);
    let source = local_range(doc, id, range.clone());
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
    fn local_source_accepts_variable_quote_prefixes() {
        for source in ["> alpha\n>beta ^body\n", ">alpha\n> beta ^body\n", "> > alpha\n>>beta ^body\n"] {
            let doc = Document::from_text(source);
            let id = crate::block_refs::resolve(&doc, "body").unwrap();
            assert_eq!(local_source(&doc, id).as_deref(), Some("alpha\nbeta"), "{source}");
            let block = doc.blocks().find(id).unwrap();
            assert_eq!(standalone(&doc, doc.range_of(block)), "alpha\nbeta ^body", "{source}");
        }
    }

    #[test]
    fn task_continuation_excludes_checkbox_width() {
        for (source, expected) in [
            ("- [ ] alpha\n  beta ^body\n", "  "),
            ("> - [x] alpha\n>   beta ^body\n", ">   "),
            ("> 12. [ ] alpha\n>     beta ^body\n", ">     "),
            ("- [ ] parent\n\n  > alpha\n  >beta ^body\n", "  > "),
        ] {
            let doc = Document::from_text(source);
            let id = crate::block_refs::resolve(&doc, "body").unwrap();
            assert_eq!(continuation(&doc, id), expected, "{source}");
            assert_eq!(local_source(&doc, id).as_deref(), Some("alpha\nbeta"), "{source}");
        }
    }

    #[test]
    fn local_source_preserves_code_indentation_beyond_container_prefix() {
        for source in [
            "> ```text\n>  leading\n> \tindented\n>    four\n> ```\n",
            "- item\n\n  > ```text\n  >  leading\n  > \tindented\n  >    four\n  > ```\n",
        ] {
            let doc = Document::from_text(source);
            let code =
                doc.blocks().iter().find(|(_, b)| matches!(b.kind, BlockKind::CodeFence { .. })).unwrap().1;
            assert_eq!(
                local_source(&doc, code.id).as_deref(),
                Some("```text\n leading\n\tindented\n   four\n```"),
                "{source}"
            );
        }
    }

    #[test]
    fn local_source_keeps_whitespace_after_tabbed_container_prefixes() {
        let doc = Document::from_text("-\t[ ] alpha\n\tbeta ^body\n");
        let id = crate::block_refs::resolve(&doc, "body").unwrap();
        assert_eq!(continuation(&doc, id), "    ");
        assert_eq!(local_source(&doc, id).as_deref(), Some("alpha\nbeta"));
        for (source, expected) in [
            ("> ```text\n>\tindented\n> ```\n", "```text\n  indented\n```"),
            ("- ```text\n\tindented\n  ```\n", "```text\n  indented\n```"),
            ("> - ```text\n>\t  indented\n>   ```\n", "```text\n  indented\n```"),
        ] {
            let doc = Document::from_text(source);
            let code =
                doc.blocks().iter().find(|(_, b)| matches!(b.kind, BlockKind::CodeFence { .. })).unwrap().1;
            assert_eq!(local_source(&doc, code.id).as_deref(), Some(expected), "{source}");
        }
    }

    fn code_literals(source: &str) -> Vec<String> {
        let arena = comrak::Arena::new();
        let root = comrak::parse_document(&arena, source, &crate::parse::comrak_options());
        root.descendants()
            .filter_map(|node| match &node.data().value {
                comrak::nodes::NodeValue::CodeBlock(code) => Some(code.literal.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn indented_fence_exports_preserve_parsed_code_literals() {
        for source in [
            "tail\n\n  ~~~text\n    keep\n  ~~~\n",
            "  ::::highlight\n  :::highlight\n  ~~~text\n      :::\n  ~~~\n  :::\n  ::::\n",
            "  :::highlight\n    ~~~text\n      keep\n  \tmore\n    ~~~\n  :::\n",
            "> - item\n>\n>     :::highlight\n>       ~~~text\n>         keep\n>     \tmore\n>       ~~~\n>     :::\n",
            "  :::highlight\n  > - item\n  >\n  >     ~~~text\n  >       keep\n  > \tmore\n  >     ~~~\n  :::\n",
        ] {
            let doc = Document::from_text(source);
            let code =
                doc.blocks().iter().find(|(_, b)| matches!(b.kind, BlockKind::CodeFence { .. })).unwrap().1;
            let before = code_literals(source);
            assert_eq!(before.len(), 1, "{source}");
            let local = local_source(&doc, code.id).unwrap();
            assert_eq!(code_literals(&local), before, "local: {local:?}, source: {source}");
            let exported = standalone(&doc, doc.range_of(code));
            assert_eq!(code_literals(&exported), before, "exported: {exported:?}, source: {source}");
        }
    }

    #[test]
    fn indented_directive_export_preserves_descendant_code_literal() {
        for source in [
            "  :::highlight\n  > - item\n  >\n  >     ~~~text\n  >       keep\n  >     ~~~\n  :::\n",
            "  :::highlight\n~~~text\n    keep\n~~~\n  :::\n",
        ] {
            let doc = Document::from_text(source);
            let block = &doc.blocks().roots[0];
            let exported = standalone(&doc, doc.range_of(block));
            assert_eq!(code_literals(&exported), code_literals(source), "{exported}");
        }
    }

    #[test]
    fn wrapping_indented_fences_preserves_literals_and_undo() {
        for source in [
            "  ~~~text\n    keep\n  \tmore\n  ~~~\n\ntail\n",
            "  :::highlight\n~~~text\n    keep\n~~~\n  :::\n\ntail\n",
            "> - item\n>\n>     :::highlight\n>       ~~~text\n>         keep\n>       ~~~\n>     :::\n\ntail\n",
        ] {
            for highlight in [false, true] {
                let mut doc = Document::from_text(source);
                let id = doc
                    .blocks()
                    .iter()
                    .find(|(_, b)| matches!(b.kind, BlockKind::CodeFence { .. }))
                    .unwrap()
                    .1
                    .id;
                doc.edit(wrap(&doc, id, highlight).unwrap()).unwrap();
                doc.reparse();
                assert_eq!(code_literals(&doc.to_string()), code_literals(source), "{doc}");
                assert!(doc.to_string().ends_with("\n\ntail\n"));
                doc.undo().unwrap();
                assert_eq!(doc.to_string(), source);
            }
        }
    }

    #[test]
    fn unwrapping_indented_directive_preserves_literals_and_undo() {
        let source = "  :::highlight\n~~~text\n    keep\n~~~\n  :::\n\ntail\n";
        let mut doc = Document::from_text(source);
        let id = doc.blocks().roots[0].id;
        doc.edit(unwrap(&doc, id).unwrap()).unwrap();
        doc.reparse();
        assert_eq!(code_literals(&doc.to_string()), code_literals(source), "{doc}");
        assert!(doc.to_string().ends_with("\n\ntail\n"));
        doc.undo().unwrap();
        assert_eq!(doc.to_string(), source);
    }

    #[test]
    fn standalone_indented_containers_and_code_keep_code_literals() {
        for source in
            ["  - ~~~\n    keep\n    ~~~\n", "  > ~~~\n  >   keep\n  > ~~~\n", "    keep\n      more\n"]
        {
            let doc = Document::from_text(source);
            let root = &doc.blocks().roots[0];
            let exported = standalone(&doc, crate::properties::owned_range(&doc, root.id).unwrap());
            assert_eq!(code_literals(&exported), code_literals(source), "{source:?} => {exported:?}");
            let decoded = Document::from_text(&exported);
            assert_eq!(decoded.blocks().roots[0].kind, root.kind);
        }
    }

    #[test]
    fn nested_directive_operation_range_includes_its_closing_fence() {
        let source = "  ::::highlight\n  > - item\n  >\n  >   :::highlight\n  >   y\n  >   :::\n  ::::\n";
        let doc = Document::from_text(source);
        let inner = doc
            .blocks()
            .iter()
            .filter(|(_, b)| matches!(b.kind, BlockKind::Directive { .. }))
            .last()
            .unwrap()
            .1;
        let arena = comrak::Arena::new();
        let root = comrak::parse_document(&arena, source, &crate::parse::comrak_options());
        let positions: Vec<_> = root
            .descendants()
            .filter_map(|n| {
                let data = n.data();
                matches!(data.value, comrak::nodes::NodeValue::BlockDirective(_)).then_some(data.sourcepos)
            })
            .collect();
        assert_eq!(
            doc.slice(doc.range_of(inner)).to_string(),
            ":::highlight\n  >   y\n  >   :::",
            "comrak positions: {positions:?}; parse: {:?}",
            crate::parse(source)
        );
    }

    #[test]
    fn directive_continuation_uses_consumed_columns_after_ancestors() {
        for (source, expected) in [
            ("  ::::highlight\n  :::highlight\n  body ^body\n  :::\n  ::::\n", "  "),
            ("  ::::highlight\n    :::highlight\n    body ^body\n    :::\n  ::::\n", "    "),
            ("> - item\n>\n>     :::highlight\n>       body ^body\n>     :::\n", ">     "),
            ("-\t:::highlight\n\t  body ^body\n\t:::\n", "    "),
        ] {
            let doc = Document::from_text(source);
            let id = crate::block_refs::resolve(&doc, "body").unwrap();
            assert_eq!(continuation(&doc, id), expected, "{source}");
        }
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
    fn nested_wrap_changes_only_ancestor_fences_and_selected_body() {
        let source = "> ::::highlight {.outer}\n> :::highlight {.inner}\n> body {.red} ^body\n> :::\n>\n> ^inner\n>\n> ```text\n> :::::literal\n> \n> \n> \n> end\n> ```\n> ::::\n>\n> ^outer\n\ntail\n";
        let mut doc = Document::from_text(source);
        let body = crate::block_refs::resolve(&doc, "body").unwrap();
        doc.edit(wrap(&doc, body, true).unwrap()).unwrap();
        doc.reparse();
        assert_eq!(
            doc.to_string(),
            source
                .replacen("> ::::highlight", "> :::::highlight", 1)
                .replacen("> :::highlight", "> ::::highlight", 1)
                .replacen(
                    "> body {.red} ^body\n> :::",
                    "> :::highlight\n> body {.red}\n> :::\n> \n>  ^body\n> ::::",
                    1
                )
                .replacen("> ::::\n>\n> ^outer", "> :::::\n>\n> ^outer", 1)
        );
        let body = crate::block_refs::resolve(&doc, "body").unwrap();
        let inner = crate::block_refs::resolve(&doc, "inner").unwrap();
        let outer = crate::block_refs::resolve(&doc, "outer").unwrap();
        assert!(ancestors(&doc, body).iter().any(|b| b.id == inner));
        assert!(ancestors(&doc, body).iter().any(|b| b.id == outer));
        let body = doc.blocks().find(body).unwrap();
        assert_eq!(body.children[0].attrs.classes, ["red"]);
        assert_eq!(local_source(&doc, body.children[0].id).as_deref(), Some("body"));
        assert_eq!(doc.blocks().find(inner).unwrap().attrs.classes, ["inner"]);
        assert_eq!(doc.blocks().find(outer).unwrap().attrs.classes, ["outer"]);
        doc.undo().unwrap();
        assert_eq!(doc.to_string(), source);
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
