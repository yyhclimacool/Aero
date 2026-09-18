//! ⌘K 的源码编辑计划：包裹选区、定位已有地址，或启动笔记补全。

use std::ops::Range;

use comrak::nodes::{AstNode, NodeValue};
use comrak::{Arena, parse_document};

use crate::{Edit, links, parse};

#[derive(Debug)]
pub struct Plan {
    /// None 表示仅选中已有地址，不修改文档或撤销栈。
    pub edit: Option<Edit>,
    /// 编辑后的文档字节选区。
    pub selection: Range<usize>,
    pub complete_note: bool,
}

/// 单个编辑单位内的行内操作；跨行和无法保持 Markdown 结构的选区不处理。
pub fn plan(src: &str, selection: Range<usize>, clipboard: Option<&str>) -> Option<Plan> {
    let chosen = src.get(selection.clone())?;
    if chosen.contains(['\n', '\r']) {
        return None;
    }
    for span in links::scan(src) {
        if contains(&span.range, &selection) {
            return Some(Plan {
                edit: None,
                selection: links::target_range(src, &span),
                complete_note: false,
            });
        }
        if overlaps(&span.range, &selection) {
            return None;
        }
    }
    if links::scan_embeds(src).iter().any(|s| overlaps(&s.range, &selection)) {
        return None;
    }
    let arena = Arena::new();
    let root = parse_document(&arena, src, &parse::comrak_options());
    let positions = SourceMap::new(src);
    for node in root.descendants() {
        let data = node.data();
        if !matches!(
            data.value,
            NodeValue::Link(_)
                | NodeValue::Image(_)
                | NodeValue::Code(_)
                | NodeValue::CodeBlock(_)
                | NodeValue::Math(_)
                | NodeValue::HtmlInline(_)
                | NodeValue::HtmlBlock(_)
                | NodeValue::FrontMatter(_)
        ) {
            continue;
        }
        let Some(range) = positions.range(node) else { continue };
        if !overlaps(&range, &selection) {
            continue;
        }
        if matches!(data.value, NodeValue::Link(_)) && contains(&range, &selection) {
            // 引用式链接 / 自动链接不改写公共定义，也不把链接再嵌套一次。
            let destination = destination_range(src.get(range.clone())?)?;
            return Some(Plan {
                edit: None,
                selection: range.start + destination.start..range.start + destination.end,
                complete_note: false,
            });
        }
        // 完整的行内代码可以作为链接文字；代码内部的局部选区不能。
        if matches!(data.value, NodeValue::Code(_))
            && !selection.is_empty()
            && selection.start <= range.start
            && selection.end >= range.end
        {
            continue;
        }
        return None;
    }

    if selection.is_empty() {
        let marker = "[[aero-link-placeholder]]";
        let mut probe = src.to_owned();
        probe.insert_str(selection.start, marker);
        if !links::scan(&probe).iter().any(|s| s.range == (selection.start..selection.start + marker.len())) {
            return None;
        }
        return Some(Plan {
            edit: Some(Edit::replace(selection.clone(), "[[]]")),
            selection: selection.start + 2..selection.start + 2,
            complete_note: true,
        });
    }

    // 选区必须在同一行内容器的文字范围内，不包裹标题、列表或引用的结构标记。
    if chosen.trim().is_empty()
        || !root.descendants().any(|node| {
            matches!(node.data().value, NodeValue::Paragraph | NodeValue::Heading(_) | NodeValue::TableCell)
                && node
                    .first_child()
                    .and_then(|n| positions.range(n))
                    .zip(node.last_child().and_then(|n| positions.range(n)))
                    .is_some_and(|(first, last)| selection.start >= first.start && selection.end <= last.end)
        })
    {
        return None;
    }
    let destination = clipboard.and_then(clipboard_destination).unwrap_or_else(|| "url".into());
    let structure = parse::parse(src).blocks;
    // 保留已有强调 / 行内代码等源码；只在括号妨碍链接解析时转义它们。
    for label in [chosen.to_owned(), escape_brackets(chosen)] {
        let text = format!("[{label}]({destination})");
        let mut probe = src.to_owned();
        probe.replace_range(selection.clone(), &text);
        let expected = selection.start..selection.start + text.len();
        let probe_arena = Arena::new();
        let parsed = parse_document(&probe_arena, &probe, &parse::comrak_options());
        let positions = SourceMap::new(&probe);
        if parsed.descendants().any(|n| {
            matches!(n.data().value, NodeValue::Link(_))
                && positions.range(n) == Some(expected.clone())
                && !n
                    .ancestors()
                    .any(|p| matches!(p.data().value, NodeValue::Image(_) | NodeValue::WikiLink(_)))
        }) && same_structure(&structure, &parse::parse(&probe).blocks)
        {
            let at = selection.start + label.len() + 3;
            return Some(Plan {
                edit: Some(Edit::replace(selection, text)),
                selection: at..at + destination.len(),
                complete_note: false,
            });
        }
    }
    None
}

