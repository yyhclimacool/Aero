//! 结构性编辑的"计划"：拆块、合并、跨块移动。纯函数，只看 `Document` 与编辑单位，
//! 不碰 GPUI，因而可以在没有窗口的情况下测试（`editor/mod.rs` 只负责把计划应用到文档并移动焦点）。

use std::ops::Range;

use aero_core::{BlockId, BlockKind, Document, Edit};

use super::block_ops;
use super::text_ops::{ListMarker, heading_prefix_len, line_start, strip_continuation};
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
    if text.is_empty() {
        let container = unit.and_then(|u| doc.blocks().find(u.id)).or_else(|| doc.block_at(range.start));
        if let Some(container) = container.filter(|b| aero_core::containers::is_editable(&b.kind)) {
            let at_end = container
                .children
                .iter()
                .filter(|block| block.kind != BlockKind::Marker)
                .all(|block| doc.range_of(block).end <= range.start);
            let prefix = aero_core::containers::continuation(doc, container.id);
            if !at_end {
                let prefix = if matches!(container.kind, BlockKind::BlockQuote) {
                    format!("{prefix}> ")
                } else {
                    prefix
                };
                let separator = format!("\n{prefix}\n{prefix}");
                return Plan {
                    focus_offset: range.start + separator.len(),
                    edit: Edit::insert(range.start, separator),
                };
            }
            let at = aero_core::properties::owned_range(doc, container.id).unwrap().end;
            let separator = format!("\n{prefix}\n{prefix}");
            return Plan { focus_offset: at + separator.len(), edit: Edit::insert(at, separator) };
        }
    }
    // 光标两侧的空格一并吃掉：Markdown 会忽略段首空白，留着会让新块的起点与焦点错位
    let ws_before = text[..cursor].len() - text[..cursor].trim_end_matches([' ', '\t']).len();
    let ws_after = text[cursor..].len() - text[cursor..].trim_start_matches([' ', '\t']).len();
    let cut = doc_cursor - ws_before..doc_cursor + ws_after;

    // The suffix belongs to the original body, never to the newly-created right half.
    if let Some(unit) = unit
        && let Some(block) = doc.blocks().find(unit.id)
        && unit.is_text()
    {
        let owned = aero_core::properties::owned_range(doc, block.id).unwrap();
        let (mut left, mut right) = aero_core::inline_format::split_at(&text, cursor);
        let heading = heading_prefix_len(&text);
        let left_empty = left.trim().is_empty()
            || (heading > 0 && (cursor <= heading || left.trim() == text[..heading].trim()));
        if left_empty && heading > 0 {
            left.clear();
            right = text.clone();
        }
        let mut copied = block.attrs.clone();
        aero_core::properties::set(&mut copied, "aero-aliases", None);
        let original_suffix = doc.slice(range.end..owned.end).to_string();
        let suffix = |body: &str, original: bool| {
            if body.is_empty() {
                return String::new();
            }
            if original {
                return format!("{body}{original_suffix}");
            }
            let mut out = body.to_owned();
            if !copied.is_empty() {
                out.push_str(&format!(" {{{}}}", aero_core::properties::render(&copied)));
            }
            out
        };
        let list = unit.list.as_ref().filter(|ctx| {
            doc.blocks().iter().any(|(_, item)| {
                matches!(item.kind, BlockKind::ListItem { .. })
                    && doc.range_of(item) == ctx.item_range
                    && item.children.iter().any(|child| child.id == unit.id)
            })
        });
        if left_empty
            && let Some(ctx) = list
            && doc.blocks().iter().any(|(_, item)| {
                doc.range_of(item) == ctx.item_range
                    && item
                        .children
                        .iter()
                        .find(|b| b.kind != BlockKind::Marker)
                        .is_some_and(|b| b.id == unit.id)
            })
        {
            // Enter before the first body inserts an empty sibling before
            // the intact item, including its owned header and nested subtree.
            let source = doc.to_string();
            let start = line_start(&source, ctx.item_range.start);
            let number = ctx.ordered.map(|s| s + ctx.item_index as u32);
            let marker =
                ListMarker::from_item_source(&source[ctx.item_range.clone()], number, ctx.task).text();
            let inserted = format!("{}{marker}\n", &source[start..ctx.item_range.start]);
            return Plan { focus_offset: range.start + inserted.len(), edit: Edit::insert(start, inserted) };
        }
        let separator = if let Some(ctx) = list {
            let source = doc.to_string();
            let next = ctx.ordered.map(|s| s + ctx.item_index as u32 + 1);
            let start = line_start(&source, ctx.item_range.start);
            format!(
                "\n{}{}",
                &source[start..ctx.item_range.start],
                ListMarker::from_item_source(&source[ctx.item_range.clone()], next, ctx.task).text()
            )
        } else {
            let prefix = aero_core::containers::continuation(doc, unit.id);
            format!("\n{prefix}\n{prefix}")
        };
        let left = suffix(&left, !left_empty);
        let right = suffix(&right, left_empty);
        // Existing continuation paragraphs / nested children stay with the
        // original item rather than silently moving under the new item.
        let end = list.map_or(owned.end, |ctx| ctx.item_range.end.max(owned.end));
        let tail = doc.slice(owned.end..end).to_string();
        let focus = range.start + left.len() + tail.len() + separator.len();
        return Plan {
            focus_offset: focus,
            edit: Edit::replace(range.start..end, format!("{left}{tail}{separator}{right}")),
        };
    }

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

    let prefix = unit.map(|u| aero_core::containers::continuation(doc, u.id)).unwrap_or_default();
    let separator = format!("\n{prefix}\n{prefix}");
    Plan { focus_offset: cut.start + separator.len(), edit: Edit::replace(cut, separator) }
}

#[derive(Debug, PartialEq)]
pub enum Backspace {
    SelectContainer(aero_core::BlockId),
    Edit(Plan),
    /// 不改文档，只把焦点移到某个单位的某个偏移
    MoveTo {
        unit: usize,
        cursor: usize,
    },
    StyleMismatch {
        unit: usize,
        cursor: usize,
    },
    Nothing,
}

