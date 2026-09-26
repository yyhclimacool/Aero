//! 聚焦块的标记隐藏（方案 9.1）。源码不变，只告诉界面哪些字节不要画出来、剩下的字用什么样式。
//!
//! 配对完成的标记始终藏着，光标停在旁边也不露出来：浏览与编辑是同一副样子。
//! 没配对上的标记解析器不会认成格式，所以正在输入的 `*`、`[` 本来就看得见。

use std::ops::Range;

use comrak::nodes::{AstNode, NodeValue};
use comrak::{Arena, parse_document};

use crate::colors::{self, Slot};
use crate::inline_links::SourceMap;
use crate::parse;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mark {
    Bold,
    Italic,
    Strike,
    Code,
    Highlight,
    Link,
    /// 调色板类名，文字色是 `red`，背景色是 `bg-blue`。
    Palette(Vec<String>),
}

pub struct Analysis {
    pub conceal: Vec<Range<usize>>,
    pub hard_breaks: Vec<Range<usize>>,
    pub styles: Vec<(Range<usize>, Mark)>,
    /// 链接文字的区间与地址。笔记链接的地址是 [`crate::links::encode_note_url`] 的结果。
    pub links: Vec<(Range<usize>, String)>,
}

impl Analysis {
    /// `offset` 处的字节是否藏起来了。
    pub fn conceals(&self, offset: usize) -> bool {
        self.conceal.iter().any(|range| range.contains(&offset))
    }

    /// 源码去掉隐藏字节后显示出来的文字。
    pub fn visible_text(&self, src: &str) -> String {
        src.char_indices()
            .filter_map(|(i, c)| {
                if self.hard_breaks.iter().any(|range| range.start == i) {
                    Some('\n')
                } else {
                    (!self.conceals(i)).then_some(c)
                }
            })
            .collect()
    }

    /// 成对标记包住的正文区间（加粗、链接文字等）。删字删到只剩标记时要连标记一起删。
    pub fn spans(&self) -> impl Iterator<Item = &Range<usize>> {
        self.styles.iter().map(|(range, _)| range)
    }
}

pub fn analyze(src: &str) -> Analysis {
    analyze_editing(src, None)
}

/// 同 [`analyze`]，但 `selection` 落在某个链接的 `(地址)` 里时，那段地址露出来：⌘K 改地址时要看得见在改什么。
pub fn analyze_editing(src: &str, selection: Option<Range<usize>>) -> Analysis {
    let arena = Arena::new();
    let root = parse_document(&arena, src, &parse::comrak_options());
    let positions = SourceMap::new(src);
    let wikilinks = crate::links::scan(src);
    let pointers = crate::links::pointers(src);
    let mut out =
        Analysis { conceal: Vec::new(), hard_breaks: Vec::new(), styles: Vec::new(), links: Vec::new() };
    let mut code = Vec::new();

    for node in root.descendants() {
        let Some(mut range) = positions.range(node) else { continue };
        if matches!(node.data().value, NodeValue::Code(_))
            && let Some(text) = src.get(range.clone())
            && let Some(brace) = text.rfind('{')
        {
            let at = range.start + brace;
            if colors::attrs_at(src, at).is_some_and(|(_, end)| end == range.end) {
                range.end = at;
            }
        }
        if matches!(node.data().value, NodeValue::Code(_)) {
            code.push(range.clone());
        }
        mark_node(src, &range, node, &wikilinks, &pointers, selection.as_ref(), &mut out);
    }
    line_prefixes(src, &code, &mut out.conceal);
    color_marks(src, &code, &mut out);
    escapes(src, &code, &mut out.conceal);
    out.conceal.sort_by_key(|range| range.start);
    out
}

