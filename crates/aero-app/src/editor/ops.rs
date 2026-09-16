//! 结构性编辑的"计划"：拆块、合并、跨块移动。纯函数，只看 `Document` 与编辑单位，
//! 不碰 GPUI，因而可以在没有窗口的情况下测试（`editor/mod.rs` 只负责把计划应用到文档并移动焦点）。

use std::ops::Range;

use aero_core::{BlockKind, Document, Edit};

use super::text_ops::{ListMarker, heading_prefix_len, line_start};
use super::units::Unit;

/// 一次编辑，以及编辑后光标应落到的文档偏移。
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub edit: Edit,
    pub focus_offset: usize,
}

/// ⏎（非软换行）：
/// - 列表项：空项退出列表（嵌套则降一级）；项末在整项之后新建一项；项中拆成两项
/// - 其余：插入空行，光标后的部分成为新段落
pub fn split(doc: &Document, unit: Option<&Unit>, range: &Range<usize>, cursor: usize) -> Plan {
    let text = doc.slice(range.clone()).to_string();
    let cursor = cursor.min(text.len());
    let doc_cursor = range.start + cursor;
    // 光标两侧的空格一并吃掉：Markdown 会忽略段首空白，留着会让新块的起点与焦点错位
    let ws_before = text[..cursor].len() - text[..cursor].trim_end_matches([' ', '\t']).len();
    let ws_after = text[cursor..].len() - text[cursor..].trim_start_matches([' ', '\t']).len();
    let cut = doc_cursor - ws_before..doc_cursor + ws_after;

    if let Some(ctx) = unit.and_then(|u| u.list.as_ref()) {
        let doc_text = doc.to_string();
        if text.trim().is_empty() {
            let ls = line_start(&doc_text, ctx.item_range.start);
            return match ctx.parent.as_deref() {
                Some(parent) => {
                    let parent_src = &doc_text[parent.item_range.clone()];
                    let next = parent.ordered.map(|s| s + parent.item_index as u32 + 1);
                    let marker = ListMarker::from_item_source(parent_src, next, false).text();
                    Plan { focus_offset: ls + marker.len(), edit: Edit::replace(ls..range.end, marker) }
                }
                None => {
                    let replacement = if ctx.item_index > 0 { "\n" } else { "" };
                    Plan {
                        focus_offset: ls + replacement.len(),
                        edit: Edit::replace(ls..range.end, replacement),
                    }
                }
            };
        }
        let item_src = &doc_text[ctx.item_range.clone()];
        let next = ctx.ordered.map(|s| s + ctx.item_index as u32 + 1);
        let marker = ListMarker::from_item_source(item_src, next, ctx.task).text();
        let insert = format!("\n{marker}");
        return if cursor >= text.len() {
            let at = ctx.item_range.end;
            Plan { focus_offset: at + insert.len(), edit: Edit::insert(at, insert) }
        } else {
            Plan { focus_offset: cut.start + insert.len(), edit: Edit::replace(cut, insert) }
        };
    }

    Plan { focus_offset: cut.start + 2, edit: Edit::replace(cut, "\n\n") }
}

#[derive(Debug, PartialEq)]
pub enum Backspace {
    Edit(Plan),
    /// 不改文档，只把焦点移到某个单位的某个偏移
    MoveTo {
        unit: usize,
        cursor: usize,
    },
    Nothing,
}

/// ⌫ 在单位开头：列表项脱离列表；段落 / 标题并入上一个文本块（保留其类型，标题去掉 `#`）；
/// 上一块不是文本时只移动光标。
pub fn backspace_at_start(doc: &Document, units: &[Unit], ix: usize) -> Backspace {
    let unit = &units[ix];
    let doc_text = doc.to_string();

    if let Some(ctx) = &unit.list {
        let between = &doc_text[ctx.item_range.start..unit.range.start];
        if is_list_marker(between) {
            let ls = line_start(&doc_text, ctx.item_range.start);
            let replacement = if ctx.item_index > 0 || ctx.parent.is_some() { "\n" } else { "" };
            return Backspace::Edit(Plan {
                focus_offset: ls + replacement.len(),
                edit: Edit::replace(ls..unit.range.start, replacement),
            });
        }
    }

    if ix == 0 {
        return Backspace::Nothing;
    }
    let prev = &units[ix - 1];
    let mergeable = unit.is_text() && prev.is_text() && prev.list.is_none() && unit.list.is_none();
    if mergeable {
        let strip = if matches!(unit.kind, BlockKind::Heading { .. }) {
            heading_prefix_len(&doc_text[unit.range.clone()])
        } else {
            0
        };
        Backspace::Edit(Plan {
            focus_offset: prev.range.end,
            edit: Edit::delete(prev.range.end..unit.range.start + strip),
        })
    } else {
        Backspace::MoveTo { unit: ix - 1, cursor: prev.range.len() }
    }
}