/// ⌫ 在单位开头：列表项的第一行脱离列表；其余段落 / 标题并入上一行
/// （上一行可以是段落、标题、列表项正文、引用或 callout，标题自己的 `#` 会去掉）。
/// 上一块是代码、数学、表格这类不能接文字的块时，只把光标移过去。
pub fn backspace_at_start(doc: &Document, units: &[Unit], ix: usize) -> Backspace {
    let unit = &units[ix];
    let doc_text = doc.to_string();
    if let Some(block) = doc.blocks().find(unit.id).filter(|b| aero_core::containers::is_editable(&b.kind)) {
        return Backspace::SelectContainer(block.id);
    }
    if let Some(container) = aero_core::containers::ancestors(doc, unit.id)
        .into_iter()
        .rev()
        .find(|b| aero_core::containers::is_editable(&b.kind))
        && !units[..ix].iter().any(|u| u.range.start >= doc.range_of(container).start)
    {
        return if unit.range.is_empty() {
            Backspace::SelectContainer(container.id)
        } else {
            Backspace::MoveTo { unit: ix, cursor: 0 }
        };
    }

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
    // 列表项的第一行已经在上面脱离列表。这里的段落即使跟在列表项、引用后面，也要并回上一行。
    let parent = |id| {
        doc.blocks().iter().find(|(_, b)| b.children.iter().any(|child| child.id == id)).map(|(_, b)| b.id)
    };
    let style = |id| {
        let mut attrs = doc.blocks().find(id).map(|b| b.attrs.clone()).unwrap_or_default();
        attrs.id = None;
        aero_core::properties::set(&mut attrs, "aero-aliases", None);
        aero_core::properties::set(&mut attrs, "aero-owner", None);
        attrs
    };
    let siblings = unit.is_text() && prev.is_text() && parent(unit.id) == parent(prev.id);
    if siblings && style(unit.id) != style(prev.id) {
        return Backspace::StyleMismatch { unit: ix - 1, cursor: prev.range.len() };
    }
    let mergeable = siblings;
    if mergeable {
        if [prev.id, unit.id].iter().any(|id| {
            doc.blocks()
                .find(*id)
                .and_then(|b| b.persisted_id.as_deref())
                .is_some_and(|id| !aero_core::identity::is_block_id(id))
        }) {
            let mut normalized = doc.clone();
            for id in [prev.id, unit.id] {
                let Ok(assignment) = aero_core::block_refs::permanent(&normalized, id) else {
                    return Backspace::Nothing;
                };
                if normalized.edit_group(assignment.edits).is_err() {
                    return Backspace::Nothing;
                }
                normalized.reparse();
            }
            let normalized_units = super::units::units(&normalized);
            if let Backspace::Edit(plan) = backspace_at_start(&normalized, &normalized_units, ix) {
                let at = plan.focus_offset;
                if normalized.edit(plan.edit).is_ok() {
                    return Backspace::Edit(Plan {
                        edit: Edit::replace(0..doc.len(), normalized.to_string()),
                        focus_offset: at,
                    });
                }
            }
            return Backspace::Nothing;
        }
        let strip = if matches!(unit.kind, BlockKind::Heading { .. }) {
            heading_prefix_len(&doc_text[unit.range.clone()])
        } else {
            0
        };
        let previous = doc.blocks().find(prev.id).unwrap();
        let current = doc.blocks().find(unit.id).unwrap();
        if previous.persisted_id.is_some() || current.persisted_id.is_some() || !previous.attrs.is_empty() {
            let left = &doc_text[prev.range.clone()];
            let right = &doc_text[unit.range.start + strip..unit.range.end];
            let mut attrs = previous.attrs.clone();
            let primary = previous.persisted_id.as_deref().or(current.persisted_id.as_deref());
            let mut aliases: Vec<String> = aero_core::block_refs::aliases(previous)
                .chain(aero_core::block_refs::aliases(current))
                .map(str::to_owned)
                .collect();
            if previous.persisted_id.is_some()
                && let Some(id) = current.persisted_id.as_deref()
                && Some(id) != primary
            {
                aliases.push(id.into());
            }
            aliases.sort();
            aliases.dedup();
            if !aliases.is_empty() {
                aero_core::properties::set(&mut attrs, "aero-aliases", Some(&aliases.join(" ")));
            }
            let mut combined = format!("{left}{right}");
            if !attrs.is_empty() {
                combined.push_str(&format!(" {{{}}}", aero_core::properties::render(&attrs)));
            }
            if let Some(id) = primary {
                combined.push_str(&format!(" ^{id}"));
            }
            let end = aero_core::properties::owned_range(doc, unit.id).unwrap().end;
            return Backspace::Edit(Plan {
                focus_offset: prev.range.end,
                edit: Edit::replace(prev.range.start..end, combined),
            });
        }
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
/// 用于平面拖拽在列表项边缘定位落点。
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

/// 拖拽落点：插到第 `before` 个单位之前；`parent` 有值时成为那个列表项的子项。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DropPlace {
    pub before: usize,
    pub parent: Option<usize>,
}

/// 把单位 `from` 搬到 `place`。列表之间搬动会改标记和缩进；缩进一层成为子项。
/// 原地、或搬进自己的子树时返回 `None`。
pub fn move_unit(doc: &Document, units: &[Unit], from: usize, place: DropPlace) -> Option<Plan> {
    if from >= units.len() || place.before > units.len() {
        return None;
    }
    if place.parent == Some(from) {
        return None;
    }
    if place.parent.is_none() && place.before == from {
        return None;
    }
    if place.parent.is_none() && place.before == from + 1 {
        let src_list = units[from].list.is_some();
        let dest_list = place.before < units.len() && units[place.before].list.is_some();
        if src_list == dest_list {
            return None;
        }
    }
    if let Some(parent) = place.parent {
        if parent >= units.len() || contains_unit(units, from, parent) {
            return None;
        }
    } else if place.before < units.len() && contains_unit(units, from, place.before) {
        return None;
    }
    let text = doc.to_string();
    let span = units_span(doc, units, from, from);
    // Take the source line ending and at most one adjacent blank line.
    let separator = text[span.end..].bytes().take(2).take_while(|&b| b == b'\n').count();
    let mut cut = span.start..span.end + separator;
    if cut.end == text.len() && text[..cut.start].ends_with("\n\n") {
        cut.start -= 1;
    }
    let piece = adapt_moved_block(&text[span], &units[from], units, place, &text);
    let insert_at = if let Some(parent) = place.parent {
        let mut at =
            units[parent].list.as_ref().map(|ctx| ctx.item_range.end).unwrap_or(units[parent].range.end);
        // 项的源区间常常停在行尾换行之前。先跨过这个换行，子项才接在下一行，而不会把下一行顶出空行。
        if at < text.len() && text.as_bytes()[at] == b'\n' {
            at += 1;
        }
        at
    } else if place.before >= units.len() {
        text.len()
    } else {
        line_start(&text, units_span(doc, units, place.before, place.before).start)
    };
    if place.parent.is_none() && insert_at > cut.start && insert_at < cut.end {
        return None;
    }
    let plan = assemble_move(&text, cut, insert_at, &piece);
    (text[plan.edit.range.clone()] != plan.edit.text).then_some(plan)
}

fn contains_unit(units: &[Unit], ancestor: usize, index: usize) -> bool {
    if index == ancestor {
        return false;
    }
    let span = units[ancestor]
        .list
        .as_ref()
        .map(|ctx| ctx.item_range.clone())
        .unwrap_or_else(|| units[ancestor].range.clone());
    let unit = &units[index];
    unit.range.start >= span.start && unit.range.end <= span.end && span.start != span.end
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ListLevelDirection {
    Indent,
    Outdent,
}

fn list_level_target(
    doc: &Document,
    id: BlockId,
    direction: ListLevelDirection,
) -> Option<(BlockId, bool, bool)> {
    let item = doc.blocks().find(id)?;
    if !matches!(item.kind, BlockKind::ListItem { .. }) {
        return None;
    }
    let ancestors = aero_core::containers::ancestors(doc, id);
    match direction {
        ListLevelDirection::Indent => {
            let parent = ancestors.last()?;
            let index = parent.children.iter().position(|block| block.id == id)?;
            let previous = parent.children[..index]
                .iter()
                .rev()
                .find(|block| matches!(block.kind, BlockKind::ListItem { .. }))?;
            Some((previous.id, false, true))
        }
        ListLevelDirection::Outdent => {
            let parent =
                ancestors.iter().rev().find(|block| matches!(block.kind, BlockKind::ListItem { .. }))?;
            Some((parent.id, true, false))
        }
    }
}

pub(super) fn can_change_list_level(doc: &Document, id: BlockId, direction: ListLevelDirection) -> bool {
    list_level_target(doc, id, direction).is_some()
}

pub(super) fn change_list_level(doc: &Document, id: BlockId, direction: ListLevelDirection) -> Option<Plan> {
    let (target, after, into) = list_level_target(doc, id, direction)?;
    let following = if direction == ListLevelDirection::Outdent {
        let ancestors = aero_core::containers::ancestors(doc, id);
        let parent = ancestors.last()?;
        let index = parent.children.iter().position(|block| block.id == id)?;
        parent.children[index + 1..]
            .iter()
            .filter(|block| matches!(block.kind, BlockKind::ListItem { .. }))
            .map(|block| block.id)
            .collect()
    } else {
        Vec::new()
    };
    move_node_with_following(doc, id, target, after, into, &following)
}

fn subtree_end(doc: &Document, id: BlockId) -> Option<usize> {
    let block = doc.blocks().find(id)?;
    let mut end = aero_core::properties::owned_range(doc, id)?.end;
    for child in &block.children {
        let child_end = if child.kind == BlockKind::Marker {
            doc.range_of(child).end
        } else {
            subtree_end(doc, child.id)?
        };
        end = end.max(child_end);
    }
    Some(end)
}

pub fn move_node(
    doc: &Document,
    source: aero_core::BlockId,
    target: aero_core::BlockId,
    after: bool,
    into: bool,
) -> Option<Plan> {
    move_node_with_following(doc, source, target, after, into, &[])
}

fn move_node_with_following(
    doc: &Document,
    source: aero_core::BlockId,
    target: aero_core::BlockId,
    after: bool,
    into: bool,
    following: &[BlockId],
) -> Option<Plan> {
    let from = doc.blocks().find(source)?;
    let to = doc.blocks().find(target)?;
    let target_path = aero_core::containers::ancestors(doc, target);
    if source == target || target_path.iter().any(|b| b.id == source) {
        return None;
    }
    let parent = target_path.last();
    if !into && parent.map(|b| b.id) == aero_core::containers::ancestors(doc, source).last().map(|b| b.id) {
        let siblings = parent.map_or(&doc.blocks().roots, |b| &b.children);
        let mut ids = siblings.iter().filter(|block| block.kind != BlockKind::Marker).map(|block| block.id);
        let (left, right) = if after { (target, source) } else { (source, target) };
        if ids.any(|id| id == left) && ids.next() == Some(right) {
            return None;
        }
    }
    let mut target_range = aero_core::properties::owned_range(doc, target)?;
    if after && !into {
        target_range.end = subtree_end(doc, target)?;
    }
    let original = doc.to_string();
    let mut moving = Vec::with_capacity(1 + following.len());
    moving.push(source);
    moving.extend_from_slice(following);
    let delete = if following.is_empty() {
        block_ops::deletions(doc, &original, &moving).pop()?
    } else {
        Edit::delete(block_ops::deletion_span(doc, &original, &moving)?)
    };
    let range = delete.range.clone();
    let moves_out_of_target = after
        && !into
        && aero_core::containers::ancestors(doc, source).iter().any(|block| block.id == target);
    if moves_out_of_target {
        target_range.end = target_range.end.max(range.end);
    }
    let mut piece = block_ops::source(doc, &original, &[source]);
    if !following.is_empty() {
        let marker_range = aero_core::properties::owned_range(doc, source)?;
        let nested_prefix = from.children.first().map_or_else(
            || {
                let marker = &original[marker_range];
                let width = list_marker_len(marker).unwrap_or_else(|| marker.trim_end().len() + 1);
                " ".repeat(width)
            },
            |child| block_ops::continuation(doc, &original, child.id, doc.range_of(child).start).0,
        );
        let nested_prefix = if nested_prefix.contains('>') {
            let relative = nested_prefix.rsplit('>').next().unwrap_or(&nested_prefix);
            relative.strip_prefix([' ', '\t']).unwrap_or(relative).to_owned()
        } else {
            nested_prefix
        };
        for id in following {
            let child = block_ops::source(doc, &original, &[*id]);
            piece =
                format!("{piece}\n\n{nested_prefix}{}", child.replace('\n', &format!("\n{nested_prefix}")));
        }
    }
    let (at, prefix, task_header) = if into {
        if matches!(to.kind, BlockKind::ListItem { .. }) {
            let prefix = to.children.first().map_or_else(
                || {
                    let prefix = aero_core::containers::continuation(doc, target);
                    let marker = &original[target_range.clone()];
                    let width = list_marker_len(marker).unwrap_or_else(|| marker.trim_end().len() + 1);
                    format!("{prefix}{}", " ".repeat(width))
                },
                |child| block_ops::continuation(doc, &original, child.id, doc.range_of(child).start).0,
            );
            if !matches!(from.kind, BlockKind::ListItem { .. } | BlockKind::List { .. }) {
                piece = format!("- {}", piece.replace('\n', "\n  "));
            }
            (target_range.end, prefix, false)
        } else {
            let prefix = aero_core::containers::continuation(doc, target);
            match &to.kind {
                BlockKind::BlockQuote => (doc.range_of(to).end, format!("{prefix}> "), false),
                BlockKind::Directive { name } if name == "highlight" => {
                    let span = doc.range_of(to);
                    let offset = original[span.clone()].rfind('\n')? + span.start;
                    (offset, prefix, false)
                }
                _ => return None,
            }
        }
    } else {
        let (prefix, task_header) = block_ops::continuation(doc, &original, target, target_range.start);
        (if after { target_range.end } else { target_range.start }, prefix, task_header)
    };
    if range.start < at && at < range.end {
        return None;
    }
    let fence_edits = if into && matches!(&to.kind, BlockKind::Directive { name } if name == "highlight") {
        aero_core::containers::extend_nested_fences(doc, target, &piece).ok()?
    } else {
        Vec::new()
    };
    let piece = piece.trim_end_matches('\n').replace('\n', &format!("\n{prefix}"));
    let separator = format!("\n{}\n{prefix}", prefix.trim_end_matches([' ', '\t']));
    let (insertion, leading) = if into || after || task_header {
        let needs_tail = moves_out_of_target
            && matches!(from.kind, BlockKind::ListItem { .. })
            && at < original.len()
            && !original[at..].starts_with('\n');
        let tail_separator = format!("\n{}\n", prefix.trim_end_matches([' ', '\t']));
        let tail = if task_header && !after {
            separator.as_str()
        } else if needs_tail {
            tail_separator.as_str()
        } else {
            ""
        };
        (format!("{separator}{piece}{tail}"), separator.len())
    } else {
        (format!("{piece}{separator}"), 0)
    };
    let mut edits = vec![delete, Edit::insert(at, insertion)];
    edits.extend(fence_edits);
    edits.sort_by_key(|e| std::cmp::Reverse(e.range.start));
    let mut probe = doc.clone();
    probe.edit_group(edits).ok()?;
    probe.reparse();
    // Reindentation must never hide, duplicate, or detach a persistent identity.
    let identities = |doc: &Document| {
        let mut ids: Vec<_> = doc
            .blocks()
            .iter()
            .filter(|(_, b)| b.kind != BlockKind::Marker)
            .filter_map(|(_, b)| b.persisted_id.as_deref().map(str::to_owned))
            .collect();
        ids.sort();
        ids
    };
    if identities(doc) != identities(&probe) {
        return None;
    }
    if into {
        let moved = probe.blocks().find(source)?;
        if !aero_core::containers::ancestors(&probe, moved.id).iter().any(|block| block.id == target) {
            return None;
        }
    }
    let content_start = if matches!(from.kind, BlockKind::ListItem { .. }) {
        list_marker_len(&piece).unwrap_or(0)
    } else {
        0
    };
    let focus_offset = if at >= range.end { at - range.len() } else { at } + leading + content_start;
    let text = probe.to_string();
    (original != text).then(|| Plan { edit: Edit::replace(0..doc.len(), text), focus_offset })
}

fn adapt_moved_block(raw: &str, source: &Unit, units: &[Unit], place: DropPlace, text: &str) -> String {
    let body = if source.list.is_some() { strip_list_marker(raw) } else { raw.to_owned() };
    match destination_style(source, units, place, text) {
        Some(style) => prefix_list(&body, &style),
        None => body,
    }
}

struct ListStyle {
    indent: String,
    marker: String,
}

fn destination_style(source: &Unit, units: &[Unit], place: DropPlace, text: &str) -> Option<ListStyle> {
    if let Some(parent) = place.parent {
        let item = units.get(parent)?;
        let ctx = item.list.as_ref()?;
        let src = &text[ctx.item_range.clone()];
        let marker = ListMarker::from_item_source(src, None, false);
        return Some(ListStyle { indent: format!("{}  ", marker.indent), marker: "- ".into() });
    }
    if place.before < units.len() && units[place.before].list.is_none() {
        return None;
    }
    let neighbor = if place.before < units.len() { units.get(place.before) } else { units.last() };
    let ctx = neighbor?.list.as_ref()?;
    if place.before == units.len() && source.list.is_none() {
        return None;
    }
    let item = &text[ctx.item_range.clone()];
    let ordered = ctx.ordered.map(|start| start + ctx.item_index as u32);
    let marker =
        ListMarker::from_item_source(item, ordered, ctx.task && source.list.as_ref().is_some_and(|s| s.task));
    Some(ListStyle { indent: marker.indent, marker: marker.marker })
}

fn strip_list_marker(piece: &str) -> String {
    let mut lines = piece.split('\n');
    let first = lines.next().unwrap_or_default();
    let indent_len = first.chars().take_while(|c| *c == ' ' || *c == '\t').map(char::len_utf8).sum();
    let Some(marker_len) = list_marker_len(&first[indent_len..]) else { return piece.to_owned() };
    let indent = &first[..indent_len];
    let mut body = first[indent_len + marker_len..].to_owned();
    for line in lines {
        body.push('\n');
        body.push_str(line.strip_prefix(indent).unwrap_or(line));
    }
    body
}

fn list_marker_len(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    if bytes.first().is_some_and(|c| matches!(c, b'-' | b'*' | b'+')) && bytes.get(1) == Some(&b' ') {
        let mut len = 2;
        if rest[len..].starts_with("[ ] ")
            || rest[len..].starts_with("[x] ")
            || rest[len..].starts_with("[X] ")
        {
            len += 4;
        }
        return Some(len);
    }
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        let delim = *bytes.get(digits)?;
        if (delim == b'.' || delim == b')') && bytes.get(digits + 1) == Some(&b' ') {
            return Some(digits + 2);
        }
    }
    None
}

fn prefix_list(body: &str, style: &ListStyle) -> String {
    let mut lines = body.split('\n');
    let first = lines.next().unwrap_or("");
    let mut out = format!("{}{}{first}", style.indent, style.marker);
    for line in lines {
        out.push('\n');
        if !line.is_empty() {
            out.push_str(&style.indent);
        }
        out.push_str(line);
    }
    out
}

fn assemble_move(text: &str, cut: Range<usize>, insert_at: usize, piece: &str) -> Plan {
    let move_up = insert_at <= cut.start;
    let (range, between, left, right) = if move_up {
        let between = &text[insert_at..cut.start];
        let right = if between.is_empty() { &text[cut.end..] } else { between };
        (insert_at..cut.end, between, &text[..insert_at], right)
    } else {
        let at = insert_at.max(cut.end);
        let between = &text[cut.end..at];
        let left = if between.is_empty() { &text[..cut.start] } else { between };
        (cut.start..at, between, left, &text[at..])
    };
    let separator = if is_list_piece(piece) { 1 } else { 2 };
    let leading = if left.is_empty() { 0 } else { missing_line_breaks(left, piece, separator) };
    let end_separator = if right.is_empty() { usize::from(text.ends_with('\n')) } else { separator };
    let trailing = missing_line_breaks(piece, right, end_separator);

    let mut replacement = String::with_capacity(between.len() + leading + piece.len() + trailing);
    if !move_up {
        replacement.push_str(between);
    }
    replacement.push_str(&"\n\n"[..leading]);
    let focus_offset = range.start + replacement.len();
    replacement.push_str(piece);
    replacement.push_str(&"\n\n"[..trailing]);
    if move_up {
        replacement.push_str(between);
    }
    Plan { edit: Edit::replace(range, replacement), focus_offset }
}

fn is_list_piece(piece: &str) -> bool {
    let line = piece.trim_start_matches('\n').lines().next().unwrap_or("");
    let indent = line.chars().take_while(|c| *c == ' ' || *c == '\t').map(char::len_utf8).sum();
    list_marker_len(&line[indent..]).is_some()
}

fn missing_line_breaks(left: &str, right: &str, separator: usize) -> usize {
    let left_breaks = left.bytes().rev().take(separator).take_while(|&b| b == b'\n').count();
    let right_breaks = right.bytes().take(separator).take_while(|&b| b == b'\n').count();
    separator.saturating_sub(left_breaks + right_breaks)
}

/// 段首刚打完的列表标记。光标还没走出这段标记时不认。标题和引用的标记留在单位源码里，不在这里。
pub fn typed_prefix_len(text: &str, cursor: usize) -> Option<usize> {
    if text.starts_with("- [ ] ") || text.starts_with("- [x] ") || text.starts_with("- [X] ") {
        return (cursor >= 6).then_some(6);
    }
    if matches!(text.as_bytes().first(), Some(b'-' | b'*' | b'+')) && text.as_bytes().get(1) == Some(&b' ') {
        return (cursor >= 2).then_some(2);
    }
    let digits = text.bytes().take_while(|byte| byte.is_ascii_digit()).count();
    if (1..=9).contains(&digits) {
        let rest = &text[digits..];
        if rest.starts_with(". ") || rest.starts_with(") ") {
            let len = digits + 2;
            return (cursor >= len).then_some(len);
        }
    }
    None
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
    Highlight,
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
    let id = conversion_target(doc, unit, target);
    if conversion_type(unit, id) == Some(target) {
        return None;
    }
    convert_node(doc, id, target).ok().flatten()
}

/// Keyboard and slash commands can address the first paragraph's list item.
/// Converting body text to code keeps the paragraph in its item.
pub fn conversion_target(doc: &Document, unit: &Unit, target: BlockType) -> BlockId {
    if target != BlockType::Code
        && matches!(unit.kind, BlockKind::Paragraph)
        && let Some(item) = unit.list.as_ref().and_then(|ctx| {
            doc.blocks().iter().map(|(_, b)| b).find(|b| {
                matches!(b.kind, BlockKind::ListItem { .. })
                    && doc.range_of(b) == ctx.item_range
                    && b.children
                        .iter()
                        .find(|c| c.kind != BlockKind::Marker)
                        .is_some_and(|c| c.id == unit.id)
            })
        })
    {
        return item.id;
    }
    unit.id
}

pub fn conversion_available(doc: &Document, unit: &Unit, target: BlockType) -> bool {
    let id = conversion_target(doc, unit, target);
    conversion_type(unit, id).is_some_and(|kind| kind != target)
        && conversion_reason(doc, id, target).is_none()
}

fn conversion_type(unit: &Unit, scope: BlockId) -> Option<BlockType> {
    if scope == unit.id && matches!(unit.kind, BlockKind::Paragraph) {
        return Some(BlockType::Text);
    }
    block_type(unit)
}

pub fn conversion_reason(doc: &Document, id: BlockId, target: BlockType) -> Option<&'static str> {
    let Some(block) = doc.blocks().find(id) else { return Some("目标块已不存在") };
    if matches!(target, BlockType::Quote | BlockType::Highlight) {
        return None;
    }
    if matches!(block.kind, BlockKind::List { .. }) {
        return (!matches!(target, BlockType::Bullet | BlockType::Numbered | BlockType::Task))
            .then_some("列表整体只能切换列表类型或包裹为容器");
    }
    if aero_core::containers::is_editable(&block.kind) {
        return Some("容器不能无损压缩为单个正文块，请使用解除容器");
    }
    if matches!(block.kind, BlockKind::ListItem { .. })
        && !matches!(target, BlockType::Bullet | BlockType::Numbered | BlockType::Task)
    {
        let children: Vec<_> = block.children.iter().filter(|b| b.kind != BlockKind::Marker).collect();
        if children.len() > 1
            || children.first().is_some_and(|b| {
                !matches!(
                    b.kind,
                    BlockKind::Paragraph | BlockKind::Heading { .. } | BlockKind::CodeFence { .. }
                )
            })
        {
            return Some("此列表项包含多个块");
        }
        let mut attrs = block.attrs.clone();
        aero_core::properties::set(&mut attrs, "aero-owner", None);
        if !attrs.is_empty() || block.persisted_id.is_some() {
            return Some("列表项带有独立样式或身份，请转换其正文子块以保留这些属性");
        }
    }
    if !matches!(
        block.kind,
        BlockKind::Paragraph
            | BlockKind::Heading { .. }
            | BlockKind::ListItem { .. }
            | BlockKind::CodeFence { .. }
            | BlockKind::CodeBlock
            | BlockKind::ThematicBreak
    ) {
        return Some("此块无法无损转换");
    }
    None
}

