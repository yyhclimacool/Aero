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

/// Split visible inline text without leaving either paragraph with an unpaired
/// delimiter. Only wrappers crossing the caret are rebuilt; all other source
/// bytes, including link destinations and inline attributes, stay intact.
pub fn split_at(src: &str, cursor: usize) -> (String, String) {
    let arena = Arena::new();
    let root = parse_document(&arena, src, &parse::comrak_options());
    let positions = SourceMap::new(src);
    let mut wrappers = Vec::new();
    for node in root.descendants() {
        let Some(mut outer) = positions.range(node) else { continue };
        if let Some((_, end)) = crate::colors::attrs_at(src, outer.end) {
            outer.end = end;
        }
        let data = node.data();
        let code = matches!(data.value, NodeValue::Code(_));
        let body = if code {
            let raw = &src[outer.clone()];
            let width = raw.bytes().take_while(|&b| b == b'`').count();
            if width == 0 {
                continue;
            }
            let mut end = width;
            while end < raw.len() {
                if raw.as_bytes()[end] != b'`' {
                    end += 1;
                    continue;
                }
                let run = raw[end..].bytes().take_while(|&b| b == b'`').count();
                if run == width {
                    break;
                }
                end += run;
            }
            let mut body = outer.start + width..outer.start + end;
            let raw_body = &src[body.clone()];
            if raw_body.starts_with(' ') && raw_body.ends_with(' ') && !raw_body.chars().all(|c| c == ' ') {
                body.start += 1;
                body.end -= 1;
            }
            body
        } else if matches!(
            data.value,
            NodeValue::Strong
                | NodeValue::Emph
                | NodeValue::Strikethrough
                | NodeValue::Highlight
                | NodeValue::Insert
                | NodeValue::Superscript
                | NodeValue::Link(_)
        ) {
            let Some(body) = node
                .first_child()
                .and_then(|n| positions.range(n))
                .zip(node.last_child().and_then(|n| positions.range(n)))
                .map(|(first, last)| first.start..last.end)
            else {
                continue;
            };
            // Autolinks have no separable label/destination syntax.
            if matches!(data.value, NodeValue::Link(_)) && !src[outer.clone()].starts_with('[') {
                continue;
            }
            body
        } else {
            continue;
        };
        if outer.start <= body.start && body.end <= outer.end {
            wrappers.push((outer, body, code));
        }
    }
    fn divide(
        src: &str,
        range: Range<usize>,
        cursor: usize,
        wrappers: &[(Range<usize>, Range<usize>, bool)],
    ) -> (String, String) {
        if let Some((outer, body, code)) = wrappers.iter().find(|(outer, body, _)| {
            range.start <= outer.start
                && outer.end <= range.end
                && outer.start < cursor
                && cursor < outer.end
                && body.start <= cursor
                && cursor <= body.end
                && *body != range
        }) {
            let (left, right) = divide(src, body.clone(), cursor, wrappers);
            let wrap = |part: &str| {
                if part.is_empty() {
                    return String::new();
                }
                if *code {
                    let raw = &src[outer.clone()];
                    let width = raw.bytes().take_while(|&b| b == b'`').count();
                    let marker = &raw[..width];
                    // Strip the old padding, then choose padding for each half.
                    let suffix = &src[body.end..outer.end];
                    let suffix = suffix.strip_prefix(' ').unwrap_or(suffix);
                    let pad = part.starts_with('`')
                        || part.ends_with('`')
                        || (part.starts_with(' ') && part.ends_with(' ') && !part.trim().is_empty());
                    let padding = if pad { " " } else { "" };
                    format!("{marker}{padding}{part}{padding}{suffix}")
                } else {
                    format!("{}{part}{}", &src[outer.start..body.start], &src[body.end..outer.end])
                }
            };
            return (
                format!("{}{}", &src[range.start..outer.start], wrap(&left)),
                format!("{}{}", wrap(&right), &src[outer.end..range.end]),
            );
        }
        (
            src[range.start..cursor].trim_end_matches([' ', '\t']).to_owned(),
            src[cursor..range.end].trim_start_matches([' ', '\t']).to_owned(),
        )
    }
    let mut cursor = cursor.min(src.len());
    while !src.is_char_boundary(cursor) {
        cursor -= 1;
    }
    divide(src, 0..src.len(), cursor, &wrappers)
}

/// 选区已经带着的行内格式，按 [`Format`] 的固定顺序返回（选区浮动工具条的按下态，UI 设计 5.9）。
///
/// 判定是"选区落在这种格式的元素里"，恰好覆盖元素或只选中其中一段都算。
pub fn active(src: &str, selection: Range<usize>) -> Vec<Format> {
    let arena = Arena::new();
    let root = parse_document(&arena, src, &parse::comrak_options());
    let positions = SourceMap::new(src);
    [Format::Bold, Format::Italic, Format::Code, Format::Strike, Format::Highlight]
        .into_iter()
        .filter(|format| {
            root.descendants().any(|node| {
                format.matches(&node.data().value)
                    && positions
                        .range(node)
                        .is_some_and(|r| r.start <= selection.start && selection.end <= r.end)
            })
        })
        .collect()
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

    #[test]
    fn split_balances_nested_formatting_links_and_inline_attributes() {
        for (source, needle, expected) in [
            ("前 **中文😀** 后", "😀", ("前 **中文**", "**😀** 后")),
            ("***abcd***", "cd", ("***ab***", "***cd***")),
            ("__ab cd__", " cd", ("__ab__", "__cd__")),
            ("~~ab cd~~", " cd", ("~~ab~~", "~~cd~~")),
            ("==ab cd==", " cd", ("==ab==", "==cd==")),
            ("**ab *cd ef* gh**", " ef", ("**ab *cd***", "***ef* gh**")),
            (
                "[**ab cd**](https://example.test \"标题\")",
                " cd",
                ("[**ab**](https://example.test \"标题\")", "[**cd**](https://example.test \"标题\")"),
            ),
            ("前 **ab cd**{.red} 后", " cd", ("前 **ab**{.red}", "**cd**{.red} 后")),
            ("`ab cd`", " cd", ("`ab`", "`cd`")),
            ("`` a`b ``{.red}", "`b", ("``a``{.red}", "`` `b ``{.red}")),
            ("前 **ab** 后", "ab", ("前 ", "**ab** 后")),
            ("前 **ab** 后", "** 后", ("前 **ab**", " 后")),
        ] {
            let at = source.find(needle).unwrap();
            assert_eq!(split_at(source, at), (expected.0.into(), expected.1.into()), "{source}");
        }
        // Escaped and unmatched delimiters remain ordinary source text.
        assert_eq!(split_at(r"a \**bc", 5), (r"a \**".into(), "bc".into()));
    }

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

    /// 工具条按下态：选区带着哪些格式。
    #[test]
    fn active_formats_report_what_the_selection_already_carries() {
        let src = "前 **粗 `码` ==亮==** 与 *斜* 与 ~~删~~ 后\n";
        let find = |needle: &str| {
            let at = src.find(needle).expect(needle);
            at..at + needle.len()
        };
        // 嵌在粗体里的行内代码：两种都算
        assert_eq!(active(src, find("码")), [Format::Bold, Format::Code]);
        assert_eq!(active(src, find("亮")), [Format::Bold, Format::Highlight]);
        // 恰好覆盖元素本身（含标记）也算
        assert_eq!(active(src, find("*斜*")), [Format::Italic]);
        assert_eq!(active(src, find("删")), [Format::Strike]);
        // 普通文字什么都没带
        assert!(active(src, find("与")).is_empty());
        assert!(active("干净的一行\n", 0..6).is_empty());
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
