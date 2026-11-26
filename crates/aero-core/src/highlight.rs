//! `==...==` 与行内属性列表（6.3.10）的显示适配。源码始终保留 Markdown，HTML 仅供原生富文本预览。
//!
//! 颜色的具体色值是主题的事，这里只把承载颜色的类名写进 `<span class="red bg-blue">`，
//! 调用方按 13.3 的 `content.*` 令牌换成实际颜色。

use std::borrow::Cow;
use std::ops::Range;

use comrak::{
    Arena,
    nodes::{AstNode, NodeValue},
    parse_document,
};

use crate::{colors, inline_links::SourceMap, parse};

/// 高亮正文的源字节范围；使用与编辑 / 索引相同的解析器排除代码、转义、URL 等。
pub fn ranges(src: &str) -> Vec<Range<usize>> {
    if !src.contains("==") {
        return Vec::new();
    }
    let arena = Arena::new();
    let root = parse_document(&arena, src, &parse::comrak_options());
    ranges_in(root, &SourceMap::new(src))
}

pub(crate) fn ranges_in<'a>(root: &'a AstNode<'a>, positions: &SourceMap) -> Vec<Range<usize>> {
    root.descendants()
        .filter(|n| matches!(n.data().value, NodeValue::Highlight))
        .filter_map(|n| {
            let r = positions.range(n)?;
            (r.len() > 4).then_some(r.start + 2..r.end - 2)
        })
        .collect()
}

pub enum Preview {
    Html(Vec<String>),
    Table { alignments: Vec<Alignment>, rows: Vec<Vec<Cell>> },
}

pub enum Alignment {
    Left,
    Center,
    Right,
}

pub struct Cell {
    pub html: Vec<String>,
    pub text: String,
}

/// 块的富文本 HTML。只有块里真的有 `==` 或行内属性列表时才返回，否则调用方按普通 Markdown 渲染。
pub fn preview(src: &str, range: Range<usize>) -> Option<Preview> {
    let text = src.get(range.clone())?;
    if !text.contains("==") && !text.contains('{') {
        return None;
    }
    let mut options = parse::comrak_options();
    options.render.sourcepos = false;
    // 第一遍：这个块要不要富文本渲染，以及行内属性挂在哪些文字上
    let carriers = {
        let arena = Arena::new();
        let root = parse_document(&arena, src, &options);
        let node = block_node(root, src, &range)?;
        let carriers = render_carriers(src, &range, node);
        if carriers.is_empty() && !node.descendants().any(|n| matches!(n.data().value, NodeValue::Highlight))
        {
            return None;
        }
        carriers
    };
    // 属性列表本身不进渲染：先换成哨兵，重新解析，最后在 HTML 上换成 `<span class>`。
    // 哨兵用私有使用区字符，Markdown 不解析它、HTML 也不转义它。
    let (source, range) = match carriers.is_empty() {
        true => (Cow::Borrowed(src), range),
        false => {
            let (rewritten, range) = rewrite(src, &range, &carriers);
            (Cow::Owned(rewritten), range)
        }
    };
    let src = source.as_ref();
    let arena = Arena::new();
    let root = parse_document(&arena, src, &options);
    let node = block_node(root, src, &range)?;
    for child in node.descendants() {
        let mut data = child.data_mut();
        if let NodeValue::HtmlInline(raw) = &data.value
            && matches!(raw.to_ascii_lowercase().as_str(), "<br>" | "<br/>" | "<br />")
        {
            data.value = NodeValue::LineBreak;
        }
    }
    let html = |node| {
        let mut lines = split_lines(node, &arena)
            .into_iter()
            .map(|line| {
                if matches!(line.data().value, NodeValue::TableCell) {
                    line.data_mut().value = NodeValue::Paragraph;
                }
                let mut html = String::new();
                comrak::format_html(line, &options, &mut html).ok()?;
                Some(html)
            })
            .collect::<Option<Vec<_>>>()?;
        paint(&mut lines, &carriers);
        Some(lines)
    };
    if let NodeValue::Table(table) = &node.data().value {
        let alignments = table
            .alignments
            .iter()
            .map(|align| match align {
                comrak::nodes::TableAlignment::Center => Alignment::Center,
                comrak::nodes::TableAlignment::Right => Alignment::Right,
                _ => Alignment::Left,
            })
            .collect();
        let rows = node
            .children()
            .map(|row| {
                row.children()
                    .map(|cell| {
                        let content = html(cell)?;
                        let text: String = cell
                            .descendants()
                            .filter_map(|child| match &child.data().value {
                                NodeValue::Text(text) => Some(text.to_string()),
                                NodeValue::Code(code) => Some(code.literal.clone()),
                                NodeValue::LineBreak | NodeValue::SoftBreak => Some("\n".to_owned()),
                                _ => None,
                            })
                            .collect();
                        // 列宽按可见文字量，哨兵不是文字
                        let text = text.replace(is_sentinel, "");
                        Some(Cell { html: content, text })
                    })
                    .collect::<Option<Vec<_>>>()
            })
            .collect::<Option<Vec<_>>>()?;
        return Some(Preview::Table { alignments, rows });
    }
    Some(Preview::Html(html(node)?))
}