fn convert_leaf(doc: &Document, unit: &Unit, scope: BlockId, target: BlockType) -> Option<Plan> {
    let current = conversion_type(unit, scope)?;
    if current == target {
        return None;
    }
    let doc_text = doc.to_string();
    let start = doc.range_of(doc.blocks().find(scope)?).start;
    let end = aero_core::properties::owned_range(doc, unit.id)?.end;
    let mut source_prefix = aero_core::containers::continuation(doc, unit.id);
    // A task checkbox is first-line syntax. Its continuation uses only the list marker's indent.
    let task_header = unit.list.as_ref().filter(|ctx| ctx.task).and_then(|ctx| {
        let marker = doc_text.get(ctx.item_range.start..unit.range.start)?;
        if marker.contains('\n') || !matches!(unit.kind, BlockKind::Paragraph) {
            return None;
        }
        let checkbox = marker.find('[')?;
        source_prefix.truncate(source_prefix.len().checked_sub(marker.len() - checkbox)?);
        Some(())
    });
    let scope_prefix = (scope != unit.id).then(|| aero_core::containers::continuation(doc, scope));
    let prefix = scope_prefix.as_deref().unwrap_or(&source_prefix);
    let src = doc_text[unit.range.clone()]
        .split('\n')
        .enumerate()
        .map(|(i, line)| if i == 0 { line } else { strip_continuation(line, &source_prefix) })
        .collect::<Vec<_>>()
        .join("\n");
    let content: String = match current {
        BlockType::Heading(_) => src[heading_prefix_len(&src)..].to_owned(),
        BlockType::Code if matches!(unit.kind, BlockKind::CodeBlock) => {
            src.lines().map(|l| l.strip_prefix("    ").unwrap_or(l)).collect::<Vec<_>>().join("\n")
        }
        BlockType::Code => strip_fence(&src).to_owned(),
        BlockType::Divider => String::new(),
        _ => src,
    };
    let content = content.trim_end_matches('\n').to_owned();

    if target == BlockType::Divider && !content.trim().is_empty() {
        let text = format!("\n{prefix}\n{prefix}---\n{prefix}\n{prefix}");
        return Some(Plan { focus_offset: end + text.len(), edit: Edit::insert(end, text) });
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
        BlockType::Code => {
            let longest = content
                .lines()
                .map(|line| line.trim_start_matches([' ', '\t']).bytes().take_while(|&b| b == b'`').count())
                .max()
                .unwrap_or(0);
            let fence = "`".repeat(longest.max(2) + 1);
            (format!("{fence}\n{content}\n{fence}"), fence.len() + 1 + content.len())
        }
        BlockType::Divider => ("---".to_owned(), 3),
        BlockType::Quote | BlockType::Highlight => unreachable!(),
    };
    let block = doc.blocks().find(unit.id)?;
    let mut text = text;
    let metadata = if block.attrs.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", aero_core::properties::render(&block.attrs))
    };
    if !metadata.is_empty() || block.persisted_id.is_some() {
        text.push_str(if matches!(target, BlockType::Code | BlockType::Divider) { "\n\n" } else { " " });
        text.push_str(&metadata);
        if let Some(id) = block.persisted_id.as_deref() {
            if !metadata.is_empty() {
                text.push(' ');
            }
            text.push('^');
            text.push_str(id);
        }
    }
    let mut focus_offset =
        start + cursor + text[..cursor].bytes().filter(|&b| b == b'\n').count() * prefix.len();
    let mut text = text.replace('\n', &format!("\n{prefix}"));
    if target == BlockType::Code && scope == unit.id && task_header.is_some() {
        // The checkbox needs its own line so Markdown can recognize both the task and the fence.
        let blank_prefix = prefix.trim_end_matches([' ', '\t']);
        let header = format!("\n{blank_prefix}\n{prefix}");
        focus_offset += header.len();
        text.insert_str(0, &header);
    }
    Some(Plan { focus_offset, edit: Edit::replace(start..end, text) })
}

