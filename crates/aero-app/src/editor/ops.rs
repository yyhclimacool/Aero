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
    fn append_paragraph_pads_newlines() {
        assert_eq!(append_paragraph(&Document::from_text("")), (None, 0));
        assert_eq!(append_paragraph(&Document::from_text("a")), (Some(Edit::insert(1, "\n\n")), 3));
        assert_eq!(append_paragraph(&Document::from_text("a\n")), (Some(Edit::insert(2, "\n")), 3));
        assert_eq!(append_paragraph(&Document::from_text("a\n\n")), (None, 3));
    }
}