/// 块在 AST 里的节点。渲染的单位与 `preview` 的调用方一致：段落、标题、表格。
fn block_node<'a>(root: &'a AstNode<'a>, src: &str, range: &Range<usize>) -> Option<&'a AstNode<'a>> {
    let positions = SourceMap::new(src);
    root.descendants().find(|node| {
        matches!(node.data().value, NodeValue::Paragraph | NodeValue::Heading(_) | NodeValue::Table(_))
            && positions.range(node).as_ref() == Some(range)
    })
}

/// 哨兵：私有使用区字符，Markdown 不解析、HTML 不转义。第 i 处的开标签是 `OPEN + i`，闭标签固定。
const OPEN: char = '\u{E000}';
const CLOSE: char = '\u{E0FF}';
/// 一个块里最多处理的承载者数量（哨兵一处一个字符），超出的不上色，内容照常渲染。
const MAX_CARRIERS: usize = 0xFF;

fn is_sentinel(c: char) -> bool {
    (OPEN..=CLOSE).contains(&c)
}

/// 一处行内属性列表（6.3.10）：`[文本]{...}`，或紧跟行内元素的 `{...}`。
pub(crate) struct Carrier {
    /// 上色的正文
    pub content: Range<usize>,
    /// 渲染时要隐藏的两段标记：`[`（元素形式为空区间）与 `]{...}` / `{...}`
    pub before: Range<usize>,
    pub after: Range<usize>,
    pub attrs: crate::Attrs,
}

impl Carrier {
    pub(crate) fn full_range(&self) -> Range<usize> {
        self.before.start..self.after.end
    }

    pub(crate) fn attrs_range(&self) -> Range<usize> {
        self.after.start + usize::from(!self.before.is_empty())..self.after.end
    }
}