fn contains(outer: &Range<usize>, inner: &Range<usize>) -> bool {
    if inner.is_empty() {
        outer.start < inner.start && inner.start < outer.end
    } else {
        outer.start <= inner.start && inner.end <= outer.end
    }
}

fn overlaps(outer: &Range<usize>, inner: &Range<usize>) -> bool {
    if inner.is_empty() { contains(outer, inner) } else { outer.start < inner.end && inner.start < outer.end }
}

/// comrak 的行 / 列是 1-based 字节位置，末端含最后一字节。
/// 表格解析先移除 `\|` 的反斜杠，行内节点列号需映射回原始单元格。
struct SourceMap<'s> {
    src: &'s str,
    starts: Vec<usize>,
}

impl<'s> SourceMap<'s> {
    fn new(src: &'s str) -> Self {
        Self { src, starts: std::iter::once(0).chain(src.match_indices('\n').map(|(i, _)| i + 1)).collect() }
    }

    fn range<'a>(&self, node: &'a AstNode<'a>) -> Option<Range<usize>> {
        let src = self.src;
        let sp = node.data().sourcepos;
        let offset = |line: usize, column: usize| self.starts.get(line.checked_sub(1)?).map(|s| s + column);
        let mut start = offset(sp.start.line, sp.start.column.checked_sub(1)?)?;
        let mut end = offset(sp.end.line, sp.end.column)?;
        if let Some(cell) = node.ancestors().skip(1).find(|n| matches!(n.data().value, NodeValue::TableCell))
        {
            let cell = cell.data().sourcepos;
            let base = offset(cell.start.line, cell.start.column.checked_sub(1)?)?;
            let limit = offset(cell.end.line, cell.end.column)?;
            let raw = src.get(base..limit)?;
            let mut map = Vec::with_capacity(raw.len() + 1);
            let mut i = 0;
            while i < raw.len() {
                if raw.as_bytes()[i] == b'\\' && i + 1 < raw.len() {
                    if raw.as_bytes()[i + 1] != b'|' {
                        map.push(base + i);
                    }
                    i += 1;
                }
                map.push(base + i);
                i += 1;
            }
            map.push(limit);
            start = *map.get(start.checked_sub(base)?)?;
            end = *map.get(end.checked_sub(base)?)?;
        }
        src.get(start..end).map(|_| start..end)
    }
}

fn same_structure(before: &[parse::ParsedBlock], after: &[parse::ParsedBlock]) -> bool {
    let signature = |(depth, block): (usize, &parse::ParsedBlock)| {
        (depth, block.kind.clone(), block.attrs.clone(), block.persisted_id.clone())
    };
    parse::ParsedBlock::iter(before).map(signature).eq(parse::ParsedBlock::iter(after).map(signature))
}