/// ⌫ 在一个没有块的空位（刚用 ⏎ 建出的空段落）：删掉它与上一单位之间的空行，光标回到上一单位末尾。
/// 中间夹着非空白（如 `^id` 标记）时只移动光标；前面没有单位时什么都不做。
pub fn backspace_at_empty_slot(doc: &Document, units: &[Unit], offset: usize) -> Backspace {
    let Some(ix) = units.iter().rposition(|u| u.range.start < offset) else { return Backspace::Nothing };
    let prev = &units[ix];
    let end = prev.range.end.min(offset);
    let gap = doc.slice(end..offset).to_string();
    if gap.chars().all(char::is_whitespace) {
        Backspace::Edit(Plan { focus_offset: end, edit: Edit::delete(end..offset) })
    } else {
        Backspace::MoveTo { unit: ix, cursor: prev.range.len() }
    }
}

/// ↑ 在首行：上一个单位的最后一行，尽量保持列。
pub fn up_target(doc: &Document, units: &[Unit], start: usize, col: usize) -> Option<(usize, usize)> {
    let ix = units.iter().rposition(|u| u.range.start < start)?;
    let target = doc.slice(units[ix].range.clone()).to_string();
    let ls = line_start(&target, target.len());
    Some((ix, ls + col.min(target.len() - ls)))
}

/// ↓ 在末行：下一个单位的第一行，尽量保持列。
pub fn down_target(
    doc: &Document,
    units: &[Unit],
    range: &Range<usize>,
    col: usize,
) -> Option<(usize, usize)> {
    let ix = units.iter().position(|u| u.range.start > range.start && u.range.start >= range.end)?;
    let target = doc.slice(units[ix].range.clone()).to_string();
    let first_len = target.find('\n').unwrap_or(target.len());
    Some((ix, col.min(first_len)))
}

/// 连续几个单位（下标闭区间 `first..=last`）在源码里占的整行区间：首个单位所在行的行首到最后一个单位
/// 末行的行尾（不含那个换行）。列表项按整项算（含嵌套子项）；`^id` 等尾标记与正文同行，自然包含。
/// 这就是块选中态里 ⌘C 复制、⌫ 删除的对象（`ui_design.md` 6）。
pub fn units_span(doc: &Document, units: &[Unit], first: usize, last: usize) -> Range<usize> {
    let text = doc.to_string();
    let mut start = usize::MAX;
    let mut end = 0;
    for unit in &units[first..=last] {
        let (s, e) = match &unit.list {
            Some(ctx) => (ctx.item_range.start, ctx.item_range.end),
            None => (unit.range.start, unit.range.end),
        };
        // 区间末尾若已含换行，退回到行内再找行尾，免得跑到下一行
        let e = if e > s && text[..e].ends_with('\n') { e - 1 } else { e };
        start = start.min(s);
        end = end.max(e);
    }
    let start = line_start(&text, start.min(text.len()));
    let end = text[end.min(text.len())..].find('\n').map_or(text.len(), |i| end + i);
    start..end
}

/// 删除连续几个单位：整行删掉，连同它们之后的空行；删到文末时也收掉前面多出来的空行，
/// 文档结尾仍是单个换行。
pub fn delete_units(doc: &Document, units: &[Unit], first: usize, last: usize) -> Edit {
    let text = doc.to_string();
    let Range { mut start, mut end } = units_span(doc, units, first, last);
    while end < text.len() && text.as_bytes()[end] == b'\n' {
        end += 1;
    }
    if end == text.len() {
        while start >= 2 && &text[start - 2..start] == "\n\n" {
            start -= 1;
        }
    }
    Edit::delete(start..end)
}