/// 一个块里的行内属性承载者，按出现顺序。代码、数学、HTML 里的 `{...}` 不算属性列表。
pub(crate) fn carriers<'a>(
    src: &str,
    range: &Range<usize>,
    block: &'a AstNode<'a>,
    positions: &SourceMap,
) -> Vec<Carrier> {
    let Some(text) = src.get(range.clone()).filter(|text| text.contains('{')) else {
        return Vec::new();
    };
    let mut protected: Vec<Range<usize>> = Vec::new();
    let mut out: Vec<Carrier> = Vec::new();
    for node in block.descendants() {
        let data = node.data();
        let Some(node_range) = positions.range(node) else { continue };
        // 行内代码 / 链接的属性列表 comrak 自己解析掉了，节点区间含 `{...}`，正文要去掉这一段
        let attrs = data
            .attrs
            .as_ref()
            .and_then(|_| src.get(node_range.clone())?.rfind('{'))
            .map(|at| node_range.start + at)
            .filter(|&at| colors::attrs_at(src, at).is_some_and(|(_, end)| end == node_range.end));
        let content = node_range.start..attrs.unwrap_or(node_range.end);
        if matches!(
            data.value,
            NodeValue::Code(_)
                | NodeValue::CodeBlock(_)
                | NodeValue::Math(_)
                | NodeValue::HtmlInline(_)
                | NodeValue::HtmlBlock(_)
                | NodeValue::WikiLink(_)
                | NodeValue::FrontMatter(_)
        ) {
            protected.push(content.clone());
        }
        if !is_inline_element(&data.value) {
            continue;
        }
        // 元素形式：属性紧跟行内元素之后（`**粗**{.blue}`），或已经被 comrak 收进节点（`` `码`{.red} ``、链接）
        let after = content.end;
        if content.start >= range.start
            && let Some((attrs, end)) = colors::attrs_at(src, after)
            && end <= range.end
        {
            out.push(Carrier {
                content: content.clone(),
                before: content.start..content.start,
                after: after..end,
                attrs,
            });
        }
    }
    // `[文本]{...}` 形式：`]` 紧跟属性列表，向前找配对的 `[`
    for (at, _) in text.match_indices(']') {
        let close = range.start + at;
        if escaped(src, close) {
            continue;
        }
        let Some((attrs, end)) = colors::attrs_at(src, close + 1) else { continue };
        let Some(open) = colors::bracket_start(src, close, range.start) else { continue };
        if open + 1 == close || end > range.end {
            continue;
        }
        out.push(Carrier { content: open + 1..close, before: open..open + 1, after: close..end, attrs });
    }
    out.retain(|c| !protected.iter().any(|p| overlaps(p, &c.before) || overlaps(p, &c.after)));
    out.sort_by_key(|c| (c.content.start, std::cmp::Reverse(c.content.end)));
    // 同一段 `{...}` 可能被两种形式同时认出（引用式链接的 `[文本]{.red}` 既是 `[ ]` 又是链接元素）：
    // 只留最外面那一处，改写用的标记区间之间不能重叠。外层先出现，靠 sort 的顺序留下。
    let mut kept: Vec<Carrier> = Vec::with_capacity(out.len());
    for c in out {
        let clash = kept
            .iter()
            .any(|k| [&k.before, &k.after].iter().any(|k| overlaps(k, &c.before) || overlaps(k, &c.after)));
        if !clash {
            kept.push(c);
        }
    }
    kept
}

fn render_carriers<'a>(src: &str, range: &Range<usize>, block: &'a AstNode<'a>) -> Vec<Carrier> {
    // Leave literal private-use characters alone when they coincide with the HTML sentinels.
    if src.get(range.clone()).is_none_or(|text| text.contains(is_sentinel)) {
        return Vec::new();
    }
    let mut found = carriers(src, range, block, &SourceMap::new(src));
    found.truncate(MAX_CARRIERS);
    found
}

/// 能承载属性列表的行内元素（6.3.10：强调、代码、链接、`==高亮==` 等）。
pub(crate) fn is_inline_element(value: &NodeValue) -> bool {
    matches!(
        value,
        NodeValue::Strong
            | NodeValue::Emph
            | NodeValue::Strikethrough
            | NodeValue::Highlight
            | NodeValue::Underline
            | NodeValue::Superscript
            | NodeValue::Subscript
            | NodeValue::Code(_)
            | NodeValue::Link(_)
            | NodeValue::Image(_)
            | NodeValue::Math(_)
            | NodeValue::WikiLink(_)
    )
}

/// 属性列表里归我们管的类名，按 `文字 背景` 的顺序拼成 `class` 属性的值。
fn classes(attrs: &crate::block::Attrs) -> String {
    let mut out = Vec::new();
    for slot in [colors::Slot::Text, colors::Slot::Background] {
        if let Some(color) = slot.color_of(attrs) {
            out.push(slot.class(&colors::normalize_custom(color).unwrap_or_else(|| color.into())));
        }
    }
    out.join(" ")
}

fn escaped(src: &str, at: usize) -> bool {
    src[..at].chars().rev().take_while(|&c| c == '\\').count() % 2 == 1
}

fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

/// 把属性列表的标记换成哨兵，返回改写后的源码与块的新区间。
fn rewrite(src: &str, range: &Range<usize>, carriers: &[Carrier]) -> (String, Range<usize>) {
    let mut marks: Vec<(Range<usize>, String)> = Vec::with_capacity(carriers.len() * 2);
    for (ix, c) in carriers.iter().enumerate() {
        let open = char::from_u32(OPEN as u32 + ix as u32).expect("私有使用区");
        marks.push((c.before.clone(), open.to_string()));
        marks.push((c.after.clone(), CLOSE.to_string()));
    }
    marks.sort_by_key(|(r, _)| r.start);
    let mut out = String::with_capacity(src.len() + marks.len() * 3);
    let mut at = 0;
    let mut delta = 0isize;
    for (r, text) in marks {
        // 区间重叠本不该发生（`carriers` 已经去重）；真遇上就跳过这一处，不拿 panic 赌
        if r.start < at {
            continue;
        }
        out.push_str(&src[at..r.start]);
        out.push_str(&text);
        delta += text.len() as isize - r.len() as isize;
        at = r.end;
    }
    out.push_str(&src[at..]);
    let end = (range.end as isize + delta) as usize;
    (out, range.start..end)
}

