//! 行内格式的可撤销源码编辑计划。AST 校验包裹范围，避免改坏块、链接与属性。

use std::ops::Range;

use comrak::{Arena, nodes::NodeValue, parse_document};

use crate::inline_links::{SourceMap, same_structure};
use crate::{Edit, links, parse};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Bold,
    Italic,
    Code,
    Strike,
    Highlight,
}

impl Format {
    fn delimiter(self) -> &'static str {
        match self {
            Self::Bold => "**",
            Self::Italic => "*",
            Self::Code => "`",
            Self::Strike => "~~",
            Self::Highlight => "==",
        }
    }

    fn matches(self, value: &NodeValue) -> bool {
        matches!(
            (self, value),
            (Self::Bold, NodeValue::Strong)
                | (Self::Italic, NodeValue::Emph)
                | (Self::Code, NodeValue::Code(_))
                | (Self::Strike, NodeValue::Strikethrough)
                | (Self::Highlight, NodeValue::Highlight)
        )
    }
}

pub struct Plan {
    pub edit: Edit,
    /// 编辑后的文档字节选区；空选区位于新标记中间。
    pub selection: Range<usize>,
}

pub fn plan(src: &str, mut selection: Range<usize>, format: Format) -> Option<Plan> {
    let selected = src.get(selection.clone())?;
    if selected.contains(['\n', '\r']) {
        return None;
    }
    if !selection.is_empty() {
        selection.start += selected.len() - selected.trim_start().len();
        selection.end -= selected.len() - selected.trim_end().len();
        if selection.start >= selection.end {
            return None;
        }
    }
    let marker = format.delimiter();
    if selection.is_empty()
        && let Some(start) = selection.start.checked_sub(marker.len())
        && let Some(end) = selection.end.checked_add(marker.len())
        && src.get(start..selection.start) == Some(marker)
        && src.get(selection.end..end) == Some(marker)
        && !src[..start].ends_with(marker)
        && !src[end..].starts_with(marker)
    {
        let mut removed = src.to_owned();
        removed.replace_range(start..end, "");
        if plan(&removed, start..start, format).is_some() {
            return Some(Plan { edit: Edit::delete(start..end), selection: start..start });
        }
    }
    let arena = Arena::new();
    let root = parse_document(&arena, src, &parse::comrak_options());
    let positions = SourceMap::new(src);
    let structure = parse::parse(src).blocks;

    // 优先取消恰好覆盖整个元素 / 元素正文的格式，保留内部格式和原始字节。
    for node in root.descendants().filter(|n| format.matches(&n.data().value)) {
        let Some(outer) = positions.range(node) else { continue };
        let raw = src.get(outer.clone())?;
        let width = if format == Format::Code {
            raw.bytes().take_while(|&b| b == b'`').count()
        } else if format == Format::Bold {
            2
        } else if format == Format::Strike && !raw.starts_with("~~") {
            1
        } else if matches!(format, Format::Strike | Format::Highlight) {
            format.delimiter().len()
        } else {
            1
        };
        if width == 0 || raw.len() < width * 2 {
            continue;
        }
        let mut inner = outer.start + width..outer.end - width;
        if format == Format::Code {
            let raw = &src[inner.clone()];
            if raw.starts_with(' ') && raw.ends_with(' ') && !raw.chars().all(|c| c == ' ') {
                inner.start += 1;
                inner.end -= 1;
            }
        }
        if !selection.is_empty() && (selection == inner || selection == outer) {
            if node.data().attrs.is_some() {
                return None; // 去掉承载属性的元素会让属性失去归属，留给颜色编辑计划处理。
            }
            let replacement = src[inner].to_owned();
            let mut after = src.to_owned();
            after.replace_range(outer.clone(), &replacement);
            if !same_structure(&structure, &parse::parse(&after).blocks) {
                return None;
            }
            let selection = outer.start..outer.start + replacement.len();
            return Some(Plan { edit: Edit::replace(outer, replacement), selection });
        }
    }

    let overlaps = |r: &Range<usize>| {
        if selection.is_empty() {
            r.start < selection.start && selection.start < r.end
        } else {
            r.start < selection.end && selection.start < r.end
        }
    };
    if links::scan(src).iter().any(|s| overlaps(&s.range))
        || links::scan_embeds(src).iter().any(|s| overlaps(&s.range))
    {
        return None;
    }
    for node in root.descendants() {
        let data = node.data();
        let Some(range) = positions.range(node) else { continue };
        if selection.is_empty()
            && range.contains(&selection.start)
            && matches!(
                data.value,
                NodeValue::CodeBlock(_) | NodeValue::HtmlBlock(_) | NodeValue::FrontMatter(_)
            )
        {
            return None;
        }
        if !overlaps(&range) {
            continue;
        }
        match data.value {
            NodeValue::CodeBlock(_)
            | NodeValue::Math(_)
            | NodeValue::HtmlInline(_)
            | NodeValue::HtmlBlock(_)
            | NodeValue::FrontMatter(_)
            | NodeValue::Image(_) => return None,
            NodeValue::Code(_)
                if data.attrs.is_some() || selection.start > range.start || selection.end < range.end =>
            {
                return None;
            }
            NodeValue::Link(_) => {
                let label = node
                    .first_child()
                    .and_then(|n| positions.range(n))
                    .zip(node.last_child().and_then(|n| positions.range(n)));
                let inside_label = label
                    .is_some_and(|(first, last)| first.start <= selection.start && selection.end <= last.end);
                let whole = selection.start <= range.start && selection.end >= range.end;
                if !inside_label && !whole {
                    return None;
                }
            }
            _ => {}
        }
    }

    // 对空光标也用正文探针检查结构位置，允许空段落和列表内容末端。
    let placeholder = "aero-format-probe";
    let chosen = if selection.is_empty() { placeholder } else { &src[selection.clone()] };
    let delimiter = if format == Format::Code {
        let longest = chosen.split(|c| c != '`').map(str::len).max().unwrap_or(0);
        "`".repeat(longest + 1)
    } else {
        format.delimiter().to_owned()
    };
    let pad = format == Format::Code
        && (chosen.starts_with('`')
            || chosen.ends_with('`')
            || (chosen.starts_with(' ') && chosen.ends_with(' ')));
    let padding = if pad { " " } else { "" };
    let wrap = |body: &str| format!("{delimiter}{padding}{body}{padding}{delimiter}");
    let mut before = src.to_owned();
    let mut after = src.to_owned();
    before.replace_range(selection.clone(), chosen);
    if selection.is_empty() {
        // 空段落 / 空列表项可增加正文叶子，但不能因光标位于 #、>、围栏之前而改变容器。
        let skeleton = |blocks: &[parse::ParsedBlock]| {
            parse::ParsedBlock::iter(blocks)
                .filter(|(_, b)| b.kind != crate::BlockKind::Paragraph)
                .map(|(depth, b)| (depth, b.kind.clone(), b.attrs.clone(), b.persisted_id.clone()))
                .collect::<Vec<_>>()
        };
        if skeleton(&structure) != skeleton(&parse::parse(&before).blocks) {
            return None;
        }
    }
    let wrapped = wrap(chosen);
    after.replace_range(selection.clone(), &wrapped);
    let arena = Arena::new();
    let parsed = parse_document(&arena, &after, &parse::comrak_options());
    let positions = SourceMap::new(&after);
    let expected = selection.start..selection.start + wrapped.len();
    if !parsed
        .descendants()
        .any(|n| format.matches(&n.data().value) && positions.range(n) == Some(expected.clone()))
        || !same_structure(&parse::parse(&before).blocks, &parse::parse(&after).blocks)
    {
        return None;
    }
    let body = if selection.is_empty() { "" } else { chosen };
    let start = selection.start + delimiter.len() + padding.len();
    Some(Plan { edit: Edit::replace(selection, wrap(body)), selection: start..start + body.len() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(src: &str, chosen: &str, format: Format) -> (String, Range<usize>) {
        let at = src.find(chosen).unwrap();
        let plan = plan(src, at..at + chosen.len(), format).expect(src);
        let mut result = src.to_owned();
        result.replace_range(plan.edit.range, &plan.edit.text);
        (result, plan.selection)
    }

    #[test]
    fn inline_format_wraps_and_toggles_unicode_and_nested_markup() {
        for (format, marker) in [
            (Format::Bold, "**"),
            (Format::Italic, "*"),
            (Format::Code, "`"),
            (Format::Strike, "~~"),
            (Format::Highlight, "=="),
        ] {
            let src = "前 中文😀 后\n";
            let (after, selection) = apply(src, "中文😀", format);
            assert_eq!(after, format!("前 {marker}中文😀{marker} 后\n"));
            assert_eq!(&after[selection], "中文😀");
            for chosen in ["中文😀".to_owned(), format!("{marker}中文😀{marker}")] {
                assert_eq!(apply(&after, &chosen, format).0, src);
            }
        }
        assert_eq!(apply("前 **粗 *斜* 体** 后", "粗 *斜* 体", Format::Bold).0, "前 粗 *斜* 体 后");
        assert_eq!(apply("前 __粗体__ 后", "粗体", Format::Bold).0, "前 粗体 后");
    }

    #[test]
    fn inline_format_preserves_whitespace_and_uses_long_code_delimiters() {
        assert_eq!(apply("前  中文  后", " 中文 ", Format::Bold).0, "前  **中文**  后");
        let src = "前 a`b``c 后";
        let (wrapped, _) = apply(src, "a`b``c", Format::Code);
        assert_eq!(wrapped, "前 ```a`b``c``` 后");
        assert_eq!(apply(&wrapped, "a`b``c", Format::Code).0, src);
        let (wrapped, _) = apply("前 `code` 后", "`code`", Format::Bold);
        assert_eq!(wrapped, "前 **`code`** 后");
    }

    #[test]
    fn inline_format_links_tables_callouts_and_attributes_keep_structure() {
        for src in [
            "# 标题 中文 {.red}\n",
            "- 中文\n- 第二行\n",
            "> [!note]- 标题\n> 中文 {.red} ^id\n",
            "| 表头 |\n|---|\n| a\\|中文 |\n",
            "[中文](https://example.com \"title\")\n",
        ] {
            let (after, _) = apply(src, "中文", Format::Bold);
            assert_eq!(after, src.replace("中文", "**中文**"));
            assert_eq!(apply(&after, "中文", Format::Bold).0, src);
        }
    }

    #[test]
    fn inline_format_refuses_code_urls_metadata_and_cross_block_edits() {
        for (src, chosen) in [
            ("```md\ninside\n```", "inside"),
            ("`inside`", "inside"),
            ("$inside$", "inside"),
            ("---\ntitle: inside\n---\n", "inside"),
            ("[label](https://inside)", "inside"),
            ("![inside](img.png)", "inside"),
            ("[[inside]]", "inside"),
            ("![[inside]]", "inside"),
            ("# heading", "# heading"),
            ("one\n\ntwo", "one\n\ntwo"),
            ("text {.red}", ".red"),
        ] {
            let at = src.find(chosen).unwrap();
            assert!(plan(src, at..at + chosen.len(), Format::Bold).is_none(), "{src}");
        }
        assert!(plan("**# title**", 2..9, Format::Bold).is_none());
        for format in [Format::Bold, Format::Code] {
            for selection in [0..6, 1..5] {
                assert!(plan("`code`{.red}", selection, format).is_none());
            }
        }
    }

    #[test]
    fn inline_format_empty_caret_has_markers_and_interior_position() {
        for src in ["", "正文 ", "# 标题 ", "- ", "> "] {
            for format in [Format::Bold, Format::Italic, Format::Code, Format::Strike, Format::Highlight] {
                let at = src.len();
                let plan = plan(src, at..at, format).expect(src);
                let marker = format.delimiter();
                assert_eq!(plan.edit.text, marker.repeat(2));
                assert_eq!(plan.selection, at + marker.len()..at + marker.len());
            }
        }
        for (src, at) in [("# heading", 0), ("```md\ncode\n```", 0), ("---\ntitle: x\n---", 0)] {
            assert!(plan(src, at..at, Format::Bold).is_none(), "{src}");
        }
        assert_eq!(apply("前 ~删除~ 后", "删除", Format::Strike).0, "前 删除 后");
    }
}