/// 块 gutter 的 `+`（`ui_design.md` 5.3）：在单位 `ix` 下方新开一个段落，内容是一个 `/`，
/// 焦点放在 `/` 之后——编辑器随后按 `/` 菜单的常规逻辑把菜单打开；Esc 关菜单时 `/` 留在文档里，
/// 与手敲 `/` 一致。列表项按整项算，新段落落在项之后（把列表断开；菜单里选列表类型即可续上）。
pub fn insert_slash_below(doc: &Document, units: &[Unit], ix: usize) -> Plan {
    let text = doc.to_string();
    let at = units_span(doc, units, ix, ix).end;
    let rest = &text[at..];
    // 后面已经是空行或文末：两个换行把新段落隔开就够；紧跟着别的内容时再补一个，免得 `/` 并进它的段落
    let insert = if rest.is_empty() || rest.starts_with("\n\n") { "\n\n/" } else { "\n\n/\n" };
    Plan { focus_offset: at + "\n\n/".len(), edit: Edit::insert(at, insert) }
}

/// 在文末新建空段落：需要补的换行与新段落的偏移；不需要编辑时 `edit` 为 `None`。
pub fn append_paragraph(doc: &Document) -> (Option<Edit>, usize) {
    let text = doc.to_string();
    let len = text.len();
    let insert = if text.is_empty() || text.ends_with("\n\n") {
        ""
    } else if text.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    if insert.is_empty() { (None, len) } else { (Some(Edit::insert(len, insert)), len + insert.len()) }
}

/// `/` 菜单与 `⌘⌥0–8` 能切换到的块类型（`ui_design.md` 7.1 的最小集合）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockType {
    Text,
    Heading(u8),
    Bullet,
    Numbered,
    Task,
    Quote,
    Code,
    Divider,
}

impl BlockType {
    /// `⌘⌥0–8` 的顺序：0 文本、1–3 标题、4 无序、5 有序、6 任务、7 引用、8 代码块。
    pub fn from_shortcut(n: u8) -> Option<Self> {
        Some(match n {
            0 => Self::Text,
            1..=3 => Self::Heading(n),
            4 => Self::Bullet,
            5 => Self::Numbered,
            6 => Self::Task,
            7 => Self::Quote,
            8 => Self::Code,
            _ => return None,
        })
    }
}

/// 单位当前的块类型；不属于上面任何一种（表格、数学块…）时为 `None`。
pub fn block_type(unit: &Unit) -> Option<BlockType> {
    Some(match (&unit.kind, unit.list.as_ref()) {
        // 空列表项自身是单位（kind 是 ListItem）
        (BlockKind::Paragraph | BlockKind::ListItem { .. }, Some(ctx)) if ctx.task => BlockType::Task,
        (BlockKind::Paragraph | BlockKind::ListItem { .. }, Some(ctx)) if ctx.ordered.is_some() => {
            BlockType::Numbered
        }
        (BlockKind::Paragraph | BlockKind::ListItem { .. }, Some(_)) => BlockType::Bullet,
        (BlockKind::Paragraph, None) => BlockType::Text,
        (BlockKind::Heading { level }, _) => BlockType::Heading(*level),
        (BlockKind::BlockQuote, _) => BlockType::Quote,
        (BlockKind::CodeFence { .. } | BlockKind::CodeBlock, _) => BlockType::Code,
        (BlockKind::ThematicBreak, _) => BlockType::Divider,
        _ => return None,
    })
}