fn mark_node(
    src: &str,
    range: &Range<usize>,
    node: &AstNode,
    wikilinks: &[crate::links::LinkSpan],
    pointers: &[crate::links::LinkSpan],
    selection: Option<&Range<usize>>,
    out: &mut Analysis,
) {
    let Some(text) = src.get(range.clone()) else { return };
    let paired_mark = |markers: &[&str], mark: Mark, out: &mut Analysis| {
        if let Some((open, inner, close)) = paired(range, text, markers) {
            out.conceal.push(open);
            out.conceal.push(close);
            out.styles.push((inner, mark));
        }
    };
    match &node.data().value {
        NodeValue::Strong => paired_mark(&["**", "__"], Mark::Bold, out),
        NodeValue::Emph => paired_mark(&["*", "_"], Mark::Italic, out),
        NodeValue::Strikethrough => paired_mark(&["~~"], Mark::Strike, out),
        NodeValue::Highlight => paired_mark(&["=="], Mark::Highlight, out),
        NodeValue::Code(_) => {
            // 还没配对的 ` 解析器不会当成代码，正在输入时仍然看得见。
            if let Some((open, inner, close)) = code_delimiters(range, text) {
                if !open.is_empty() {
                    out.conceal.push(open);
                }
                if !close.is_empty() {
                    out.conceal.push(close);
                }
                out.styles.push((inner, Mark::Code));
            }
        }
        NodeValue::WikiLink(_) => mark_wikilink(range, text, wikilinks, pointers, selection, out),
        NodeValue::Link(link) => mark_link(range, text, &link.url, selection, out),
        NodeValue::Image(link) => mark_link(range, text, &link.url, selection, out),
        NodeValue::HtmlInline(_) | NodeValue::HtmlBlock(_) => {
            let tag = text.trim();
            if tag.strip_prefix('<').and_then(|s| s.strip_suffix('>')).is_some_and(|name| {
                name.trim_end().strip_suffix('/').unwrap_or(name).trim_end().eq_ignore_ascii_case("br")
            }) {
                let start = range.start + text.len() - text.trim_start().len();
                let range = start..start + tag.len();
                out.conceal.push(range.clone());
                out.hard_breaks.push(range);
            }
        }
        _ => {}
    }
}

fn paired(
    range: &Range<usize>,
    text: &str,
    markers: &[&str],
) -> Option<(Range<usize>, Range<usize>, Range<usize>)> {
    let marker = markers.iter().find(|marker| text.starts_with(*marker) && text.ends_with(*marker))?;
    let n = marker.len();
    if text.len() < n * 2 {
        return None;
    }
    Some((range.start..range.start + n, range.start + n..range.end - n, range.end - n..range.end))
}

fn code_delimiters(range: &Range<usize>, text: &str) -> Option<(Range<usize>, Range<usize>, Range<usize>)> {
    let open_len = text.bytes().take_while(|b| *b == b'`').count();
    if open_len == 0 || text.len() < open_len * 2 {
        return None;
    }
    let close_at = text.rfind('`')?;
    let mut close_start = close_at;
    while close_start > open_len && text.as_bytes()[close_start - 1] == b'`' {
        close_start -= 1;
    }
    if close_start < open_len {
        return None;
    }
    Some((
        range.start..range.start + open_len,
        range.start + open_len..range.start + close_start,
        range.start + close_start..range.end.min(range.start + close_start + open_len),
    ))
}

fn mark_wikilink(
    range: &Range<usize>,
    text: &str,
    wikilinks: &[crate::links::LinkSpan],
    pointers: &[crate::links::LinkSpan],
    selection: Option<&Range<usize>>,
    out: &mut Analysis,
) {
    let bang = text.starts_with("![[");
    let open = if bang { "![[" } else { "[[" };
    if !text.starts_with(open) || !text.ends_with("]]") {
        return;
    }
    let inner_start = range.start + open.len();
    let inner_end = range.end - 2;
    let inner = text.get(open.len()..text.len() - 2).unwrap_or("");
    let (label_start, label_end) = if let Some(pipe) = inner.find('|') {
        (inner_start + pipe + 1, inner_end)
    } else {
        (inner_start, inner_end)
    };
    out.conceal.push(range.start..label_start);
    out.conceal.push(label_end..range.end);
    if let Some(span) = pointers.iter().find(|span| span.range.start == range.start)
        && let Some(pointer) = &span.pointer
    {
        let editing_destination =
            selection.is_some_and(|selection| selection.end > range.end && selection.start < span.range.end);
        if !editing_destination {
            out.conceal.push(range.end..span.range.end);
        }
        if label_start < label_end {
            out.styles.push((label_start..label_end, Mark::Link));
            let url = crate::links::encode_vault_url(&pointer.path, pointer.fragment.as_deref());
            out.links.push((label_start..label_end, url));
        }
        return;
    }
    if label_start < label_end {
        out.styles.push((label_start..label_end, Mark::Link));
        let span = wikilinks.iter().find(|span| {
            span.range == *range
                || (bang && span.range.start == range.start + 1 && span.range.end == range.end)
        });
        if let Some(span) = span {
            let url = crate::links::encode_note_url(&span.target, span.fragment.as_deref());
            out.links.push((label_start..label_end, url));
        }
    }
}