/// 仅解析 AST 已确认的显式 `[label](destination "title")` 的地址位置。
fn destination_range(link: &str) -> Option<Range<usize>> {
    let bytes = link.as_bytes();
    if bytes.first() != Some(&b'[') {
        return None;
    }
    let arena = Arena::new();
    let root = parse_document(&arena, link, &parse::comrak_options());
    let positions = SourceMap::new(link);
    let code: Vec<_> = root
        .descendants()
        .filter(|n| matches!(n.data().value, NodeValue::Code(_)))
        .filter_map(|n| positions.range(n))
        .collect();
    let mut depth = 1;
    let mut i = 1;
    while i < bytes.len() {
        if let Some(range) = code.iter().find(|r| r.contains(&i)) {
            i = range.end;
            continue;
        }
        match bytes[i] {
            b'\\' => i += 1,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    if bytes.get(i + 1) != Some(&b'(') {
        return None;
    }
    i += 2;
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    let angle = bytes.get(i) == Some(&b'<');
    if angle {
        i += 1;
    }
    let start = i;
    depth = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'>' if angle => return Some(start..i),
            b'(' if !angle => depth += 1,
            b')' if !angle && depth == 0 => return Some(start..i),
            b')' if !angle => depth -= 1,
            c if !angle && c.is_ascii_whitespace() => return Some(start..i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn escape_brackets(label: &str) -> String {
    // 保留代码片段内的字面括号与已有转义。
    let arena = Arena::new();
    let root = parse_document(&arena, label, &parse::comrak_options());
    let positions = SourceMap::new(label);
    let code: Vec<_> = root
        .descendants()
        .filter(|n| matches!(n.data().value, NodeValue::Code(_)))
        .filter_map(|n| positions.range(n))
        .collect();
    let mut result = String::new();
    let mut escaped = false;
    for (at, c) in label.char_indices() {
        if matches!(c, '[' | ']') && !escaped && !code.iter().any(|r| r.contains(&at)) {
            result.push('\\');
        }
        result.push(c);
        escaped = c == '\\' && !escaped;
    }
    result
}

/// 只自动填入单个常见 URL；普通剪贴板文本不成为地址。
fn clipboard_destination(text: &str) -> Option<String> {
    let text = text.trim();
    if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let url = url::Url::parse(text).ok()?;
    let valid = match url.scheme() {
        "http" | "https" | "ftp" => url.host_str().is_some(),
        "mailto" => !url.path().is_empty() && url.path().contains('@'),
        "file" => url.path().starts_with('/'),
        _ => false,
    };
    if !valid {
        return None;
    }
    let mut result = String::new();
    for c in url.as_str().chars() {
        match c {
            '&' => result.push_str("&amp;"),
            '(' | ')' | '<' | '>' | '[' | ']' | '|' | '\\' | '"' | '\'' => {
                result.push_str(&format!("%{:02X}", c as u32));
            }
            _ => result.push(c),
        }
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selected(src: &str, text: &str) -> Range<usize> {
        let at = src.find(text).unwrap();
        at..at + text.len()
    }

    fn apply(src: &str, plan: &Plan) -> String {
        let mut result = src.to_owned();
        if let Some(edit) = &plan.edit {
            result.replace_range(edit.range.clone(), &edit.text);
        }
        result
    }

    #[test]
    fn inline_link_wrap_preserves_unicode_formatting_and_brackets() {
        for label in ["中文😀", "**粗体**", "`a[b`", "a[b", "a]b", "a\\[b", "a[b `c[d`"] {
            let src = format!("前 {label} 后\n");
            let p = plan(&src, selected(&src, label), None).unwrap();
            let result = apply(&src, &p);
            assert_eq!(&result[p.selection.clone()], "url");
            assert!(result.starts_with("前 [") && result.ends_with("](url) 后\n"), "{result}");
            assert!(!p.complete_note);
            let arena = Arena::new();
            let root = parse_document(&arena, &result, &parse::comrak_options());
            assert_eq!(
                root.descendants().filter(|n| matches!(n.data().value, NodeValue::Link(_))).count(),
                1
            );
        }
        let src = "# 标题\n\n> [!note]\n> 中文 **粗体**\n";
        let p = plan(src, selected(src, "**粗体**"), None).unwrap();
        assert_eq!(apply(src, &p), "# 标题\n\n> [!note]\n> 中文 [**粗体**](url)\n");
    }

    #[test]
    fn inline_link_selects_existing_destination_without_touching_label_or_title() {
        for (link, url) in [
            ("[**文本**](https://host/a(b) \"标题\")", "https://host/a(b)"),
            ("[文](<https://host/a b> '标题')", "https://host/a b"),
            ("[文](path\\(a\\) \"title\")", "path\\(a\\)"),
            ("[文]()", ""),
            ("[文](<> \"title only\")", ""),
            ("[a\\]b](url)", "url"),
            ("[`a[b`](url)", "url"),
        ] {
            let src = format!("前 {link} 后");
            let at = "前 [".len();
            let p = plan(&src, at..at, Some("https://clipboard.test")).unwrap_or_else(|| panic!("{src}"));
            assert!(p.edit.is_none());
            assert_eq!(&src[p.selection], url, "{src}");
        }
        let src = "正文 [[note#标题|显示]] 后";
        let at = src.find("显示").unwrap();
        let p = plan(src, at..at, None).unwrap();
        assert_eq!(&src[p.selection], "note");
        assert!(p.edit.is_none());
    }

    #[test]
    fn inline_link_clipboard_urls_are_validated_and_markdown_encoded() {
        let p = plan("中文", 0..6, Some(" https://example.com/a(b)?x=1&y=a|b ")).unwrap();
        let result = apply("中文", &p);
        assert_eq!(result, "[中文](https://example.com/a%28b%29?x=1&amp;y=a%7Cb)");
        assert_eq!(&result[p.selection], "https://example.com/a%28b%29?x=1&amp;y=a%7Cb");
        for invalid in [
            "hello",
            "https://",
            "https://host/one two",
            "https://host\nhttps://other",
            "javascript:alert(1)",
            "mailto:",
        ] {
            assert!(clipboard_destination(invalid).is_none(), "{invalid}");
        }
        for valid in [
            "https://example.com",
            "http://localhost:8080/a#frag",
            "mailto:a@example.com",
            "file:///tmp/note.md",
            "ftp://host/a",
        ] {
            assert!(clipboard_destination(valid).is_some(), "{valid}");
        }
    }

    #[test]
    fn inline_link_table_source_ranges_preserve_escaped_pipes_and_neighbors() {
        let src = "| A | B |\n| --- | --- |\n| a\\|b [中\\|文](<https://host/x> 'T') | [B](url) |\n";
        for (label, url) in [("中", "https://host/x"), ("B](url)", "url")] {
            let at = src.find(label).unwrap();
            let p = plan(src, at..at, None).unwrap();
            assert_eq!(&src[p.selection], url);
            assert!(p.edit.is_none());
        }
        let p = plan(src, selected(src, "a\\|b"), Some("https://host/a|b")).unwrap();
        assert_eq!(apply(src, &p), src.replacen("a\\|b", "[a\\|b](https://host/a%7Cb)", 1));
    }

    #[test]
    fn inline_link_rejects_protected_contexts_crossing_links_and_structural_selections() {
        for (src, text) in [
            ("`code`", "ode"),
            ("```\ncode\n```", "code"),
            ("$math$", "math"),
            ("![image](url)", "image"),
            ("![[embed]]", "embed"),
            ("[ref][id]\n\n[id]: https://host", "ref"),
            ("<https://host>", "host"),
            ("<span title=\"name\">x</span>", "name"),
            ("---\ntitle: name\n---\n", "name"),
            ("one\ntwo", "one\ntwo"),
            ("one [two](url)", "one [two]"),
            ("# Heading", "# Heading"),
            ("> Quote", "> Quote"),
            ("- item", "- item"),
            ("> [!note]\n> body", "[!note]"),
            ("body ^block-id", "^block-id"),
            ("body {.red}", "{.red}"),
        ] {
            assert!(plan(src, selected(src, text), None).is_none(), "{src}");
        }
        for src in [
            "`co|de`",
            "```\nco|de\n```",
            "$ma|th$",
            "![[em|bed]]",
            "https://exa|mple.com",
            "<b title='na|me'>",
        ] {
            let at = src.find('|').unwrap();
            let src = src.replace('|', "");
            assert!(plan(&src, at..at, None).is_none(), "{src}");
        }
        assert!(plan("中文", 1..3, None).is_none());
    }

    #[test]
    fn inline_link_empty_selection_starts_wikilink_without_overwriting_clipboard() {
        for (src, at) in [("正文 后", 6), ("", 0), ("# 标题", 8), ("> 正文", 8)] {
            let p = plan(src, at..at, Some("https://host")).unwrap();
            assert!(p.complete_note);
            let result = apply(src, &p);
            assert_eq!(&result[p.selection.start - 2..p.selection.end + 2], "[[]]");
        }
        assert!(plan("\\", 1..1, None).is_none());
    }
}