/// 把单位 `ix` 转成 `target`（7.1："在非空块上使用 `/` 菜单选择块类型 = 转换当前块"）。
/// 只改源码前缀：标题去 `#`、列表项去标记、引用去 `> `、代码块去围栏，再套上目标前缀。
/// 分割线特殊：空块直接变成 `---`；非空块在其后插入 `---` 与一个新段落，焦点落到新段落。
/// 已经是目标类型、或单位不是文本类时返回 `None`。
pub fn convert(doc: &Document, units: &[Unit], ix: usize, target: BlockType) -> Option<Plan> {
    let unit = &units[ix];
    let current = block_type(unit)?;
    if current == target {
        return None;
    }
    let doc_text = doc.to_string();
    // 替换区间的起点：列表项是标记所在行的行首（吃掉缩进与标记），其余是单位起点
    let start = match &unit.list {
        Some(ctx) => line_start(&doc_text, ctx.item_range.start),
        None => unit.range.start,
    };
    let src = &doc_text[unit.range.clone()];
    let content: String = match current {
        BlockType::Heading(_) => src[heading_prefix_len(src)..].to_owned(),
        BlockType::Quote => return None, // 引用整体是源码单位，转换留给以后
        BlockType::Code if matches!(unit.kind, BlockKind::CodeBlock) => {
            src.lines().map(|l| l.strip_prefix("    ").unwrap_or(l)).collect::<Vec<_>>().join("\n")
        }
        BlockType::Code => strip_fence(src).to_owned(),
        BlockType::Divider => String::new(),
        _ => src.to_owned(),
    };
    let content = content.trim_end_matches('\n').to_owned();

    if target == BlockType::Divider {
        return Some(if content.trim().is_empty() {
            Plan { focus_offset: start + 3, edit: Edit::replace(start..unit.range.end, "---") }
        } else {
            let at = unit.range.end;
            Plan { focus_offset: at + "\n\n---\n\n".len(), edit: Edit::insert(at, "\n\n---\n\n") }
        });
    }

    let (text, cursor) = match target {
        BlockType::Text => (content.clone(), content.len()),
        BlockType::Heading(n) => {
            let t = format!("{} {content}", "#".repeat(n as usize));
            (t.clone(), t.len())
        }
        BlockType::Bullet => (format!("- {content}"), 2 + content.len()),
        BlockType::Numbered => (format!("1. {content}"), 3 + content.len()),
        BlockType::Task => (format!("- [ ] {content}"), 6 + content.len()),
        BlockType::Quote => {
            let t: String = if content.is_empty() {
                "> ".to_owned()
            } else {
                content.lines().map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n")
            };
            (t.clone(), t.len())
        }
        BlockType::Code => (format!("```\n{content}\n```"), 4 + content.len()),
        BlockType::Divider => unreachable!(),
    };
    Some(Plan { focus_offset: start + cursor, edit: Edit::replace(start..unit.range.end, text) })
}

/// 在没有块的空位 `at` 直接写出目标类型的骨架（`/` 菜单用在刚 ⏎ 出来的空段落上）。
pub fn insert_block(at: usize, target: BlockType) -> Plan {
    let (text, cursor) = match target {
        BlockType::Text => (String::new(), 0),
        BlockType::Heading(n) => {
            let t = format!("{} ", "#".repeat(n as usize));
            let l = t.len();
            (t, l)
        }
        BlockType::Bullet => ("- ".to_owned(), 2),
        BlockType::Numbered => ("1. ".to_owned(), 3),
        BlockType::Task => ("- [ ] ".to_owned(), 6),
        BlockType::Quote => ("> ".to_owned(), 2),
        BlockType::Code => ("```\n\n```".to_owned(), 4),
        BlockType::Divider => ("---\n\n".to_owned(), 5),
    };
    Plan { focus_offset: at + cursor, edit: Edit::insert(at, text) }
}

/// 去掉围栏代码块的首尾围栏行，留下正文（不含末尾换行）。缩进代码块则去掉每行 4 空格。
fn strip_fence(src: &str) -> &str {
    let first_nl = match src.find('\n') {
        Some(i) => i,
        None => return "",
    };
    let body = &src[first_nl + 1..];
    match body.trim_end_matches('\n').rfind('\n') {
        Some(last_nl)
            if body[last_nl + 1..].trim_start().starts_with("```")
                || body[last_nl + 1..].trim_start().starts_with("~~~") =>
        {
            &body[..last_nl]
        }
        _ if body.trim().starts_with("```") || body.trim().starts_with("~~~") => "",
        _ => body,
    }
}

/// `- `、`3) `、`- [x] `（可带缩进）是否只是列表标记。
pub fn is_list_marker(s: &str) -> bool {
    let s = s.trim_start_matches([' ', '\t']);
    let rest = if let Some(r) = s.strip_prefix(['-', '*', '+']) {
        r
    } else {
        let digits = s.bytes().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0 {
            return false;
        }
        match s[digits..].strip_prefix(['.', ')']) {
            Some(r) => r,
            None => return false,
        }
    };
    let rest = rest.trim_start_matches(' ');
    rest.is_empty() || matches!(rest, "[ ]" | "[x]" | "[X]" | "[ ] " | "[x] " | "[X] ")
}

#[cfg(test)]
mod tests {
    use super::super::units::{unit_at, units};
    use super::*;