fn mark_link(
    range: &Range<usize>,
    text: &str,
    url: &str,
    selection: Option<&Range<usize>>,
    out: &mut Analysis,
) {
    let rest = text.strip_prefix('!').unwrap_or(text);
    let bang = rest.len() != text.len();
    if !rest.starts_with('[') {
        // `<https://…>` 藏起尖括号；裸网址本身就是文字。
        let label = if rest.starts_with('<') && rest.ends_with('>') && rest.len() > 2 {
            out.conceal.push(range.start..range.start + 1);
            out.conceal.push(range.end - 1..range.end);
            range.start + 1..range.end - 1
        } else {
            range.clone()
        };
        out.styles.push((label.clone(), Mark::Link));
        out.links.push((label, url.to_owned()));
        return;
    }
    let open_at = range.start + usize::from(bang);
    let bytes = text.as_bytes();
    let mut depth = 0;
    let mut close = None;
    for (i, byte) in bytes.iter().enumerate().skip(usize::from(bang)) {
        if *byte == b'[' {
            depth += 1;
        } else if *byte == b']' {
            depth -= 1;
            if depth == 0 {
                close = Some(i);
                break;
            }
        }
    }
    let Some(close) = close else { return };
    let label_start = open_at + 1;
    let label_end = range.start + close;
    out.conceal.push(range.start..label_start);
    let editing_destination =
        selection.is_some_and(|selection| selection.start > label_end && selection.end < range.end);
    if !editing_destination {
        out.conceal.push(label_end..range.end);
    }
    if label_start < label_end {
        out.styles.push((label_start..label_end, Mark::Link));
        out.links.push((label_start..label_end, url.to_owned()));
    }
}

fn line_prefixes(src: &str, code: &[Range<usize>], conceal: &mut Vec<Range<usize>>) {
    let mut offset = 0;
    for line in src.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        if let Some(len) = structural_prefix_len(body) {
            let range = offset..offset + len;
            let in_code = code.iter().any(|code| code.start <= range.start && range.end <= code.end);
            if !in_code && !range.is_empty() {
                conceal.push(range);
            }
        }
        offset += line.len();
    }
}

fn color_marks(src: &str, code: &[Range<usize>], out: &mut Analysis) {
    let mut search = 0;
    while let Some(rel) = src[search..].find('{') {
        let open = search + rel;
        let Some((attrs, end)) = colors::attrs_at(src, open) else {
            search = open + 1;
            continue;
        };
        search = end;
        if !colors::has_color(&attrs) {
            continue;
        }
        if code.iter().any(|code| code.start <= open && end <= code.end) {
            continue;
        }
        let classes: Vec<String> = attrs
            .classes
            .iter()
            .filter(|class| Slot::Text.owns(class) || Slot::Background.owns(class))
            .cloned()
            .collect();
        if open > 0 && src.as_bytes()[open - 1] == b']' {
            if let Some(bracket) = colors::bracket_start(src, open - 1, colors::line_start(src, open)) {
                out.conceal.push(bracket..bracket + 1);
                out.conceal.push(open - 1..end);
                let content = bracket + 1..open - 1;
                if content.start < content.end && !classes.is_empty() {
                    out.styles.push((content, Mark::Palette(classes)));
                }
                continue;
            }
        }
        out.conceal.push(open..end);
    }
}

/// `\*` 这类转义：反斜杠只是为了让后面的符号不当标记，界面上只留那个符号。
fn escapes(src: &str, code: &[Range<usize>], conceal: &mut Vec<Range<usize>>) {
    let bytes = src.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'\\' && bytes[i + 1].is_ascii_punctuation() {
            let hidden = conceal.iter().any(|range| range.contains(&i));
            let in_code = code.iter().any(|range| range.contains(&i));
            if !hidden && !in_code {
                conceal.push(i..i + 1);
            }
            i += 2;
            continue;
        }
        i += 1;
    }
}