/// 把哨兵换回 `<span class="...">`。span 跨硬换行时每行各自开合，HTML 才是配对的。
fn paint(lines: &mut [String], carriers: &[Carrier]) {
    if carriers.is_empty() {
        return;
    }
    let colors: Vec<_> = carriers.iter().map(|c| classes(&c.attrs)).collect();
    let tag = |ix: usize| match colors.get(ix) {
        Some(classes) if !classes.is_empty() => format!("<span class=\"{classes}\">"),
        _ => String::new(),
    };
    let end_tag = |ix: usize| match colors.get(ix) {
        Some(classes) if !classes.is_empty() => "</span>",
        _ => "",
    };
    let mut carried: Vec<usize> = Vec::new();
    for line in lines {
        let mut open = carried.clone();
        let mut out = String::with_capacity(line.len() + 32);
        // 上一行没闭合的 span 接着开，插在 `<p>` 之类的块级开标签之后
        let head = if carried.is_empty() { 0 } else { line.find('>').map_or(0, |i| i + 1) };
        out.push_str(&line[..head]);
        out.extend(carried.iter().map(|&ix| tag(ix)));
        let mut rest = &line[head..];
        while let Some(at) = rest.find(is_sentinel) {
            out.push_str(&rest[..at]);
            let ch = rest[at..].chars().next().expect("哨兵");
            rest = &rest[at + ch.len_utf8()..];
            if ch == CLOSE {
                if let Some(ix) = open.pop() {
                    out.push_str(end_tag(ix));
                }
            } else {
                let ix = ch as u32 - OPEN as u32;
                out.push_str(&tag(ix as usize));
                open.push(ix as usize);
            }
        }
        // 行尾仍开着的 span 在块级闭标签之前收掉，下一行再打开
        let tail = rest.rfind('<').unwrap_or(rest.len());
        out.push_str(&rest[..tail]);
        out.extend(open.iter().rev().map(|&ix| end_tag(ix)));
        out.push_str(&rest[tail..]);
        carried = open;
        *line = out;
    }
}

/// 整篇笔记导出成 HTML。行内颜色属性按预览同一套规则变成 `<span class>`，标记本身不出现。
pub fn export_html(src: &str) -> String {
    let mut options = parse::comrak_options();
    options.render.sourcepos = false;
    let range = 0..src.len();
    let found = {
        let arena = Arena::new();
        let root = parse_document(&arena, src, &options);
        render_carriers(src, &range, root)
    };
    let source = if found.is_empty() { src.to_owned() } else { rewrite(src, &range, &found).0 };
    let arena = Arena::new();
    let root = parse_document(&arena, &source, &options);
    let mut html = String::new();
    if comrak::format_html(root, &options, &mut html).is_err() {
        return String::new();
    }
    let mut lines = vec![html];
    paint(&mut lines, &found);
    lines.pop().unwrap_or_default()
}

/// 段落里的行内公式 `$…$`（不含 `$$` 行间公式）。区间含定界符。
pub struct InlineFormula {
    pub range: Range<usize>,
    pub latex: String,
}

pub fn inline_formulas(src: &str) -> Vec<InlineFormula> {
    if !src.contains('$') {
        return Vec::new();
    }
    let options = parse::comrak_options();
    let arena = Arena::new();
    let root = parse_document(&arena, src, &options);
    let positions = SourceMap::new(src);
    let mut out = Vec::new();
    for node in root.descendants() {
        let NodeValue::Math(math) = &node.data().value else { continue };
        if math.display_math || math.literal.trim().is_empty() {
            continue;
        }
        let Some(range) = positions.range(node) else { continue };
        out.push(InlineFormula { range, latex: math.literal.trim().to_owned() });
    }
    out
}