    /// 应用计划并重解析，返回新文本与焦点处的单位文本（无块时为 None）。
    fn apply(doc: &mut Document, plan: Plan) -> (String, Option<String>) {
        doc.edit(plan.edit).unwrap();
        doc.reparse();
        let us = units(doc);
        let focused = unit_at(&us, plan.focus_offset)
            .filter(|&ix| us[ix].range.start <= plan.focus_offset)
            .map(|ix| doc.slice(us[ix].range.clone()).to_string());
        (doc.to_string(), focused)
    }

    #[test]
    fn list_marker_detection() {
        assert!(is_list_marker("- "));
        assert!(is_list_marker("  * "));
        assert!(is_list_marker("12) "));
        assert!(is_list_marker("- [ ] "));
        assert!(!is_list_marker("- a "));
        assert!(!is_list_marker(""));
    }

    #[test]
    fn convert_between_block_types() {
        // 段落 → 二级标题 → 无序列表 → 任务 → 有序 → 文本
        let mut doc = Document::from_text("intro\n\nhello\n\nend\n");
        let us = units(&doc);
        assert_eq!(block_type(&us[1]), Some(BlockType::Text));
        let plan = convert(&doc, &us, 1, BlockType::Heading(2)).unwrap();
        let (text, focused) = apply(&mut doc, plan);
        assert_eq!(text, "intro\n\n## hello\n\nend\n");
        assert_eq!(focused.as_deref(), Some("## hello"));

        let us = units(&doc);
        assert_eq!(block_type(&us[1]), Some(BlockType::Heading(2)));
        let plan = convert(&doc, &us, 1, BlockType::Bullet).unwrap();
        let (text, _) = apply(&mut doc, plan);
        assert_eq!(text, "intro\n\n- hello\n\nend\n");

        let us = units(&doc);
        assert_eq!(block_type(&us[1]), Some(BlockType::Bullet));
        let plan = convert(&doc, &us, 1, BlockType::Task).unwrap();
        let (text, _) = apply(&mut doc, plan);
        assert_eq!(text, "intro\n\n- [ ] hello\n\nend\n");

        let us = units(&doc);
        assert_eq!(block_type(&us[1]), Some(BlockType::Task));
        let plan = convert(&doc, &us, 1, BlockType::Numbered).unwrap();
        let (text, _) = apply(&mut doc, plan);
        assert_eq!(text, "intro\n\n1. hello\n\nend\n");

        let us = units(&doc);
        assert_eq!(block_type(&us[1]), Some(BlockType::Numbered));
        // 已是目标类型：无事发生
        assert!(convert(&doc, &us, 1, BlockType::Numbered).is_none());
        let plan = convert(&doc, &us, 1, BlockType::Text).unwrap();
        let (text, focused) = apply(&mut doc, plan);
        assert_eq!(text, "intro\n\nhello\n\nend\n");
        assert_eq!(focused.as_deref(), Some("hello"));
    }

    #[test]
    fn convert_to_code_quote_and_divider() {
        let mut doc = Document::from_text("a\n\nb\n");
        let us = units(&doc);
        let plan = convert(&doc, &us, 0, BlockType::Code).unwrap();
        // 光标在正文末尾（围栏之前）
        assert_eq!(plan.focus_offset, 5);
        let (text, focused) = apply(&mut doc, plan);
        assert_eq!(text, "```\na\n```\n\nb\n");
        assert_eq!(focused.as_deref(), Some("```\na\n```"));
        // 代码块 → 文本：去围栏
        let us = units(&doc);
        assert_eq!(block_type(&us[0]), Some(BlockType::Code));
        let plan = convert(&doc, &us, 0, BlockType::Text).unwrap();
        let (text, _) = apply(&mut doc, plan);
        assert_eq!(text, "a\n\nb\n");

        // 引用
        let us = units(&doc);
        let plan = convert(&doc, &us, 1, BlockType::Quote).unwrap();
        let (text, _) = apply(&mut doc, plan);
        assert_eq!(text, "a\n\n> b\n");
        let us = units(&doc);
        assert_eq!(block_type(&us[1]), Some(BlockType::Quote));

        // 分割线：非空块在后面插一条并新开段落（焦点落在新段落的空位）；空块（空列表项）直接变
        let mut doc = Document::from_text("x\n");
        let us = units(&doc);
        let plan = convert(&doc, &us, 0, BlockType::Divider).unwrap();
        assert_eq!(plan.focus_offset, 8);
        let (text, _) = apply(&mut doc, plan);
        assert_eq!(text, "x\n\n---\n\n\n");
        let mut doc = Document::from_text("- \n");
        let us = units(&doc);
        let plan = convert(&doc, &us, 0, BlockType::Divider).unwrap();
        let (text, _) = apply(&mut doc, plan);
        assert_eq!(text, "---\n");
        let mut doc = Document::from_text("- item\n");
        let us = units(&doc);
        let plan = convert(&doc, &us, 0, BlockType::Heading(1)).unwrap();
        let (text, focused) = apply(&mut doc, plan);
        assert_eq!(text, "# item\n");
        assert_eq!(focused.as_deref(), Some("# item"));
    }

