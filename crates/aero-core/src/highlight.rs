//! `==...==` 的显示适配。源码始终保留 Markdown，HTML 仅供原生富文本预览。

use std::ops::Range;

use comrak::{
    Arena,
    nodes::{AstNode, NodeValue},
    parse_document,
};

use crate::{inline_links::SourceMap, parse};

/// 高亮正文的源字节范围；使用与编辑 / 索引相同的解析器排除代码、转义、URL 等。
pub fn ranges(src: &str) -> Vec<Range<usize>> {
    if !src.contains("==") {
        return Vec::new();
    }
    let arena = Arena::new();
    let root = parse_document(&arena, src, &parse::comrak_options());
    let positions = SourceMap::new(src);
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

pub fn preview(src: &str, range: Range<usize>) -> Option<Preview> {
    if !src.get(range.clone())?.contains("==") {
        return None;
    }
    let arena = Arena::new();
    let mut options = parse::comrak_options();
    options.render.sourcepos = false;
    let root = parse_document(&arena, src, &options);
    let positions = SourceMap::new(src);
    let node = root.descendants().find(|node| {
        matches!(node.data().value, NodeValue::Paragraph | NodeValue::Heading(_) | NodeValue::Table(_))
            && positions.range(node) == Some(range.clone())
    })?;
    if !node.descendants().any(|n| matches!(n.data().value, NodeValue::Highlight)) {
        return None;
    }
    for child in node.descendants() {
        let mut data = child.data_mut();
        if let NodeValue::HtmlInline(raw) = &data.value
            && matches!(raw.to_ascii_lowercase().as_str(), "<br>" | "<br/>" | "<br />")
        {
            data.value = NodeValue::LineBreak;
        }
    }
    let html = |node| {
        split_lines(node, &arena)
            .into_iter()
            .map(|line| {
                if matches!(line.data().value, NodeValue::TableCell) {
                    line.data_mut().value = NodeValue::Paragraph;
                }
                let mut html = String::new();
                comrak::format_html(line, &options, &mut html).ok()?;
                Some(html)
            })
            .collect::<Option<Vec<_>>>()
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
                        let text = cell
                            .descendants()
                            .filter_map(|child| match &child.data().value {
                                NodeValue::Text(text) => Some(text.to_string()),
                                NodeValue::Code(code) => Some(code.literal.clone()),
                                NodeValue::LineBreak | NodeValue::SoftBreak => Some("\n".to_owned()),
                                _ => None,
                            })
                            .collect();
                        Some(Cell { html: content, text })
                    })
                    .collect::<Option<Vec<_>>>()
            })
            .collect::<Option<Vec<_>>>()?;
        return Some(Preview::Table { alignments, rows });
    }
    Some(Preview::Html(html(node)?))
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
}