// TextView 吞掉嵌套 br；按硬换行拆分并复制祖先，保留每行的强调和链接。
fn split_lines<'a>(node: &'a AstNode<'a>, arena: &'a Arena<'a>) -> Vec<&'a AstNode<'a>> {
    if matches!(node.data().value, NodeValue::LineBreak) {
        return (0..2).map(|_| &*arena.alloc(NodeValue::Text("".into()).into())).collect();
    }
    let mut lines: Vec<&AstNode<'_>> = vec![arena.alloc(node.data().clone().into())];
    for child in node.children() {
        for (ix, fragment) in split_lines(child, arena).into_iter().enumerate() {
            if ix > 0 {
                lines.push(arena.alloc(node.data().clone().into()));
            }
            lines.last().unwrap().append(fragment);
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 行内属性（6.3.10）上色：标记不进渲染，颜色类名交给主题换成 `content.*` 色值。
    #[test]
    fn inline_attributes_become_color_spans_and_hide_their_markers() {
        let html = |src: &str| match preview(src, 0..src.len()) {
            Some(Preview::Html(html)) => html,
            other => panic!("{src}: {}", other.is_some()),
        };
        // `[文本]{...}`：包裹的括号与属性列表都不显示
        assert_eq!(html("前 [文字]{.red} 后"), ["<p>前 <span class=\"red\">文字</span> 后</p>\n"]);
        // 文字色与背景色叠加，顺序固定为「文字 背景」
        assert_eq!(
            html("前 [文字]{.bg-blue .red} 后"),
            ["<p>前 <span class=\"red bg-blue\">文字</span> 后</p>\n"]
        );
        // 属性紧跟行内元素：强调、行内代码、链接、`==` 都能承载
        for (src, expected) in [
            ("**粗**{.blue}", "<span class=\"blue\"><strong>粗</strong></span>"),
            ("`码`{.red}", "<span class=\"red\"><code>码</code></span>"),
            ("[链接](aero://note/a){.red}", "<span class=\"red\"><a href=\"aero://note/a\">链接</a></span>"),
            ("==高亮=={.red}", "<span class=\"red\"><mark>高亮</mark></span>"),
            (
                "[外 [内]{.red} 文]{.bg-blue}",
                "<span class=\"bg-blue\">外 <span class=\"red\">内</span> 文</span>",
            ),
        ] {
            assert!(html(src)[0].contains(expected), "{src}: {:?}", html(src));
        }
        // 调色板外的类名不上色，但标记同样不显示（与块属性一致）
        assert_eq!(html("[文字]{.custom}"), ["<p>文字</p>\n"]);
        // 跨硬换行的 span 每行各自开合，HTML 始终配对
        assert_eq!(
            html("[a  \nb]{.red}"),
            ["<p><span class=\"red\">a</span></p>\n", "<p><span class=\"red\">b</span></p>\n"]
        );
        assert_eq!(html("# [标题]{.red}"), ["<h1><span class=\"red\">标题</span></h1>\n"]);
        // 表格单元格里的颜色照常，列宽用的文字不含哨兵
        let table = "| h |\n|---|\n| [格]{.red} |";
        let Some(Preview::Table { rows, .. }) = preview(table, 0..table.len()) else { panic!("{table}") };
        assert_eq!(rows[1][0].html, ["<p><span class=\"red\">格</span></p>\n"]);
        assert_eq!(rows[1][0].text, "格");
        // 代码、数学、转义里的 `{...}` 不是属性列表；普通花括号也不是
        for src in ["`[文字]{.red}`", "$[文字]{.red}$", "\\[文字]{.red}", "f(x) = {1, 2}", "[文字]{}"]
        {
            assert!(preview(src, 0..src.len()).is_none(), "{src}");
        }
        // 引用式链接的 `[文本]{...}` 既是 `[ ]` 又是链接元素，只能算一处（改写区间不许重叠）
        let reference = "[文字]{.red}\n\n[文字]: https://example.com\n";
        let block = parse::parse(reference).blocks[0].range.clone();
        let Some(Preview::Html(html)) = preview(reference, block) else { panic!("{reference}") };
        assert_eq!(html, ["<p><span class=\"red\"><a href=\"https://example.com\">文字</a></span></p>\n"]);
    }

    #[test]
    fn highlight_hard_breaks_keep_nested_styles_links_and_html_isolation() {
        let src = "==**[中  \n文](aero://note/a)**==";
        let Some(Preview::Html(html)) = preview(src, 0..src.len()) else { panic!("{src}") };
        assert_eq!(html.len(), 2);
        for (line, text) in html.iter().zip(["中", "文"]) {
            assert_eq!(
                line,
                &format!("<p><mark><strong><a href=\"aero://note/a\">{text}</a></strong></mark></p>\n")
            );
        }
        let src = "==a== &lt;br&gt; ` <br> ` <img src=x onerror=alert(1)>";
        let Some(Preview::Html(html)) = preview(src, 0..src.len()) else { panic!("{src}") };
        assert_eq!(html.len(), 1);
        assert!(html[0].contains("&lt;br&gt;"));
        assert!(html[0].contains("<code>&lt;br&gt;</code>"));
        assert!(!html[0].contains("onerror"));
    }

    #[test]
    fn highlight_ranges_obey_markdown_context_and_unicode_offsets() {
        let src = "==中文😀== `==code==` \\==escaped== $==math==$ [x](https://a/==url==)\n\n```md\n==fence==\n```\n";
        let ranges = ranges(src);
        assert_eq!(ranges.iter().map(|r| &src[r.clone()]).collect::<Vec<_>>(), ["中文😀"]);
        let table = "| h |\n|---|\n| a\\|==中文== |";
        assert_eq!(&table[self::ranges(table)[0].clone()], "中文");
    }

    #[test]
    fn highlight_preview_preserves_nested_styles_links_and_tables() {
        for (src, expected) in [
            (
                "前 ==**中文** [链接](aero://note/a)== 后",
                "<mark><strong>中文</strong> <a href=\"aero://note/a\">链接</a></mark>",
            ),
            ("# ==标题==", "<h1><mark>标题</mark></h1>"),
        ] {
            let Some(Preview::Html(html)) = preview(src, 0..src.len()) else { panic!("{src}") };
            assert_eq!(html.len(), 1);
            assert!(html[0].contains(expected), "{html:?}");
        }
        for src in ["==a  \nb==", "==a==<br>==b==", "==a==<BR/>==b=="] {
            let Some(Preview::Html(html)) = preview(src, 0..src.len()) else { panic!("{src}") };
            assert_eq!(html, ["<p><mark>a</mark></p>\n", "<p><mark>b</mark></p>\n"]);
        }
        let src = "> ==中文\n> 续行==";
        let Some(Preview::Html(html)) = preview(src, 2..src.len()) else { panic!("{src}") };
        assert!(html[0].contains("<mark>中文\n续行</mark>"), "{html:?}");
        let src = "| h | right |\n|---|---:|\n| a\\|==中文== | |";
        let Some(Preview::Table { alignments, rows }) = preview(src, 0..src.len()) else { panic!("{src}") };
        assert!(matches!(alignments[1], Alignment::Right));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].len(), 2);
        assert!(rows[1][0].html[0].contains("a|<mark>中文</mark>"));
        assert_eq!(rows[1][1].html, ["<p></p>\n"]);
        for src in ["`==code==`", "\\==escaped==", "$==math==$", "ordinary"] {
            assert!(preview(src, 0..src.len()).is_none(), "{src}");
        }
    }

    #[test]
    fn export_html_renders_structure_and_inline_color() {
        let html = export_html("# 标题\n\n正文\n");
        assert!(html.contains("<h1>标题</h1>"), "{html}");
        assert!(html.contains("<p>正文</p>"), "{html}");
        let colored = export_html("前 [文字]{.red} 后\n");
        assert!(colored.contains("<span class=\"red\">文字</span>"), "{colored}");
        assert!(!colored.contains("{.red}"), "{colored}");
    }

    #[test]
    fn inline_formulas_skip_display_math_and_code() {
        let found = inline_formulas("能量 $E=mc^2$ 守恒");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].latex, "E=mc^2");
        assert_eq!(&"能量 $E=mc^2$ 守恒"[found[0].range.clone()], "$E=mc^2$");
        assert!(inline_formulas("$$\nE=mc^2\n$$").is_empty());
        assert!(inline_formulas("` $E$ `").is_empty());
    }
}