    #[test]
    fn enter_splits_paragraph() {
        let mut doc = Document::from_text("hello world\n");
        let us = units(&doc);
        let plan = split(&doc, Some(&us[0]), &us[0].range, 5);
        // 光标后的空格被吃掉，新段落从 "world" 开始、焦点正好落在它上面
        assert_eq!(plan.focus_offset, 7);
        let (text, focused) = apply(&mut doc, plan);
        assert_eq!(text, "hello\n\nworld\n");
        assert_eq!(focused.as_deref(), Some("world"));

        // 列表项中间拆分同样处理空格
        let mut doc = Document::from_text("- ab cd\n");
        let us = units(&doc);
        let plan = split(&doc, Some(&us[0]), &us[0].range, 2);
        let (text, focused) = apply(&mut doc, plan);
        assert_eq!(text, "- ab\n- cd\n");
        assert_eq!(focused.as_deref(), Some("cd"));
    }

    #[test]
    fn enter_at_end_makes_empty_paragraph() {
        let mut doc = Document::from_text("# Title\n");
        let us = units(&doc);
        let plan = split(&doc, Some(&us[0]), &us[0].range, 7);
        assert_eq!(plan.focus_offset, 9);
        let (text, focused) = apply(&mut doc, plan);
        assert_eq!(text, "# Title\n\n\n");
        // 空段落没有块，焦点落在空位
        assert_eq!(focused, None);
    }

    #[test]
    fn enter_in_list_item_continues_list() {
        let mut doc = Document::from_text("- a\n- b\n");
        let us = units(&doc);
        assert_eq!(&doc.to_string()[us[0].range.clone()], "a");
        let plan = split(&doc, Some(&us[0]), &us[0].range, 1);
        let (text, focused) = apply(&mut doc, plan);
        assert_eq!(text, "- a\n- \n- b\n");
        // 新的空项是一个单位
        assert_eq!(focused.as_deref(), Some(""));
        let us = units(&doc);
        assert_eq!(us.len(), 3);
        assert!(us[1].range.is_empty());
    }

    #[test]
    fn enter_in_ordered_task_item() {
        let mut doc = Document::from_text("1. [x] done\n");
        let us = units(&doc);
        let plan = split(&doc, Some(&us[0]), &us[0].range, us[0].range.len());
        let (text, _) = apply(&mut doc, plan);
        assert_eq!(text, "1. [x] done\n2. [ ] \n");
    }

    #[test]
    fn enter_on_empty_item_leaves_list() {
        let mut doc = Document::from_text("- a\n- \n");
        let us = units(&doc);
        assert!(us[1].range.is_empty());
        let plan = split(&doc, Some(&us[1]), &us[1].range, 0);
        // 焦点落在空行上（"- a\n\n" 之后）
        assert_eq!(plan.focus_offset, 5);
        let (text, focused) = apply(&mut doc, plan);
        assert_eq!(text, "- a\n\n\n");
        assert_eq!(focused, None);
    }

    #[test]
    fn enter_on_empty_nested_item_outdents() {
        // 注意 CommonMark 不允许空列表项打断段落，所以 "- a\n  - \n" 里的 "- " 只是 a 的续行；
        // 空的嵌套项只能出现在非空兄弟之后
        let mut doc = Document::from_text("- a\n  - b\n  - \n");
        let us = units(&doc);
        assert_eq!(us.len(), 3);
        let nested = &us[2];
        assert!(nested.range.is_empty());
        assert!(nested.list.as_ref().unwrap().parent.is_some());
        let plan = split(&doc, Some(nested), &nested.range, 0);
        let (text, focused) = apply(&mut doc, plan);
        assert_eq!(text, "- a\n  - b\n- \n");
        assert_eq!(focused.as_deref(), Some(""));
    }