/// Container handles address tree nodes, independently of their leaf editors.
pub fn convert_node(
    doc: &Document,
    id: aero_core::BlockId,
    target: BlockType,
) -> Result<Option<Plan>, String> {
    if let Some(reason) = conversion_reason(doc, id, target) {
        return Err(reason.into());
    }
    let block = doc.blocks().find(id).ok_or("目标块已不存在")?;
    if matches!(target, BlockType::Quote | BlockType::Highlight) {
        let edit = aero_core::containers::wrap(doc, id, target == BlockType::Highlight)?;
        return Ok(Some(Plan { focus_offset: edit.range.start, edit }));
    }
    if let BlockKind::List { .. } = block.kind {
        if !matches!(target, BlockType::Bullet | BlockType::Numbered | BlockType::Task) {
            return Err("列表整体只能切换列表类型或包裹为容器".into());
        }
        let mut edits = Vec::new();
        for (index, item) in
            block.children.iter().filter(|b| matches!(b.kind, BlockKind::ListItem { .. })).enumerate()
        {
            edits.push(item_marker(doc, item, target, index + 1)?);
        }
        edits.sort_by_key(|e| std::cmp::Reverse(e.range.start));
        let mut probe = doc.clone();
        probe.edit_group(edits).map_err(|e| e.to_string())?;
        return Ok(Some(Plan {
            focus_offset: doc.range_of(block).start,
            edit: Edit::replace(0..doc.len(), probe.to_string()),
        }));
    }
    if matches!(block.kind, BlockKind::ListItem { .. }) {
        if matches!(target, BlockType::Bullet | BlockType::Numbered | BlockType::Task) {
            let edit = item_marker(doc, block, target, 1)?;
            let header = block.attrs.pairs.iter().any(|(key, value)| key == "aero-owner" && value == "item");
            let mut end = 0;
            for (index, line) in edit.text.split_inclusive('\n').enumerate() {
                end += line.len();
                if (!header || index > 0) && !line.trim().is_empty() {
                    end -= usize::from(line.ends_with('\n'));
                    break;
                }
            }
            return Ok(Some(Plan { focus_offset: edit.range.start + end, edit }));
        }
        let children: Vec<_> = block.children.iter().filter(|b| b.kind != BlockKind::Marker).collect();
        let units = super::units::units(doc);
        let leaf = children.first().map_or(block.id, |b| b.id);
        let unit = units.iter().find(|u| u.id == leaf).ok_or("正文不可转换")?;
        return Ok(convert_leaf(doc, unit, id, target));
    }
    if aero_core::containers::is_editable(&block.kind) {
        return Err("容器不能无损压缩为单个正文块，请使用解除容器".into());
    }
    let units = super::units::units(doc);
    let unit = units.iter().find(|u| u.id == id).ok_or("此块不支持类型转换")?;
    Ok(convert_leaf(doc, unit, id, target))
}