/// 行首的块标记长度。`# `、`> `，以及 `> [!tip]` / `> [!tip]- ` 这种 callout 头。
fn structural_prefix_len(line: &str) -> Option<usize> {
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    if (1..=6).contains(&hashes) && line.as_bytes().get(hashes) == Some(&b' ') {
        return Some(hashes + 1);
    }
    if let Some(len) = callout_marker_len(line) {
        return Some(len);
    }
    if line.starts_with("> ") {
        Some(2)
    } else if line == ">" {
        Some(1)
    } else {
        None
    }
}

fn callout_marker_len(line: &str) -> Option<usize> {
    let rest = line.strip_prefix("> ")?;
    if !rest.starts_with("[!") {
        return None;
    }
    let close = rest.find(']')?;
    let mut end = 2 + close + 1;
    let bytes = line.as_bytes();
    if matches!(bytes.get(end), Some(b'-' | b'+')) {
        end += 1;
    }
    if bytes.get(end) == Some(&b' ') {
        end += 1;
    }
    crate::parse::callout_header(line)?;
    Some(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hidden(src: &str) -> String {
        analyze(src).visible_text(src)
    }

    #[test]
    fn paired_markers_stay_hidden() {
        assert_eq!(hidden("say **bold** please"), "say bold please");
        assert_eq!(hidden("# Title"), "Title");
        assert_eq!(hidden("> [!tip] 注意"), "注意");
        assert_eq!(hidden("see [[note|label]] now"), "see label now");
        assert_eq!(hidden("see `code` now"), "see code now");
        assert_eq!(hidden("运行 `aero 路径"), "运行 `aero 路径");
        assert_eq!(hidden("先输入 `"), "先输入 `");
        assert_eq!(hidden("a [word]{.red} b"), "a word b");
        assert_eq!(hidden("a [word]{.blue} b"), "a word b");
        assert_eq!(hidden("use `a **b**` here"), "use a **b** here");
        assert_eq!(hidden("你的 **Aero 示例库**。[它](url) 后面"), "你的 Aero 示例库。它 后面");
        assert_eq!(hidden("> 第一行\n> 第二行"), "第一行\n第二行");
        assert_eq!(hidden("> 第一行\n>\n> 第三行"), "第一行\n\n第三行");
        assert_eq!(
            hidden("[[目标|别名]] 和 [外部](https://example.com)，还有 [红色]{.red} 这"),
            "别名 和 外部，还有 红色 这"
        );
        assert_eq!(
            hidden(
                "第一段有 **加粗文字**、*斜体*、`行内代码`、~~删除线~~、==高亮==，以及 [[目标笔记|双链别名]] 和 [外部链接](https://example.com)，还有 [红色文字]{.red} 这种颜色。"
            ),
            "第一段有 加粗文字、斜体、行内代码、删除线、高亮，以及 双链别名 和 外部链接，还有 红色文字 这种颜色。"
        );
    }

    #[test]
    fn html_breaks_are_displayed_as_newlines_without_exposing_tags() {
        let source = "**第一行<br>第二行**<BR/><br />末行";
        assert_eq!(hidden(source), "第一行\n第二行\n\n末行");
        assert_eq!(hidden("<br>末行<br>"), "\n末行\n");
        assert_eq!(hidden("1. **标题**<br>    <br>2. **标题**"), "1. 标题\n    \n2. 标题");
    }

    #[test]
    fn break_like_literals_do_not_turn_into_newlines() {
        assert_eq!(hidden("`<br>` 与 \\<br> 与 <bracket>"), "<br> 与 <br> 与 <bracket>");
    }

    #[test]
    fn links_escapes_and_autolinks() {
        assert_eq!(hidden(r"1\. 不是列表 \*星号\*"), "1. 不是列表 *星号*");
        assert_eq!(hidden("`\\*` 在代码里"), "\\* 在代码里");
        assert_eq!(hidden("见 <https://example.com> 吧"), "见 https://example.com 吧");
        let src = "看 [[目标#段落|这里]] 和 [站点](https://example.com)";
        let analysis = analyze(src);
        let links: Vec<(&str, &str)> =
            analysis.links.iter().map(|(range, url)| (&src[range.clone()], url.as_str())).collect();
        assert_eq!(
            links,
            vec![
                ("这里", crate::links::encode_note_url("目标", Some("段落")).as_str()),
                ("站点", "https://example.com")
            ]
        );
    }
}