    #[test]
    fn backspace_merges_into_previous_paragraph() {
        let mut doc = Document::from_text("aaa\n\nbbb\n");
        let us = units(&doc);
        match backspace_at_start(&doc, &us, 1) {
            Backspace::Edit(plan) => {
                assert_eq!(plan.focus_offset, 3);
                let (text, focused) = apply(&mut doc, plan);
                assert_eq!(text, "aaabbb\n");
                assert_eq!(focused.as_deref(), Some("aaabbb"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn backspace_heading_into_paragraph_drops_hashes() {
        let mut doc = Document::from_text("intro\n\n## Head\n");
        let us = units(&doc);
        let Backspace::Edit(plan) = backspace_at_start(&doc, &us, 1) else { panic!() };
        let (text, _) = apply(&mut doc, plan);
        assert_eq!(text, "introHead\n");
    }

    #[test]
    fn backspace_leaves_list() {
        let mut doc = Document::from_text("- a\n- b\n");
        let us = units(&doc);
        let Backspace::Edit(plan) = backspace_at_start(&doc, &us, 1) else { panic!() };
        let (text, focused) = apply(&mut doc, plan);
        assert_eq!(text, "- a\n\nb\n");
        assert_eq!(focused.as_deref(), Some("b"));
        // 首项：直接变段落
        let mut doc = Document::from_text("- a\n");
        let us = units(&doc);
        let Backspace::Edit(plan) = backspace_at_start(&doc, &us, 0) else { panic!() };
        let (text, _) = apply(&mut doc, plan);
        assert_eq!(text, "a\n");
    }

    #[test]
    fn backspace_on_empty_slot_removes_blank_line() {
        let mut doc = Document::from_text("para\n\n\n");
        let us = units(&doc);
        let Backspace::Edit(plan) = backspace_at_empty_slot(&doc, &us, 6) else { panic!() };
        assert_eq!(plan.focus_offset, 4);
        let (text, focused) = apply(&mut doc, plan);
        assert_eq!(text, "para\n");
        assert_eq!(focused.as_deref(), Some("para"));
        assert_eq!(backspace_at_empty_slot(&Document::from_text(""), &[], 0), Backspace::Nothing);
    }

    #[test]
    fn backspace_after_code_fence_only_moves() {
        let doc = Document::from_text("```\nx\n```\n\npara\n");
        let us = units(&doc);
        assert_eq!(
            backspace_at_start(&doc, &us, 1),
            Backspace::MoveTo { unit: 0, cursor: us[0].range.len() }
        );
        assert_eq!(backspace_at_start(&doc, &us, 0), Backspace::Nothing);
    }

    #[test]
    fn vertical_moves_keep_column() {
        let doc = Document::from_text("first line\nsecond\n\nnext paragraph\n");
        let us = units(&doc);
        assert_eq!(us.len(), 2);
        // 从第二段第 3 列向上：到第一段最后一行 "second" 第 3 列
        let (ix, cursor) = up_target(&doc, &us, us[1].range.start, 3).unwrap();
        assert_eq!(ix, 0);
        assert_eq!(cursor, "first line\n".len() + 3);
        // 列超出目标行长度时夹到行尾
        let (_, cursor) = up_target(&doc, &us, us[1].range.start, 40).unwrap();
        assert_eq!(cursor, us[0].range.len());
        let (ix, cursor) = down_target(&doc, &us, &us[0].range, 4).unwrap();
        assert_eq!((ix, cursor), (1, 4));
        assert_eq!(down_target(&doc, &us, &us[1].range, 0), None);
    }

    #[test]
    fn unit_spans_cover_whole_lines_and_list_items() {
        let doc = Document::from_text("# T\n\n- a\n  - b\n- c\n\n```rs\nx\n```\n\npara ^p1\n");
        let us = units(&doc);
        let text = doc.to_string();
        let kinds: Vec<&str> = us.iter().map(|u| u.kind.tag()).collect();
        assert_eq!(kinds, ["heading", "paragraph", "paragraph", "paragraph", "code_fence", "paragraph"]);
        assert_eq!(&text[units_span(&doc, &us, 0, 0)], "# T");
        // 列表项按整项：a 带着嵌套的 b
        assert_eq!(&text[units_span(&doc, &us, 1, 1)], "- a\n  - b");
        assert_eq!(&text[units_span(&doc, &us, 2, 2)], "  - b");
        assert_eq!(&text[units_span(&doc, &us, 1, 3)], "- a\n  - b\n- c");
        assert_eq!(&text[units_span(&doc, &us, 4, 4)], "```rs\nx\n```");
        // 尾标记同行，一起算
        assert_eq!(&text[units_span(&doc, &us, 5, 5)], "para ^p1");
        assert_eq!(&text[units_span(&doc, &us, 0, 5)], text.trim_end());
    }

    #[test]
    fn delete_units_removes_lines_and_surrounding_blank_lines() {
        let mut doc = Document::from_text("aaa\n\nbbb\n\nccc\n");
        let us = units(&doc);
        doc.edit(delete_units(&doc, &us, 1, 1)).unwrap();
        assert_eq!(doc.to_string(), "aaa\n\nccc\n");
        // 删最后一块：前面的空行也收掉
        doc.reparse();
        let us = units(&doc);
        doc.edit(delete_units(&doc, &us, 1, 1)).unwrap();
        assert_eq!(doc.to_string(), "aaa\n");
        // 删第一块
        let mut doc = Document::from_text("aaa\n\nbbb\n");
        let us = units(&doc);
        doc.edit(delete_units(&doc, &us, 0, 0)).unwrap();
        assert_eq!(doc.to_string(), "bbb\n");
        // 全删：空文档
        let mut doc = Document::from_text("aaa\n\nbbb\n");
        let us = units(&doc);
        doc.edit(delete_units(&doc, &us, 0, 1)).unwrap();
        assert_eq!(doc.to_string(), "");
        // 列表项：整行；紧邻的项不需要空行
        let mut doc = Document::from_text("- a\n- b\n- c\n");
        let us = units(&doc);
        doc.edit(delete_units(&doc, &us, 1, 1)).unwrap();
        assert_eq!(doc.to_string(), "- a\n- c\n");
        // 标题与紧跟的段落之间没有空行
        let mut doc = Document::from_text("# T\npara\n");
        let us = units(&doc);
        doc.edit(delete_units(&doc, &us, 0, 0)).unwrap();
        assert_eq!(doc.to_string(), "para\n");
    }

    #[test]
    fn insert_slash_below_opens_a_separate_paragraph() {
        // 后面隔着空行
        let mut doc = Document::from_text("aaa\n\nbbb\n");
        let us = units(&doc);
        let plan = insert_slash_below(&doc, &us, 0);
        let (text, focused) = apply(&mut doc, plan);
        assert_eq!(text, "aaa\n\n/\n\nbbb\n");
        assert_eq!(focused.as_deref(), Some("/"));
        // 紧跟内容（标题后的段落）：多补一个换行，`/` 不并进下一段
        let mut doc = Document::from_text("# T\npara\n");
        let us = units(&doc);
        let plan = insert_slash_below(&doc, &us, 0);
        let (text, focused) = apply(&mut doc, plan);
        assert_eq!(text, "# T\n\n/\n\npara\n");
        assert_eq!(focused.as_deref(), Some("/"));
        // 最后一块 / 列表项之后
        let mut doc = Document::from_text("- a\n  - b\n- c");
        let us = units(&doc);
        let plan = insert_slash_below(&doc, &us, 0);
        let (text, _) = apply(&mut doc, plan);
        assert_eq!(text, "- a\n  - b\n\n/\n\n- c");
        let us = units(&doc);
        let plan = insert_slash_below(&doc, &us, us.len() - 1);
        let (text, focused) = apply(&mut doc, plan);
        assert_eq!(text, "- a\n  - b\n\n/\n\n- c\n\n/");
        assert_eq!(focused.as_deref(), Some("/"));
    }

    #[test]
    fn append_paragraph_pads_newlines() {
        assert_eq!(append_paragraph(&Document::from_text("")), (None, 0));
        assert_eq!(append_paragraph(&Document::from_text("a")), (Some(Edit::insert(1, "\n\n")), 3));
        assert_eq!(append_paragraph(&Document::from_text("a\n")), (Some(Edit::insert(2, "\n")), 3));
        assert_eq!(append_paragraph(&Document::from_text("a\n\n")), (None, 3));
    }
}