fn item_marker(
    doc: &Document,
    item: &aero_core::Block,
    target: BlockType,
    number: usize,
) -> Result<Edit, String> {
    let range = aero_core::properties::owned_range(doc, item.id).ok_or("列表项已不存在")?;
    let src = doc.slice(range.clone()).to_string();
    let old = list_marker_len(&src).ok_or("无法定位列表标记")?;
    let first = src.find('\n').unwrap_or(src.len());
    let prefix = match target {
        BlockType::Bullet => "- ".into(),
        BlockType::Task => "- [ ] ".into(),
        BlockType::Numbered => format!("{number}. "),
        _ => return Err("无效列表类型".into()),
    };
    let old_indent = src.bytes().take_while(|b| b.is_ascii_digit()).count();
    let old_indent = if old_indent > 0 { old_indent + 2 } else { 2 };
    let new_indent = if target == BlockType::Numbered { number.to_string().len() + 2 } else { 2 };
    let external = aero_core::containers::continuation(doc, item.id);
    let mut result = format!("{prefix}{}", &src[old..first]);
    for line in src[first..].split('\n').skip(1) {
        result.push('\n');
        let local = line.strip_prefix(external.as_str()).unwrap_or(line);
        result.push_str(&external);
        if let Some(rest) = local.strip_prefix(&" ".repeat(old_indent)) {
            result.push_str(&" ".repeat(new_indent));
            result.push_str(rest);
        } else {
            result.push_str(local);
        }
    }
    Ok(Edit::replace(range, result))
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
        BlockType::Highlight => (":::highlight\n\n:::".to_owned(), ":::highlight\n".len()),
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

/// 任务项源码（`item_src` 从行首缩进开始，位于文档 `base`）里复选框的勾：`[ ]` ↔ `[x]`。
pub fn toggle_task(item_src: &str, base: usize) -> Option<Edit> {
    let indent = item_src.len() - item_src.trim_start_matches([' ', '\t']).len();
    let s = &item_src[indent..];
    let marker = if s.starts_with(['-', '*', '+']) {
        1
    } else {
        let digits = s.bytes().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0 || !matches!(s.as_bytes().get(digits), Some(b'.' | b')')) {
            return None;
        }
        digits + 1
    };
    let spaces = s[marker..].len() - s[marker..].trim_start_matches(' ').len();
    let box_at = indent + marker + spaces;
    let mark = match item_src.get(box_at..box_at + 3)? {
        "[ ]" => "x",
        "[x]" | "[X]" => " ",
        _ => return None,
    };
    Some(Edit::replace(base + box_at + 1..base + box_at + 2, mark))
}

/// 上一行还能接着写字：段落、标题、列表项正文、引用和 callout。
/// `- `、`3) `、`- [x] `（可带缩进）是否只是列表标记。
fn is_list_marker(s: &str) -> bool {
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
        assert_eq!(block_type(&us[1]), Some(BlockType::Text));
        assert_eq!(doc.blocks().roots[1].kind, BlockKind::BlockQuote);

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
    fn converting_quoted_text_to_code_preserves_container_content_and_identity() {
        let source = "> first  \n>second {.red custom=keep} ^body\n>\n> sibling\n\nafter\n";
        let expected =
            "> ```\n> first  \n> second\n> ```\n> \n> {.red custom=keep} ^body\n>\n> sibling\n\nafter\n";
        let mut doc = Document::from_text(source);
        let id = units(&doc)[0].id;
        let plan = convert_node(&doc, id, BlockType::Code).unwrap().unwrap();
        assert_eq!(&expected[..plan.focus_offset], "> ```\n> first  \n> second");
        assert_eq!(apply(&mut doc, plan).0, expected);
        let quote = &doc.blocks().roots[0];
        assert_eq!(quote.kind, BlockKind::BlockQuote);
        let code = &quote.children[0];
        assert!(matches!(code.kind, BlockKind::CodeFence { .. }));
        assert_eq!(code.persisted_id.as_deref(), Some("body"));
        assert_eq!(code.attrs.pairs, [("custom".into(), "keep".into())]);
        assert_eq!(code.attrs.classes, ["red"]);
        doc.undo().unwrap();
        assert_eq!(doc.to_string(), source);
        doc.redo().unwrap();
        assert_eq!(doc.to_string(), expected);
    }

    #[test]
    fn converting_list_body_to_code_preserves_the_item() {
        for (source, expected, index, checked) in [
            (
                "12. first\n    second\n13. sibling\n",
                "12. ```\n    first\n    second\n    ```\n13. sibling\n",
                0,
                None,
            ),
            (
                "- first\n\n  second\n  third\n- sibling\n",
                "- first\n\n  ```\n  second\n  third\n  ```\n- sibling\n",
                1,
                None,
            ),
            (
                "- [x] first\n  second\n- sibling\n",
                "- [x] \n\n  ```\n  first\n  second\n  ```\n- sibling\n",
                0,
                Some(true),
            ),
            (
                "- > first\n  > second\n- sibling\n",
                "- > ```\n  > first\n  > second\n  > ```\n- sibling\n",
                0,
                None,
            ),
        ] {
            let mut doc = Document::from_text(source);
            let plan = convert(&doc, &units(&doc), index, BlockType::Code).unwrap();
            assert_eq!(apply(&mut doc, plan).0, expected);
            let list = &doc.blocks().roots[0];
            assert_eq!(list.children.len(), 2);
            let item = &list.children[0];
            assert_eq!(item.kind, BlockKind::ListItem { task: checked });
            assert_eq!(item.children.len(), index + 1);
            let body = &item.children[index];
            let code = if body.kind == BlockKind::BlockQuote { &body.children[0] } else { body };
            assert!(matches!(code.kind, BlockKind::CodeFence { .. }));
            doc.undo().unwrap();
            assert_eq!(doc.to_string(), source);
        }
    }

    #[test]
    fn converting_item_as_a_whole_keeps_its_outer_quote() {
        let source = "> - first\n>   second\n\nafter\n";
        let mut doc = Document::from_text(source);
        let item = doc.blocks().roots[0].children[0].children[0].id;
        let plan = convert_node(&doc, item, BlockType::Code).unwrap().unwrap();
        assert_eq!(apply(&mut doc, plan).0, "> ```\n> first\n> second\n> ```\n\nafter\n");
        assert!(matches!(doc.blocks().roots[0].children[0].kind, BlockKind::CodeFence { .. }));
    }

    #[test]
    fn container_handle_conversion_keeps_nested_lists_and_item_metadata() {
        let mut doc =
            Document::from_text("- first\n\n  continuation {.red}\n\n  - nested ^nested\n- second\n");
        let list = doc.blocks().roots[0].id;
        let plan = convert_node(&doc, list, BlockType::Numbered).unwrap().unwrap();
        doc.edit(plan.edit).unwrap();
        doc.reparse();
        assert!(
            doc.to_string()
                .starts_with("1. first\n\n   continuation {.red}\n\n   - nested ^nested\n2. second"),
            "{doc}"
        );
        assert_eq!(doc.blocks().roots[0].children.len(), 2);
        let item = &doc.blocks().roots[0].children[0];
        assert!(convert_node(&doc, item.id, BlockType::Heading(2)).unwrap_err().contains("多个块"));
        let child = item.children[0].id;
        let plan = convert_node(&doc, child, BlockType::Heading(2)).unwrap().unwrap();
        doc.edit(plan.edit).unwrap();
        doc.reparse();
        assert!(doc.to_string().starts_with("1. ## first"), "{doc}");
        assert_eq!(doc.blocks().roots[0].children[0].children.len(), 3);
    }

    #[test]
    fn unit_conversion_preserves_complex_items_and_scopes_continuations() {
        let source = "- first ^first\n\n  continuation {.red}\n\n  - nested ^nested\n- second\n";
        let mut doc = Document::from_text(source);
        let us = units(&doc);
        assert!(!conversion_available(&doc, &us[0], BlockType::Heading(2)));
        assert!(convert(&doc, &us, 0, BlockType::Heading(2)).is_none());
        let plan = convert(&doc, &us, 0, BlockType::Task).unwrap();
        apply(&mut doc, plan);
        assert_eq!(doc.to_string(), source.replacen("- first", "- [ ] first", 1));
        doc.undo().unwrap();
        doc.reparse();
        let us = units(&doc);
        let plan = convert(&doc, &us, 1, BlockType::Heading(2)).unwrap();
        apply(&mut doc, plan);
        assert_eq!(doc.to_string(), source.replace("  continuation", "  ## continuation"));
        doc.undo().unwrap();
        assert_eq!(doc.to_string(), source);
    }

    #[test]
    fn moving_a_container_preserves_all_ids_and_rejects_its_descendants() {
        let mut doc = Document::from_text(":::highlight\nchild ^kid\n:::\n\n^box\n\nafter\n");
        let source = doc.blocks().roots[0].id;
        let child = doc.blocks().roots[0].children[0].id;
        assert!(move_node(&doc, source, child, false, false).is_none());
        let target = doc.blocks().roots.last().unwrap().id;
        let plan = move_node(&doc, source, target, true, false).unwrap();
        doc.edit(plan.edit).unwrap();
        doc.reparse();
        assert!(doc.to_string().find("after").unwrap() < doc.to_string().find(":::highlight").unwrap());
        assert!(aero_core::block_refs::resolve(&doc, "kid").is_ok());
        assert!(aero_core::block_refs::resolve(&doc, "box").is_ok());
    }

    #[test]
    fn moving_highlight_into_equal_length_fence_extends_the_outer_fence() {
        let source = ":::highlight\nouter ^outer\n:::\n\n:::highlight\ninner ^inner\n:::\n";
        let mut doc = Document::from_text(source);
        let outer = doc.blocks().roots[0].id;
        let inner = doc.blocks().roots[1].id;
        let plan = move_node(&doc, inner, outer, false, true).expect("equal fences can nest after extension");
        doc.edit(plan.edit).unwrap();
        doc.reparse();
        assert_eq!(doc.blocks().roots.len(), 1, "{}", doc);
        assert!(doc.to_string().starts_with("::::highlight\n"), "{}", doc);
        assert_eq!(doc.blocks().roots[0].children.len(), 2, "{}", doc);
        assert!(
            doc.blocks().roots[0]
                .children
                .iter()
                .any(|block| matches!(block.kind, BlockKind::Directive { .. })),
            "{}",
            doc
        );
        assert!(aero_core::block_refs::resolve(&doc, "outer").is_ok());
        assert!(aero_core::block_refs::resolve(&doc, "inner").is_ok());
        doc.undo().unwrap();
        assert_eq!(doc.to_string(), source);
    }

    #[test]
    fn moving_highlight_between_list_items_preserves_nested_structure() {
        let source = "- :::highlight\n  outer ^outer\n  :::\n\n- :::highlight\n  inner ^inner\n  :::\n";
        let mut doc = Document::from_text(source);
        let directives: Vec<_> = doc
            .blocks()
            .iter()
            .filter(|(_, block)| matches!(block.kind, BlockKind::Directive { .. }))
            .map(|(_, block)| block.id)
            .collect();
        let (outer, inner) = (directives[0], directives[1]);
        let plan = move_node(&doc, inner, outer, false, true).expect("list containers can nest");
        doc.edit(plan.edit).unwrap();
        doc.reparse();
        let inner_block = doc.blocks().find(inner).expect("moved container keeps its ID");
        assert!(
            aero_core::containers::ancestors(&doc, inner_block.id).iter().any(|block| block.id == outer),
            "{}",
            doc
        );
        assert!(aero_core::block_refs::resolve(&doc, "outer").is_ok());
        assert!(aero_core::block_refs::resolve(&doc, "inner").is_ok());
        doc.undo().unwrap();
        assert_eq!(doc.to_string(), source);
    }

    #[test]
    fn moving_identified_highlight_preserves_container_and_body_identities() {
        let source = ":::highlight\nouter ^outer\n:::\n\n^outer-box\n\n:::highlight\ninner ^inner\n:::\n\n^inner-box\n";
        let mut doc = Document::from_text(source);
        let identified = |doc: &Document, id: &str| {
            doc.blocks()
                .iter()
                .find(|(_, block)| block.persisted_id.as_deref() == Some(id))
                .map(|(_, block)| block.id)
                .unwrap()
        };
        let outer = identified(&doc, "outer-box");
        let inner = identified(&doc, "inner-box");
        let plan = move_node(&doc, inner, outer, false, true).expect("identified containers can nest");
        doc.edit(plan.edit).unwrap();
        doc.reparse();
        let inner = identified(&doc, "inner-box");
        let outer = identified(&doc, "outer-box");
        assert!(aero_core::containers::ancestors(&doc, inner).iter().any(|block| block.id == outer));
        assert!(aero_core::block_refs::resolve(&doc, "inner").is_ok());
        assert!(aero_core::block_refs::resolve(&doc, "outer").is_ok());
        doc.undo().unwrap();
        assert_eq!(doc.to_string(), source);
    }

    #[test]
    fn list_level_changes_move_the_whole_item_and_preserve_identity() {
        let source = "- first\n- second ^second\n\n  continuation\n\n  - nested ^nested\n- third\n";
        let mut doc = Document::from_text(source);
        let second = doc.blocks().roots[0].children[1].id;
        let plan =
            change_list_level(&doc, second, ListLevelDirection::Indent).expect("second item can indent");
        doc.edit(plan.edit).unwrap();
        doc.reparse();
        let body = doc
            .blocks()
            .iter()
            .find(|(_, block)| block.persisted_id.as_deref() == Some("second"))
            .unwrap()
            .1
            .id;
        let first = doc.blocks().roots[0].children[0].id;
        assert!(aero_core::containers::ancestors(&doc, body).iter().any(|block| block.id == first));
        assert!(aero_core::block_refs::resolve(&doc, "second").is_ok());
        assert!(aero_core::block_refs::resolve(&doc, "nested").is_ok());
        assert!(!doc.to_string().contains("indent="));
        doc.undo().unwrap();
        assert_eq!(doc.to_string(), source);

        let mut doc = Document::from_text(
            "- first\n  - second ^second\n\n    continuation\n\n    - nested ^nested\n- third\n",
        );
        let body = doc
            .blocks()
            .iter()
            .find(|(_, block)| block.persisted_id.as_deref() == Some("second"))
            .unwrap()
            .1
            .id;
        let second = aero_core::containers::ancestors(&doc, body)
            .into_iter()
            .rev()
            .find(|block| matches!(block.kind, BlockKind::ListItem { .. }))
            .unwrap()
            .id;
        let plan =
            change_list_level(&doc, second, ListLevelDirection::Outdent).expect("nested item can outdent");
        doc.edit(plan.edit).unwrap();
        doc.reparse();
        let body = doc
            .blocks()
            .iter()
            .find(|(_, block)| block.persisted_id.as_deref() == Some("second"))
            .unwrap()
            .1
            .id;
        assert_eq!(
            aero_core::containers::ancestors(&doc, body)
                .iter()
                .filter(|block| matches!(block.kind, BlockKind::ListItem { .. }))
                .count(),
            1
        );
        assert!(aero_core::block_refs::resolve(&doc, "second").is_ok());
        assert!(aero_core::block_refs::resolve(&doc, "nested").is_ok());
    }

    #[test]
    fn outdenting_a_middle_item_adopts_following_siblings_and_preserves_order() {
        let source = "- parent\n  - first ^first\n  - second ^second\n  - third ^third\n- next ^next\n";
        let mut doc = Document::from_text(source);
        let body = doc
            .blocks()
            .iter()
            .find(|(_, block)| block.persisted_id.as_deref() == Some("second"))
            .unwrap()
            .1
            .id;
        let second = aero_core::containers::ancestors(&doc, body)
            .into_iter()
            .rev()
            .find(|block| matches!(block.kind, BlockKind::ListItem { .. }))
            .unwrap()
            .id;
        let plan =
            change_list_level(&doc, second, ListLevelDirection::Outdent).expect("middle item can outdent");
        doc.edit(plan.edit).unwrap();
        doc.reparse();
        let text = doc.to_string();
        let order = ["first", "second", "third", "next"].map(|id| text.find(id).unwrap());
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
        let second_body = doc
            .blocks()
            .iter()
            .find(|(_, block)| block.persisted_id.as_deref() == Some("second"))
            .unwrap()
            .1
            .id;
        let third_body = doc
            .blocks()
            .iter()
            .find(|(_, block)| block.persisted_id.as_deref() == Some("third"))
            .unwrap()
            .1
            .id;
        let second_item = aero_core::containers::ancestors(&doc, second_body)
            .into_iter()
            .rev()
            .find(|block| matches!(block.kind, BlockKind::ListItem { .. }))
            .unwrap()
            .id;
        assert!(
            aero_core::containers::ancestors(&doc, third_body).iter().any(|block| block.id == second_item)
        );
    }

    #[test]
    fn outdenting_loose_quoted_items_does_not_duplicate_following_or_prefixes() {
        let source = "> - parent\n>   - first\n>   - second\n>\n>   - third\n> - next\n";
        let mut doc = Document::from_text(source);
        let second = doc
            .blocks()
            .iter()
            .filter(|(_, block)| matches!(block.kind, BlockKind::ListItem { .. }))
            .nth(1)
            .unwrap()
            .1
            .id;
        let plan = change_list_level(&doc, second, ListLevelDirection::Outdent)
            .expect("quoted loose item can outdent");
        doc.edit(plan.edit).unwrap();
        doc.reparse();
        let text = doc.to_string();
        assert_eq!(text.matches("third").count(), 1, "{text}");
        assert_eq!(text.matches("second").count(), 1, "{text}");
        assert!(!text.contains("> >"), "{text}");
        assert!(!text.contains(">>"), "{text}");
        let order = ["first", "second", "third", "next"].map(|id| text.find(id).unwrap());
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
    }

    #[test]
    fn list_level_changes_reject_missing_structural_targets() {
        let doc = Document::from_text("- only\n");
        let item = doc.blocks().roots[0].children[0].id;
        assert!(change_list_level(&doc, item, ListLevelDirection::Indent).is_none());
        assert!(change_list_level(&doc, item, ListLevelDirection::Outdent).is_none());
    }

    #[test]
    fn moving_list_body_keeps_other_children_in_their_original_item() {
        let source =
            "before\n\n- first ^first\n\n  continuation ^cont\n\n  - nested ^nested\n- sibling\n\nend\n";
        for (index, identity) in [(0, "first"), (1, "cont")] {
            let mut doc = Document::from_text(source);
            let target = doc.blocks().roots[0].id;
            let id = doc.blocks().roots[1].children[0].children[index].id;
            let plan = move_node(&doc, id, target, false, false).unwrap();
            doc.edit(plan.edit).unwrap();
            doc.reparse();
            assert_eq!(doc.blocks().roots[0].persisted_id.as_deref(), Some(identity));
            let list = &doc.blocks().roots[2];
            assert_eq!(list.children.len(), 2);
            let item = &list.children[0];
            assert_eq!(item.children.len(), 2);
            assert!(matches!(item.children[1].kind, BlockKind::List { .. }));
            assert_eq!(item.children[1].children[0].children[0].persisted_id.as_deref(), Some("nested"));
            doc.undo().unwrap();
            assert_eq!(doc.to_string(), source);
        }
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
    fn split_preserves_inline_styles_block_identity_and_undo() {
        let id = aero_core::identity::new_block_id();
        let alias = aero_core::identity::new_block_id();
        let attrs = format!("{{.red align=center aero-aliases=\"{alias}\"}} ^{id}");
        for heading in ["", "## "] {
            let source = format!("{heading}**ab cd** {attrs}\n");
            for (cursor, expected) in [
                (heading.len() + 4, format!("{heading}**ab** {attrs}\n\n**cd** {{.red align=center}}\n")),
                (heading.len() + 2, format!("\n\n{source}")),
                (heading.len() + 7, format!("{source}\n\n")),
            ] {
                let mut doc = Document::from_text(&source);
                let us = units(&doc);
                let plan = split(&doc, Some(&us[0]), &us[0].range, cursor);
                apply(&mut doc, plan);
                assert_eq!(doc.to_string(), expected);
                let original = doc.blocks().find(aero_core::block_refs::resolve(&doc, &id).unwrap()).unwrap();
                assert!(aero_core::block_refs::aliases(original).any(|a| a == alias));
                doc.undo().unwrap();
                assert_eq!(doc.to_string(), source);
                doc.redo().unwrap();
                assert_eq!(doc.to_string(), expected);
            }
        }
    }

    #[test]
    fn split_nested_list_keeps_depth_children_and_quote_boundary() {
        for (source, needle, expected) in [
            (
                "- parent\n  - **ab cd**\n    - child ^kid\n",
                " cd",
                "- parent\n  - **ab**\n    - child ^kid\n  - **cd**\n",
            ),
            ("> - **ab cd**\n>   - child ^kid\n", " cd", "> - **ab**\n>   - child ^kid\n> - **cd**\n"),
            ("- parent\n\n  > **ab cd**\n", " cd", "- parent\n\n  > **ab**\n  > \n  > **cd**\n"),
        ] {
            let mut doc = Document::from_text(source);
            let us = units(&doc);
            let at = source.find(needle).unwrap();
            let unit = us.iter().find(|u| u.range.contains(&at)).unwrap();
            let plan = split(&doc, Some(unit), &unit.range, at - unit.range.start);
            apply(&mut doc, plan);
            assert_eq!(doc.to_string(), expected);
            doc.undo().unwrap();
            assert_eq!(doc.to_string(), source);
        }
        let source = "> - {aero-owner=item .red} ^item\n> \n>   **body** ^body\n>   - child ^kid\n";
        let mut doc = Document::from_text(source);
        let us = units(&doc);
        let first = &us[0];
        let plan = split(&doc, Some(first), &first.range, 2);
        apply(&mut doc, plan);
        assert_eq!(doc.to_string(), format!("> - \n{source}"));
        let item = doc.blocks().find(aero_core::block_refs::resolve(&doc, "item").unwrap()).unwrap();
        let body = aero_core::block_refs::resolve(&doc, "body").unwrap();
        assert!(item.children.iter().any(|b| b.id == body));
        assert!(aero_core::block_refs::resolve(&doc, "kid").is_ok());
    }

    #[test]
    fn different_block_styles_only_move_the_caret() {
        let source = "**one** {.red}\n\n*two* {.blue}\n";
        let doc = Document::from_text(source);
        let us = units(&doc);
        assert_eq!(
            backspace_at_start(&doc, &us, 1),
            Backspace::StyleMismatch { unit: 0, cursor: us[0].range.len() }
        );
        assert_eq!(doc.to_string(), source);
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
    fn enter_on_trailing_empty_container_slot_exits_one_level() {
        let mut doc = Document::from_text(":::highlight\n:::\n\nafter\n");
        let us = units(&doc);
        let plan = split(&doc, Some(&us[0]), &us[0].range, 0);
        let (text, _) = apply(&mut doc, plan);
        assert_eq!(text, ":::highlight\n:::\n\n\n\nafter\n");
        assert_eq!(doc.blocks().roots.len(), 2);
        assert_eq!(doc.blocks().roots[0].children.len(), 0);
    }

    #[test]
    fn enter_on_internal_empty_container_slot_stays_inside() {
        let mut doc = Document::from_text(":::highlight\nfirst\n\nsecond\n:::\n\nafter\n");
        let us = units(&doc);
        let first = split(&doc, Some(&us[0]), &us[0].range, us[0].range.len());
        let at = first.focus_offset;
        doc.edit(first.edit).unwrap();
        doc.reparse();
        let before = doc.to_string();
        let container = &doc.blocks().roots[0];
        let empty = Unit { id: container.id, kind: BlockKind::Paragraph, range: at..at, list: None };
        let plan = split(&doc, Some(&empty), &empty.range, 0);
        doc.edit(plan.edit).unwrap();
        doc.reparse();
        assert_eq!(doc.to_string(), format!("{}\n\n{}", &before[..at], &before[at..]));
        assert_eq!(doc.blocks().roots[0].children.len(), 2);
        assert_eq!(doc.blocks().roots[1].children.len(), 0);
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
    fn nonempty_first_container_child_backspace_stays_at_boundary() {
        let doc = Document::from_text(":::highlight\nfirst\n\nsecond\n:::\n");
        let us = units(&doc);
        assert_eq!(backspace_at_start(&doc, &us, 0), Backspace::MoveTo { unit: 0, cursor: 0 });
        assert_eq!(doc.to_string(), ":::highlight\nfirst\n\nsecond\n:::\n");

        let doc = Document::from_text(":::highlight\n:::\n");
        let us = units(&doc);
        let container = doc.blocks().roots[0].id;
        assert_eq!(backspace_at_start(&doc, &us, 0), Backspace::SelectContainer(container));
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
    fn backspace_preserves_container_boundaries_and_joins_direct_siblings() {
        let cases = [
            ("- 除非你接受相关风险，\n\n否则不要。\n", "- 除非你接受相关风险，否则不要。\n"),
            ("- [ ] 先做\n\n再做\n", "- [ ] 先做再做\n"),
            ("1. 第一\n\n接着\n", "1. 第一接着\n"),
            ("- 甲\n\n  乙\n", "- 甲乙\n"),
            ("# 标题\n\n正文\n", "# 标题正文\n"),
            ("> 引用\n\n下一段\n", "> 引用下一段\n"),
            ("> 一\n> 二\n\n三段\n", "> 一\n> 二三段\n"),
            ("> [!note] 提示\n> 正文\n\n接着\n", "> [!note] 提示\n> 正文接着\n"),
        ];
        for (src, expected) in cases {
            let mut doc = Document::from_text(src);
            let us = units(&doc);
            match backspace_at_start(&doc, &us, us.len() - 1) {
                Backspace::Edit(plan) => {
                    assert!(
                        src.starts_with('#') || src == "- 甲\n\n  乙\n",
                        "unexpected boundary join {src:?}"
                    );
                    let (text, _) = apply(&mut doc, plan);
                    assert_eq!(text, expected, "{src:?}");
                }
                Backspace::MoveTo { .. } | Backspace::SelectContainer(_) => {
                    assert!(!src.starts_with('#'), "direct siblings should join");
                    assert_eq!(doc.to_string(), src);
                }
                other => panic!("unexpected {other:?} for {src:?}"),
            }
        }
    }

    #[test]
    fn backspace_after_code_or_math_only_moves() {
        for src in ["```\nx\n```\n\npara\n", "$$\nE\n$$\n\npara\n"] {
            let doc = Document::from_text(src);
            let us = units(&doc);
            assert!(us.len() >= 2, "{src:?} -> {}", us.len());
            assert_eq!(
                backspace_at_start(&doc, &us, us.len() - 1),
                Backspace::MoveTo { unit: us.len() - 2, cursor: us[us.len() - 2].range.len() },
                "{src:?}"
            );
        }
        let doc = Document::from_text("```\nx\n```\n");
        let us = units(&doc);
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
    fn deleting_nodes_removes_lines_and_surrounding_blank_lines() {
        let delete = |doc: &mut Document, ids: &[BlockId]| {
            let edits = block_ops::deletions(doc, &doc.to_string(), ids);
            doc.edit_group(edits).unwrap();
            doc.reparse();
        };
        let mut doc = Document::from_text("aaa\n\nbbb\n\nccc\n");
        let middle = doc.blocks().roots[1].id;
        delete(&mut doc, &[middle]);
        assert_eq!(doc.to_string(), "aaa\n\nccc\n");
        let last = doc.blocks().roots[1].id;
        delete(&mut doc, &[last]);
        assert_eq!(doc.to_string(), "aaa\n");

        let mut doc = Document::from_text("aaa\n\nbbb\n");
        let first = doc.blocks().roots[0].id;
        delete(&mut doc, &[first]);
        assert_eq!(doc.to_string(), "bbb\n");
        let mut doc = Document::from_text("aaa\n\nbbb\n");
        let ids: Vec<_> = doc.blocks().roots.iter().map(|b| b.id).collect();
        delete(&mut doc, &ids);
        assert_eq!(doc.to_string(), "");

        let mut doc = Document::from_text("- a\n- b\n- c\n");
        let item = doc.blocks().roots[0].children[1].id;
        delete(&mut doc, &[item]);
        assert_eq!(doc.to_string(), "- a\n- c\n");
        let mut doc = Document::from_text("# T\npara\n");
        let heading = doc.blocks().roots[0].id;
        delete(&mut doc, &[heading]);
        assert_eq!(doc.to_string(), "para\n");
        for before in ["中文。", "图 🎨", "ascii"] {
            let mut doc = Document::from_text(&format!("{before}\n\n![[x.png]]\n"));
            let image = doc.blocks().roots[1].id;
            delete(&mut doc, &[image]);
            assert_eq!(doc.to_string(), format!("{before}\n"), "{before}");
        }
    }

    #[test]
    fn append_paragraph_pads_newlines() {
        assert_eq!(append_paragraph(&Document::from_text("")), (None, 0));
        assert_eq!(append_paragraph(&Document::from_text("a")), (Some(Edit::insert(1, "\n\n")), 3));
        assert_eq!(append_paragraph(&Document::from_text("a\n")), (Some(Edit::insert(2, "\n")), 3));
        assert_eq!(append_paragraph(&Document::from_text("a\n\n")), (None, 3));
    }

    #[test]
    fn move_unit_reorders_paragraphs_and_adjusts_list_indent() {
        let moved = |src: &str, from: usize, before: usize, parent: Option<usize>| {
            let doc = Document::from_text(src);
            let us = units(&doc);
            move_unit(&doc, &us, from, DropPlace { before, parent }).map(|plan| {
                let mut doc = doc;
                doc.edit(plan.edit).unwrap();
                doc.reparse();
                doc.to_string()
            })
        };
        assert_eq!(moved("aaa\n\nbbb\n\nccc\n", 2, 0, None).as_deref(), Some("ccc\n\naaa\n\nbbb\n"));
        assert_eq!(moved("aaa\n\nbbb\n\nccc\n", 0, 2, None).as_deref(), Some("bbb\n\naaa\n\nccc\n"));
        assert!(moved("aaa\n\nbbb\n", 0, 0, None).is_none());
        assert_eq!(moved("- a\n- b\n- c\n", 2, 0, None).as_deref(), Some("- c\n- a\n- b\n"));
        assert_eq!(moved("- a\n\npara\n", 1, 1, Some(0)).as_deref(), Some("- a\n  - para\n"));
        assert_eq!(moved("- a\n  - child\n\npara\n", 1, 2, None).as_deref(), Some("- a\n\nchild\n\npara\n"));
        assert_eq!(moved("- a\n- b\n\npara\n", 2, 0, Some(0)).as_deref(), Some("- a\n  - para\n- b\n"));
    }

    #[test]
    fn moving_a_paragraph_preserves_code_and_unrelated_whitespace_byte_for_byte() {
        let fence = "```text\r\n保留\r\n\r\n\r\n  \t\r\n\r\n末尾  \r\n```";
        let moved = "拖动  \r\n下一行";
        let prefix = format!("\u{feff}开头\r\n\r\n{fence}\r\n\r\n\r\n后面\r\n\r\n");
        let tail = "最后\r\n";
        let source = format!("{prefix}{moved}\r\n\r\n{tail}");
        let expected = format!("\u{feff}{moved}\r\n\r\n{}{}", &prefix['\u{feff}'.len_utf8()..], tail);
        let mut doc = Document::from_bytes(source.as_bytes()).unwrap();
        let plan = move_unit(&doc, &units(&doc), 3, DropPlace { before: 0, parent: None }).unwrap();
        apply(&mut doc, plan);
        assert_eq!(doc.to_bytes(), expected.as_bytes());
        doc.undo().unwrap();
        assert_eq!(doc.to_bytes(), source.as_bytes());
        doc.redo().unwrap();
        assert_eq!(doc.to_bytes(), expected.as_bytes());
    }
}
