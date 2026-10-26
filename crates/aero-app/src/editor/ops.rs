//! 结构性编辑的"计划"：拆块、合并、跨块移动。纯函数，只看 `Document` 与编辑单位，
//! 不碰 GPUI，因而可以在没有窗口的情况下测试（`editor/mod.rs` 只负责把计划应用到文档并移动焦点）。

use std::ops::Range;

use aero_core::{BlockId, BlockKind, Document, Edit};

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
    if text.is_empty() {
        let container = unit.and_then(|u| doc.blocks().find(u.id)).or_else(|| doc.block_at(range.start));
        if let Some(container) = container.filter(|b| aero_core::containers::is_editable(&b.kind)) {
            let at_end = container
                .children
                .iter()
                .filter(|b| b.kind != BlockKind::Marker)
                .all(|b| doc.range_of(b).end <= doc_cursor);
            let (at, prefix) = if at_end {
                (
                    aero_core::properties::owned_range(doc, container.id).unwrap().end,
                    aero_core::containers::continuation(doc, container.id),
                )
            } else {
                (doc_cursor, aero_core::containers::content_continuation(doc, container.id))
            };
            let separator = format!("\n{prefix}\n{prefix}");
            let focus_offset = at + if at_end { separator.len() } else { 1 + prefix.len() };
            return Plan { focus_offset, edit: Edit::insert(at, separator) };
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

pub fn list_item_for(doc: &Document, id: BlockId) -> Option<BlockId> {
    let block = doc.blocks().find(id)?;
    if matches!(block.kind, BlockKind::ListItem { .. }) {
        return Some(id);
    }
    aero_core::containers::ancestors(doc, id).into_iter().rev()
        .find(|b| matches!(b.kind, BlockKind::ListItem { .. })).map(|b| b.id)
}

pub fn list_indent(doc: &Document, id: BlockId) -> Option<Plan> {
    let item = list_item_for(doc, id)?;
    let path = aero_core::containers::ancestors(doc, item);
    let list = path.last()?;
    let siblings: Vec<_> = list.children.iter().filter(|b| matches!(b.kind, BlockKind::ListItem { .. })).collect();
    let index = siblings.iter().position(|b| b.id == item)?;
    list_focus_plan(doc, move_node(doc, item, siblings.get(index.checked_sub(1)?)?.id, false, true)?)
}

fn list_focus_plan(doc: &Document, mut plan: Plan) -> Option<Plan> {
    let mut probe = doc.clone();
    probe.edit(plan.edit.clone()).ok()?;
    probe.reparse();
    if let Some(unit) = super::units::units(&probe).iter().find(|u| u.range.start >= plan.focus_offset) {
        plan.focus_offset = unit.range.start;
    }
    Some(plan)
}

pub fn list_outdent(doc: &Document, id: BlockId) -> Option<Plan> {
    let item = list_item_for(doc, id)?;
    let path = aero_core::containers::ancestors(doc, item);
    if let Some(parent) = path.iter().rev().nth(1)
        .filter(|b| matches!(b.kind, BlockKind::ListItem { .. }))
    {
        return list_focus_plan(doc, outdent_nested_item(doc, item, parent.id)?);
    }
    None
}

fn outdent_nested_item(doc: &Document, item: BlockId, parent: BlockId) -> Option<Plan> {
    let range = aero_core::properties::owned_range(doc, item)?;
    let piece = aero_core::containers::standalone(doc, range);
    let inserted = Document::from_text(&piece);
    let shapes = tree_shapes(&inserted);
    let expected = changed_tree(doc, &doc.blocks().roots, &[item], parent, TreeInsertion::After, &shapes);
    let at = aero_core::properties::owned_range(doc, parent)?.end;
    let prefix = aero_core::containers::continuation(doc, parent);
    let mut edits = SelectionPlan::nodes(doc, &[item]).deletions(doc);
    if edits.is_empty() || edits.iter().any(|e| e.range.start < at && at < e.range.end) { return None; }
    let shift: isize = edits.iter().filter(|e| e.range.end <= at)
        .map(|e| e.text.len() as isize - e.range.len() as isize).sum();
    let insertion = format!("\n{prefix}\n{prefix}{}", piece.trim_end_matches('\n').replace('\n', &format!("\n{prefix}")));
    let focus_offset = (at as isize + shift) as usize + 2 * (1 + prefix.len());
    edits.push(Edit::insert(at, insertion));
    edits.sort_by_key(|e| std::cmp::Reverse(e.range.start));
    let mut probe = doc.clone();
    probe.edit_group(edits).ok()?;
    probe.reparse();
    if tree_shapes(&probe) != expected { return None; }
    Some(Plan { edit: Edit::replace(0..doc.len(), probe.to_string()), focus_offset })
}

fn exit_list_item(doc: &Document, id: BlockId) -> Option<Plan> {
    let item = doc.blocks().find(id)?;
    let path = aero_core::containers::ancestors(doc, id);
    let list = path.last()?;
    let range = aero_core::properties::owned_range(doc, id)?;
    let children: Vec<_> = item.children.iter().filter(|b| b.kind != BlockKind::Marker).collect();
    let mut attrs = item.attrs.clone();
    aero_core::properties::set(&mut attrs, "aero-owner", None);
    if !attrs.is_empty() || item.persisted_id.is_some() {
        return None;
    }
    let body = children.iter().map(|child| {
        aero_core::containers::standalone(doc, aero_core::properties::owned_range(doc, child.id).unwrap())
    }).collect::<Vec<_>>().join("\n\n");
    let replacement_shapes: Vec<_> = children.iter().map(|child| TreeShape::read(doc, child)).collect();
    let expected = list_exit_shapes(doc, &doc.blocks().roots, list.id, id, &replacement_shapes);
    let prefix = aero_core::containers::continuation(doc, id);
    let before = list.children.iter().filter(|b| b.kind != BlockKind::Marker).next()?.id != id;
    let after = list.children.iter().filter(|b| b.kind != BlockKind::Marker).last()?.id != id;
    let replacement = format!("{}{}{}",
        if before { format!("\n{prefix}") } else { String::new() },
        body.replace('\n', &format!("\n{prefix}")),
        if after { format!("\n{prefix}") } else { String::new() });
    let mut edits = recipient_fence_edits(doc, id, false, &Document::from_text(&body))?;
    let shift: isize = edits.iter().filter(|e| e.range.end <= range.start)
        .map(|e| e.text.len() as isize - e.range.len() as isize).sum();
    let focus_offset = (range.start as isize + shift) as usize + if before { 1 + prefix.len() } else { 0 };
    edits.push(Edit::replace(range, replacement));
    edits.sort_by_key(|e| std::cmp::Reverse(e.range.start));
    let mut probe = doc.clone();
    probe.edit_group(edits.clone()).ok()?;
    probe.reparse();
    if tree_shapes(&probe) != expected { return None; }
    let edit = if edits.len() == 1 { edits.remove(0) } else { Edit::replace(0..doc.len(), probe.to_string()) };
    Some(Plan { edit, focus_offset })
}

fn list_exit_shapes(doc: &Document, blocks: &[aero_core::Block], list: BlockId, item: BlockId, inserted: &[TreeShape]) -> Vec<TreeShape> {
    let mut result = Vec::new();
    for block in blocks.iter().filter(|b| b.kind != BlockKind::Marker) {
        if block.id != list {
            let mut shape = TreeShape::read(doc, block);
            shape.children = list_exit_shapes(doc, &block.children, list, item, inserted);
            result.push(shape);
            continue;
        }
        let children: Vec<_> = block.children.iter().filter(|b| b.kind != BlockKind::Marker).collect();
        let index = children.iter().position(|b| b.id == item).unwrap();
        for (part, slice) in [&children[..index], &children[index + 1..]].into_iter().enumerate() {
            if part == 1 { result.extend_from_slice(inserted); }
            if slice.is_empty() { continue; }
            let mut shape = TreeShape::read(doc, block);
            shape.children = slice.iter().map(|b| TreeShape::read(doc, b)).collect();
            shape.content = shape.children.first().and_then(|b| b.content.clone());
            if part == 0 && index + 1 < children.len() {
                shape.attrs = Default::default();
                shape.identity = None;
            }
            if let BlockKind::List { ordered: true, ref mut start } = shape.kind
                && let Some(marker) = &shape.content
            {
                *start = marker.trim_end_matches(['.', ')']).parse().unwrap_or(*start);
            }
            result.push(shape);
        }
    }
    merge_anonymous_lists(result)
}

#[derive(Debug, PartialEq)]
pub enum Backspace {
    SelectContainer(aero_core::BlockId),
    Edit(Plan),
    Blocked(&'static str),
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
    if let Some(item) = list_item_for(doc, unit.id)
        && doc.blocks().find(item).is_some_and(|item| {
            item.id == unit.id || item.children.iter().find(|b| b.kind != BlockKind::Marker)
                .is_some_and(|b| b.id == unit.id)
        })
    {
        let nested = aero_core::containers::ancestors(doc, item).iter().rev().nth(1)
            .is_some_and(|b| matches!(b.kind, BlockKind::ListItem { .. }));
        let plan = if nested { list_outdent(doc, item) }
            else { exit_list_item(doc, item).and_then(|plan| list_focus_plan(doc, plan)) };
        return plan.map_or(Backspace::Blocked("无法无损退出此列表项，请保留其样式、身份和子块"), Backspace::Edit);
    }
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

#[derive(Clone, Debug)]
pub(crate) struct SelectionPlan {
    targets: Vec<SelectionTarget>,
    deletions: Vec<Edit>,
    replacement_deletions: Vec<Edit>,
}

#[derive(Clone, Debug)]
struct SelectionTarget {
    id: BlockId,
    kind: BlockKind,
    range: Range<usize>,
    parent: Option<BlockId>,
    nested: bool,
    prefix: String,
}

impl SelectionPlan {
    pub(crate) fn nodes(doc: &Document, ids: &[BlockId]) -> Self {
        let ids: std::collections::HashSet<_> = ids.iter().copied().collect();
        let source = doc.to_string();
        let mut targets: Vec<_> = doc
            .blocks()
            .iter()
            .filter_map(|(_, block)| {
                if !ids.contains(&block.id) {
                    return None;
                }
                let ancestors = aero_core::containers::ancestors(doc, block.id);
                if ancestors.iter().any(|b| ids.contains(&b.id)) {
                    return None;
                }
                let mut range = aero_core::properties::owned_range(doc, block.id)?;
                while range.end > range.start && source.as_bytes()[range.end - 1] == b'\n' {
                    range.end -= 1;
                }
                Some(SelectionTarget {
                    id: block.id,
                    kind: block.kind.clone(),
                    range,
                    parent: ancestors.last().map(|b| b.id),
                    nested: ancestors.iter().any(|b| !matches!(b.kind, BlockKind::List { .. })),
                    prefix: aero_core::containers::opening_continuation(doc, block.id),
                })
            })
            .collect();
        targets.sort_by_key(|t| t.range.start);
        let deletions = Self::plan_deletions(doc, &targets);
        let replacement_deletions = Self::plan_deletions(doc, targets.get(1..).unwrap_or_default());
        Self { targets, deletions, replacement_deletions }
    }

    pub(crate) fn units(doc: &Document, units: &[Unit], first: usize, last: usize) -> Self {
        let ids: Vec<_> = units[first..=last]
            .iter()
            .map(|unit| {
                // Keyboard selection keeps first-item semantics without promoting continuation leaves.
                aero_core::containers::ancestors(doc, unit.id)
                    .last()
                    .filter(|item| {
                        matches!(item.kind, BlockKind::ListItem { .. })
                            && item
                                .children
                                .iter()
                                .find(|b| b.kind != BlockKind::Marker)
                                .is_some_and(|b| b.id == unit.id)
                    })
                    .map_or(unit.id, |item| item.id)
            })
            .collect();
        Self::nodes(doc, &ids)
    }

    pub(crate) fn source(&self, doc: &Document) -> String {
        let source = doc.to_string();
        let mut result = String::new();
        for (i, target) in self.targets.iter().enumerate() {
            if i > 0 {
                let previous = &self.targets[i - 1];
                let gap = &source[previous.range.end..target.range.start];
                if previous.parent == target.parent && !gap.is_empty() && gap.chars().all(char::is_whitespace)
                {
                    result.push_str(gap.trim_end_matches([' ', '\t']));
                } else {
                    result.push_str("\n\n");
                }
            }
            result.push_str(&aero_core::containers::standalone(doc, target.range.clone()));
        }
        result
    }

    pub(crate) fn start(&self) -> Option<usize> {
        self.targets.first().map(|t| t.range.start)
    }

    pub(crate) fn deletions(&self, _doc: &Document) -> Vec<Edit> {
        self.deletions.clone()
    }

    pub(crate) fn replace(&self, doc: &Document, source: &str) -> Result<Document, String> {
        let first = self.targets.first().ok_or("选中的块已不存在")?;
        let original = doc.to_string();
        // Workspace reparsing preserves snapshot offsets, not session-local BlockIds.
        let ids = self
            .targets
            .iter()
            .map(|target| {
                doc.blocks()
                    .iter()
                    .find_map(|(_, block)| {
                        if block.kind != target.kind {
                            return None;
                        }
                        let mut range = aero_core::properties::owned_range(doc, block.id)?;
                        while range.end > range.start && original.as_bytes()[range.end - 1] == b'\n' {
                            range.end -= 1;
                        }
                        (range == target.range).then_some(block.id)
                    })
                    .ok_or("选中的块已变化")
            })
            .collect::<Result<Vec<_>, _>>()?;
        let inserted = Document::from_text(source);
        let expected = changed_tree(
            doc,
            &doc.blocks().roots,
            &ids,
            ids[0],
            TreeInsertion::Before,
            &tree_shapes(&inserted),
        );
        let mut edits = self.replacement_deletions.clone();
        edits.extend(recipient_fence_edits(doc, ids[0], false, &inserted).ok_or("目标容器围栏不完整")?);
        edits.push(Edit::replace(
            first.range.clone(),
            source.trim_end_matches('\n').replace('\n', &format!("\n{}", first.prefix)),
        ));
        edits.sort_by_key(|e| std::cmp::Reverse(e.range.start));
        let mut result = doc.clone();
        result.edit_group(edits).map_err(|e| e.to_string())?;
        result.reparse();
        if tree_shapes(&result) != expected {
            return Err("粘贴会改变未选中块的层级或内容".into());
        }
        Ok(result)
    }

    fn plan_deletions(doc: &Document, targets: &[SelectionTarget]) -> Vec<Edit> {
        let source = doc.to_string();
        let mut edits: Vec<Edit> = Vec::new();
        let mut preserves_list = false;
        for target in targets {
            let mut range = target.range.clone();
            let parent_list = target.parent.and_then(|id| doc.blocks().find(id)).filter(|list| {
                matches!(list.kind, BlockKind::List { .. })
                    && (list.persisted_id.is_some() || !list.attrs.is_empty())
            });
            preserves_list |= parent_list.is_some();
            if let Some(list) = parent_list.filter(|list| {
                list.children.iter().find(|b| b.kind != BlockKind::Marker).is_some_and(|b| b.id == target.id)
                    && list
                        .children
                        .iter()
                        .filter(|b| b.kind != BlockKind::Marker)
                        .all(|b| targets.iter().any(|t| t.id == b.id))
            }) {
                // A legal empty item keeps unselected list metadata off the preceding block.
                let marker = source[doc.range_of(list)].split_whitespace().next().unwrap_or("-");
                edits.push(Edit::replace(range, format!("{marker} ")));
                continue;
            }
            if let Some(item) = target
                .parent
                .and_then(|id| doc.blocks().find(id))
                .filter(|b| matches!(b.kind, BlockKind::ListItem { .. }))
                && item
                    .children
                    .iter()
                    .find(|b| b.kind != BlockKind::Marker)
                    .is_some_and(|b| b.id == target.id)
                && let Some(next) = item
                    .children
                    .iter()
                    .find(|b| b.kind != BlockKind::Marker && !targets.iter().any(|t| t.id == b.id))
            {
                let end = line_start(&source, doc.range_of(next).start);
                let prefix = aero_core::containers::continuation(doc, target.id);
                if end > range.end
                    && source[range.end..end]
                        .lines()
                        .all(|line| line.trim().is_empty() || line.trim() == prefix.trim())
                {
                    // An empty item cannot retain children across a blank separator.
                    range.end = end - 1;
                }
            }
            if !target.nested && parent_list.is_none() {
                while range.end < source.len() && source.as_bytes()[range.end] == b'\n' {
                    range.end += 1;
                }
                if range.end == source.len() {
                    while range.start >= 2 && source[..range.start].ends_with("\n\n") {
                        range.start -= 1;
                    }
                }
            }
            if let Some(previous) =
                edits.last_mut().filter(|e| e.text.is_empty() && e.range.end >= range.start)
            {
                previous.range.end = previous.range.end.max(range.end);
            } else {
                edits.push(Edit::delete(range));
            }
        }
        edits.reverse();
        if preserves_list {
            let removed: Vec<_> = targets.iter().map(|t| t.id).collect();
            let expected =
                changed_tree(doc, &doc.blocks().roots, &removed, removed[0], TreeInsertion::Before, &[]);
            let mut probe = doc.clone();
            if probe.edit_group(edits.clone()).is_err() {
                return Vec::new();
            }
            probe.reparse();
            if tree_shapes(&probe) != expected {
                return Vec::new();
            }
        }
        edits
    }
}

pub fn units_span(doc: &Document, units: &[Unit], first: usize, last: usize) -> Range<usize> {
    let plan = SelectionPlan::units(doc, units, first, last);
    let text = doc.to_string();
    let start = plan.start().unwrap_or(doc.len());
    let end = plan.targets.last().map_or(start, |t| t.range.end);
    line_start(&text, start)..end
}

#[cfg(test)]
pub fn delete_units(doc: &Document, units: &[Unit], first: usize, last: usize) -> Vec<Edit> {
    SelectionPlan::units(doc, units, first, last).deletions(doc)
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
    let mut cut_end = span.end;
    while cut_end < text.len() && text.as_bytes()[cut_end] == b'\n' {
        cut_end += 1;
    }
    let mut cut_start = span.start;
    if cut_end == text.len() {
        while cut_start >= 2 && text[..cut_start].ends_with("\n\n") {
            cut_start -= 1;
        }
    }
    let raw = text[span].to_string();
    let mut piece = adapt_moved_block(&raw, &units[from], units, place, &text);
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
    if place.parent.is_some()
        && !piece.starts_with('\n')
        && insert_at > 0
        && !text[..insert_at].ends_with('\n')
    {
        piece.insert(0, '\n');
    }
    if place.parent.is_none() && insert_at > cut_start && insert_at < cut_end {
        return None;
    }
    let assembled = assemble_move(&text, cut_start..cut_end, insert_at, &piece);
    if assembled == text {
        return None;
    }
    let focus = assembled.find(piece.trim_end()).unwrap_or(0);
    Some(Plan { edit: Edit::replace(0..text.len(), assembled), focus_offset: focus })
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct TreeShape {
    kind: BlockKind,
    attrs: aero_core::Attrs,
    identity: Option<String>,
    content: Option<String>,
    children: Vec<TreeShape>,
}

impl TreeShape {
    fn read(doc: &Document, block: &aero_core::Block) -> Self {
        Self {
            kind: block.kind.clone(),
            attrs: block.attrs.clone(),
            identity: block.persisted_id.as_deref().map(str::to_owned),
            content: if matches!(block.kind, BlockKind::List { .. } | BlockKind::ListItem { .. }) {
                aero_core::containers::local_source(doc, block.id)
                    .and_then(|source| source.split_whitespace().next().map(str::to_owned))
            } else if !block.kind.is_container() {
                aero_core::containers::local_source(doc, block.id)
                    .map(|source| source.trim_end_matches('\n').to_owned())
            } else {
                None
            },
            children: block
                .children
                .iter()
                .filter(|b| b.kind != BlockKind::Marker)
                .map(|b| Self::read(doc, b))
                .collect(),
        }
    }
}

fn tree_shapes(doc: &Document) -> Vec<TreeShape> {
    doc.blocks()
        .roots
        .iter()
        .filter(|b| b.kind != BlockKind::Marker)
        .map(|b| TreeShape::read(doc, b))
        .collect()
}

#[derive(Clone, Copy)]
enum TreeInsertion {
    Before,
    After,
    Into,
}

fn changed_tree(
    doc: &Document,
    blocks: &[aero_core::Block],
    removed: &[BlockId],
    target: BlockId,
    insertion: TreeInsertion,
    inserted: &[TreeShape],
) -> Vec<TreeShape> {
    let mut result = Vec::new();
    for block in blocks.iter().filter(|b| b.kind != BlockKind::Marker) {
        let here = block.id == target;
        let in_list = aero_core::containers::ancestors(doc, block.id)
            .last()
            .is_some_and(|b| matches!(b.kind, BlockKind::List { .. }));
        let siblings = || {
            inserted.iter().flat_map(|b| {
                if in_list && matches!(b.kind, BlockKind::List { .. }) {
                    b.children.clone()
                } else {
                    vec![b.clone()]
                }
            })
        };
        if here && matches!(insertion, TreeInsertion::Before) {
            result.extend(siblings());
        }
        if !removed.contains(&block.id) {
            let mut shape = TreeShape::read(doc, block);
            shape.children = changed_tree(doc, &block.children, removed, target, insertion, inserted);
            if here && matches!(insertion, TreeInsertion::Into) {
                shape.children.extend_from_slice(inserted);
                shape.children = merge_anonymous_lists(shape.children);
            }
            if let BlockKind::List { ordered, ref mut start } = shape.kind {
                if shape.children.is_empty() && (shape.identity.is_some() || !shape.attrs.is_empty()) {
                    shape.children.push(TreeShape {
                        kind: BlockKind::ListItem { task: None },
                        attrs: aero_core::Attrs::default(),
                        identity: None,
                        content: shape.content.clone(),
                        children: Vec::new(),
                    });
                }
                shape.content = shape.children.first().and_then(|child| child.content.clone());
                if ordered && let Some(marker) = &shape.content {
                    *start = marker.trim_end_matches(['.', ')']).parse().unwrap_or(*start);
                }
            }
            if !matches!(shape.kind, BlockKind::List { .. })
                || !shape.children.is_empty()
                || shape.identity.is_some()
                || !shape.attrs.is_empty()
            {
                result.push(shape);
            }
        }
        if here && matches!(insertion, TreeInsertion::After) {
            result.extend(siblings());
        }
    }
    merge_anonymous_lists(result)
}

fn merge_anonymous_lists(shapes: Vec<TreeShape>) -> Vec<TreeShape> {
    let mut result: Vec<TreeShape> = Vec::new();
    for shape in shapes {
        if let Some(previous) = result.last_mut().filter(|previous| {
            matches!(shape.kind, BlockKind::List { .. })
                && previous.kind == shape.kind
                && previous.content == shape.content
                && previous.identity.is_none()
                && shape.identity.is_none()
                && previous.attrs.is_empty()
                && shape.attrs.is_empty()
        }) {
            previous.children.extend(shape.children);
        } else {
            result.push(shape);
        }
    }
    result
}

fn recipient_fence_edits(
    doc: &Document,
    target: BlockId,
    into: bool,
    inserted: &Document,
) -> Option<Vec<Edit>> {
    let original = doc.to_string();
    let mut edits = Vec::new();
    // Comrak checks pure colon closers before visiting literal code children.
    let mut required = inserted.to_string().lines().map(directive_closing_width).max().unwrap_or(0);
    for (_, block) in inserted.blocks().iter().filter(|(_, b)| matches!(b.kind, BlockKind::Directive { .. }))
    {
        let local = aero_core::containers::local_source(inserted, block.id)?;
        let opening = local.bytes().take_while(|b| *b == b':').count();
        required = required.max(opening).max(local.lines().last().map(directive_closing_width).unwrap_or(0));
    }
    let mut ancestors = aero_core::containers::ancestors(doc, target);
    if into {
        ancestors.push(doc.blocks().find(target)?);
    }
    for ancestor in ancestors.into_iter().rev().filter(|b| matches!(b.kind, BlockKind::Directive { .. })) {
        let span = doc.range_of(ancestor);
        let opening = &original[span.clone()];
        let count = opening.bytes().take_while(|b| *b == b':').count();
        let local = aero_core::containers::standalone(doc, span.clone());
        let length = local.lines().last().map(directive_closing_width).unwrap_or(0);
        if count > required {
            required = count.max(length);
            continue;
        }
        let closing = span.start + opening.rfind('\n')? + 1;
        let start = closing + original[closing..span.end].find(':')?;
        if count < 3 || length < count || !original[start + length..span.end].trim().is_empty() {
            return None;
        }
        required = (required + 1).max(length);
        let fence = ":".repeat(required);
        edits.push(Edit::replace(span.start..span.start + count, fence.clone()));
        edits.push(Edit::replace(start..start + length, fence));
    }
    Some(edits)
}

pub fn move_node(
    doc: &Document,
    source: aero_core::BlockId,
    target: aero_core::BlockId,
    after: bool,
    into: bool,
) -> Option<Plan> {
    let to = doc.blocks().find(target)?;
    if source == target || aero_core::containers::ancestors(doc, target).iter().any(|b| b.id == source) {
        return None;
    }
    let range = aero_core::properties::owned_range(doc, source)?;
    let target_range = aero_core::properties::owned_range(doc, target)?;
    let original = doc.to_string();
    let piece = aero_core::containers::standalone(doc, range.clone());
    let (at, prefix) = if into {
        if !aero_core::containers::is_editable(&to.kind) && !matches!(to.kind, BlockKind::ListItem { .. }) {
            return None;
        }
        let prefix = aero_core::containers::content_continuation(doc, target);
        match to.kind {
            BlockKind::BlockQuote | BlockKind::ListItem { .. } => (doc.range_of(to).end, prefix),
            _ => {
                let span = doc.range_of(to);
                let offset = original[span.clone()].rfind('\n')? + span.start;
                (offset, prefix)
            }
        }
    } else {
        (
            if after { target_range.end } else { target_range.start },
            if after {
                aero_core::containers::continuation(doc, target)
            } else {
                aero_core::containers::opening_continuation(doc, target)
            },
        )
    };
    if range.start <= at && at <= range.end {
        return None;
    }
    let inserted = Document::from_text(&piece);
    let moved = TreeShape::read(doc, doc.blocks().find(source)?);
    let shapes = tree_shapes(&inserted);
    let exported = if matches!(moved.kind, BlockKind::ListItem { .. }) {
        if shapes.len() != 1 {
            return None;
        }
        &shapes.first().filter(|b| matches!(b.kind, BlockKind::List { .. }))?.children
    } else {
        &shapes
    };
    if exported.as_slice() != [moved] {
        return None;
    }
    let position = if into {
        TreeInsertion::Into
    } else if after {
        TreeInsertion::After
    } else {
        TreeInsertion::Before
    };
    let expected = changed_tree(doc, &doc.blocks().roots, &[source], target, position, &shapes);
    if expected == tree_shapes(doc) && prefix == aero_core::containers::opening_continuation(doc, source) {
        return None;
    }
    let piece = piece.trim_end_matches('\n').replace('\n', &format!("\n{prefix}"));
    let insertion = if into || after {
        format!("\n{prefix}\n{prefix}{piece}")
    } else {
        format!("{piece}\n{prefix}\n{prefix}")
    };
    let selection = SelectionPlan::nodes(doc, &[source]);
    let mut edits = selection.deletions(doc);
    if !selection.targets.first()?.nested {
        // Moving a root leaves unrelated separator trivia in its original slot.
        for edit in edits.iter_mut().filter(|edit| edit.text.is_empty()) {
            edit.text = format!(
                "{}{}",
                &original[edit.range.start..range.start],
                &original[range.end..edit.range.end],
            );
        }
    }
    edits.extend(recipient_fence_edits(doc, target, into, &inserted)?);
    if edits.iter().any(|e| e.range.start < at && at < e.range.end) {
        return None;
    }
    let shift = edits
        .iter()
        .filter(|e| e.range.end <= at)
        .map(|e| e.text.len() as isize - e.range.len() as isize)
        .sum::<isize>();
    let focus_offset =
        (at as isize + shift) as usize + if into || after { 2 * (1 + prefix.len()) } else { 0 };
    edits.push(Edit::insert(at, insertion));
    edits.sort_by_key(|e| std::cmp::Reverse(e.range.start));
    let mut probe = doc.clone();
    probe.edit_group(edits).ok()?;
    probe.reparse();
    if tree_shapes(&probe) != expected || probe.to_string() == original {
        return None;
    }
    Some(Plan { edit: Edit::replace(0..doc.len(), probe.to_string()), focus_offset })
}

fn directive_closing_width(line: &str) -> usize {
    // Ancestor indentation can expose colon closers before the parser visits code children.
    let rest = line.trim_start_matches([' ', '\t']);
    let count = rest.bytes().take_while(|b| *b == b':').count();
    if count >= 3 && rest[count..].bytes().all(|b| matches!(b, b' ' | b'\t' | b'\r')) { count } else { 0 }
}

fn adapt_moved_block(raw: &str, source: &Unit, units: &[Unit], place: DropPlace, text: &str) -> String {
    let body = strip_list_marker(raw);
    let style = destination_style(source, units, place, text);
    let plain = style.is_none();
    let mut rendered = match style {
        Some(style) => prefix_list(&body, &style),
        None => body,
    };
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    // 段落之间留空行；列表项只换行，避免把列表拆开
    if plain {
        rendered.push('\n');
    }
    rendered
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
    let Some(first) = piece.lines().next() else { return piece.to_owned() };
    let indent_len = first.chars().take_while(|c| *c == ' ' || *c == '\t').map(char::len_utf8).sum();
    let Some(marker_len) = list_marker_len(&first[indent_len..]) else { return piece.trim_end().to_owned() };
    let indent = &first[..indent_len];
    let mut body = first[indent_len + marker_len..].to_owned();
    for line in piece.lines().skip(1) {
        body.push('\n');
        body.push_str(line.strip_prefix(indent).unwrap_or(line));
    }
    body.trim_end().to_owned()
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
    let mut lines = body.lines();
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

fn assemble_move(text: &str, cut: Range<usize>, insert_at: usize, piece: &str) -> String {
    let piece = if piece.ends_with('\n') { piece.to_owned() } else { format!("{piece}\n") };
    let mut out = String::with_capacity(text.len() + piece.len());
    let push_piece = |out: &mut String, left_ends: bool, piece: &str| {
        if left_ends && !piece.starts_with('\n') && !is_list_piece(piece) && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(piece);
    };
    if insert_at <= cut.start {
        out.push_str(&text[..insert_at]);
        let gap = !out.is_empty();
        push_piece(&mut out, gap, &piece);
        let between = &text[insert_at..cut.start];
        if !between.chars().all(|ch| ch == '\n') {
            out.push_str(between);
        }
        out.push_str(&text[cut.end..]);
    } else {
        let at = insert_at.max(cut.end);
        out.push_str(&text[..cut.start]);
        out.push_str(&text[cut.end..at]);
        let gap = !out.is_empty();
        push_piece(&mut out, gap, &piece);
        out.push_str(&text[at..]);
    }
    // Only normalize boundary separators; interior blank lines may be literal code data.
    out
}

fn is_list_piece(piece: &str) -> bool {
    let line = piece.trim_start_matches('\n').lines().next().unwrap_or("");
    let indent = line.chars().take_while(|c| *c == ' ' || *c == '\t').map(char::len_utf8).sum();
    list_marker_len(&line[indent..]).is_some()
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
    let id = conversion_target(doc, unit);
    let mut current = unit.clone();
    if id == unit.id && !matches!(unit.kind, BlockKind::ListItem { .. }) {
        current.list = None;
    }
    if block_type(&current) == Some(target) {
        return None;
    }
    convert_node(doc, id, target).ok().flatten()
}

/// Keyboard and slash commands address the first paragraph's item, while
/// continuation paragraphs and nested containers retain their own scope.
pub fn conversion_target(doc: &Document, unit: &Unit) -> BlockId {
    if matches!(unit.kind, BlockKind::Paragraph) {
        if let Some(item) = unit.list.as_ref().and_then(|ctx| {
            doc.blocks().iter().map(|(_, b)| b).find(|b| {
                matches!(b.kind, BlockKind::ListItem { .. })
                    && doc.range_of(b) == ctx.item_range
                    && b.children
                        .iter()
                        .find(|c| c.kind != BlockKind::Marker)
                        .is_some_and(|c| c.id == unit.id)
            })
        }) {
            return item.id;
        }
    }
    unit.id
}

pub fn conversion_available(doc: &Document, unit: &Unit, target: BlockType) -> bool {
    let id = conversion_target(doc, unit);
    let mut current = unit.clone();
    if id == unit.id && !matches!(unit.kind, BlockKind::ListItem { .. }) {
        current.list = None;
    }
    block_type(&current).is_some_and(|kind| kind != target) && conversion_reason(doc, id, target).is_none()
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

fn convert_leaf(doc: &Document, units: &[Unit], ix: usize, target: BlockType) -> Option<Plan> {
    let unit = &units[ix];
    if target == BlockType::Highlight || target == BlockType::Quote {
        let edit = aero_core::containers::wrap(doc, unit.id, target == BlockType::Highlight).ok()?;
        let focus_offset = edit.range.start + edit.text.find('\n').map_or(0, |n| n + 1);
        return Some(Plan { edit, focus_offset });
    }
    let current = block_type(unit)?;
    if current == target {
        return None;
    }
    let (start, external) = if let Some(ctx) = &unit.list {
        let item =
            doc.blocks().iter().map(|(_, b)| b).find(|b| {
                matches!(b.kind, BlockKind::ListItem { .. }) && doc.range_of(b) == ctx.item_range
            })?;
        (ctx.item_range.start, aero_core::containers::continuation(doc, item.id))
    } else {
        (unit.range.start, aero_core::containers::continuation(doc, unit.id))
    };
    let local = if unit.range.is_empty() {
        String::new()
    } else {
        aero_core::containers::local_source(doc, unit.id)?
    };
    let src = local.as_str();
    let content: String = match current {
        BlockType::Heading(_) => src[heading_prefix_len(src)..].to_owned(),
        BlockType::Quote | BlockType::Highlight => return None,
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
        BlockType::Code => {
            let longest =
                content.lines().map(|line| line.chars().take_while(|c| *c == '`').count()).max().unwrap_or(0);
            let fence = "`".repeat(longest.max(2) + 1);
            (format!("{fence}\n{content}\n{fence}"), fence.len() + 1 + content.len())
        }
        BlockType::Highlight => unreachable!(),
        BlockType::Divider => unreachable!(),
    };
    let block = doc.blocks().find(unit.id)?;
    let mut text = text;
    let metadata = if block.attrs.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", aero_core::properties::render(&block.attrs))
    };
    if !metadata.is_empty() || block.persisted_id.is_some() {
        text.push_str(if target == BlockType::Code { "\n\n" } else { " " });
        text.push_str(&metadata);
        if let Some(id) = block.persisted_id.as_deref() {
            if !metadata.is_empty() {
                text.push(' ');
            }
            text.push('^');
            text.push_str(id);
        }
    }
    let end = aero_core::properties::owned_range(doc, unit.id)?.end;
    let cursor = cursor + text[..cursor].bytes().filter(|b| *b == b'\n').count() * external.len();
    let text = text.replace('\n', &format!("\n{external}"));
    Some(Plan { focus_offset: start + cursor, edit: Edit::replace(start..end, text) })
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
        let ix = units.iter().position(|u| u.id == leaf).ok_or("正文不可转换")?;
        return Ok(convert_leaf(doc, &units, ix, target));
    }
    if aero_core::containers::is_editable(&block.kind) {
        return Err("容器不能无损压缩为单个正文块，请使用解除容器".into());
    }
    let mut units = super::units::units(doc);
    let ix = units.iter().position(|u| u.id == id).ok_or("此块不支持类型转换")?;
    // A paragraph's own handle changes its syntax inside the existing item.
    units[ix].list = None;
    Ok(convert_leaf(doc, &units, ix, target))
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
#[cfg(test)]
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
    fn phase2_list_levels_preserve_quoted_task_subtree_and_sibling_metadata() {
        let source = "> - parent\n> - [x] {aero-owner=item .red custom=keep} ^item\n>\n>   body ^body\n>\n>   continuation ^cont\n>\n>   - nested ^nested\n> - {aero-owner=item .blue} ^sibling\n>\n>   other\n";
        let mut doc = Document::from_text(source);
        let item = aero_core::block_refs::resolve(&doc, "item").unwrap();
        let old = TreeShape::read(&doc, doc.blocks().find(item).unwrap());
        let sibling = TreeShape::read(&doc, doc.blocks().find(aero_core::block_refs::resolve(&doc, "sibling").unwrap()).unwrap());
        let plan = list_indent(&doc, item).expect("preceding item receives the whole subtree");
        doc.edit(plan.edit).unwrap();
        doc.reparse();
        let indented = doc.to_string();
        let item = aero_core::block_refs::resolve(&doc, "item").unwrap();
        assert_eq!(TreeShape::read(&doc, doc.blocks().find(item).unwrap()), old);
        assert_eq!(aero_core::containers::ancestors(&doc, item).iter().filter(|b| matches!(b.kind, BlockKind::ListItem { .. })).count(), 1);
        assert_eq!(TreeShape::read(&doc, doc.blocks().find(aero_core::block_refs::resolve(&doc, "sibling").unwrap()).unwrap()), sibling);
        let plan = list_outdent(&doc, item).unwrap();
        doc.edit(plan.edit).unwrap();
        doc.reparse();
        let item = aero_core::block_refs::resolve(&doc, "item").unwrap();
        assert_eq!(TreeShape::read(&doc, doc.blocks().find(item).unwrap()), old);
        assert_eq!(aero_core::containers::ancestors(&doc, item).iter().filter(|b| matches!(b.kind, BlockKind::ListItem { .. })).count(), 0);
        doc.undo().unwrap();
        assert_eq!(doc.to_string(), indented);
        doc.undo().unwrap();
        assert_eq!(doc.to_string(), source);
    }

    #[test]
    fn phase2_root_task_exit_keeps_continuations_and_nested_children() {
        for source in [
            "> - [x] first ^first\n>\n>   second ^second\n>\n>   - child ^child\n> - after ^after\n",
            "- {aero-owner=item .red} ^item\n\n  first ^first\n\n  second ^second\n\n  - child ^child\n- after ^after\n",
            ":::highlight\n- {aero-owner=item .red} ^item\n\n  first ^first\n\n  second ^second\n\n  - child ^child\n- after ^after\n:::\n",
        ] {
            let mut doc = Document::from_text(source);
            let us = units(&doc);
            if source.contains("^item") {
                assert!(matches!(backspace_at_start(&doc, &us, 0), Backspace::Blocked(_)));
                assert_eq!(doc.to_string(), source);
                continue;
            }
            let Backspace::Edit(plan) = backspace_at_start(&doc, &us, 0) else { panic!("root exit {source}") };
            doc.edit(plan.edit).unwrap();
            doc.reparse();
            for id in ["first", "second", "child", "after"] { assert!(aero_core::block_refs::resolve(&doc, id).is_ok(), "{id}: {}", doc.to_string()); }
            if source.contains("^item") { assert!(aero_core::block_refs::resolve(&doc, "item").is_ok()); }
            doc.undo().unwrap();
            assert_eq!(doc.to_string(), source);
        }
    }

    #[test]
    fn phase2_numbered_task_levels_keep_marker_width_and_literal_code() {
        let source = "> 9) parent\n> 10) [x] body ^body\n>\n>     ```text\n>      leading\n>         four\n>     ```\n>\n>     continuation ^cont\n> 11) sibling ^sibling\n";
        let mut doc = Document::from_text(source);
        let item = list_item_for(&doc, aero_core::block_refs::resolve(&doc, "body").unwrap()).unwrap();
        let old = TreeShape::read(&doc, doc.blocks().find(item).unwrap());
        let plan = list_indent(&doc, item).unwrap();
        doc.edit(plan.edit).unwrap();
        doc.reparse();
        let item = list_item_for(&doc, aero_core::block_refs::resolve(&doc, "body").unwrap()).unwrap();
        assert_eq!(TreeShape::read(&doc, doc.blocks().find(item).unwrap()), old);
        let plan = list_outdent(&doc, item).unwrap();
        doc.edit(plan.edit).unwrap();
        doc.reparse();
        let item = list_item_for(&doc, aero_core::block_refs::resolve(&doc, "body").unwrap()).unwrap();
        assert_eq!(TreeShape::read(&doc, doc.blocks().find(item).unwrap()), old);
        assert!(doc.to_string().contains("10) [x] body"));
    }

    #[test]
    fn phase2_first_list_item_cannot_indent_and_empty_nested_item_outdents() {
        let quoted = Document::from_text("- parent\n\n  > quoted ^body\n");
        assert_eq!(list_item_for(&quoted, aero_core::block_refs::resolve(&quoted, "body").unwrap()), Some(quoted.blocks().roots[0].children[0].id));
        let mut doc = Document::from_text("- parent\n  - child\n  - \n");
        let first = doc.blocks().roots[0].children[0].id;
        assert!(list_indent(&doc, first).is_none());
        assert!(list_outdent(&doc, first).is_none());
        let us = units(&doc);
        let Backspace::Edit(plan) = backspace_at_start(&doc, &us, 2) else { panic!("outdent empty item") };
        doc.edit(plan.edit).unwrap();
        doc.reparse();
        assert_eq!(units(&doc).last().unwrap().list.as_ref().unwrap().parent, None);
        assert_eq!(units(&doc).len(), 3);
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

    fn check_nested_code_conversion(source: &str, expected: &str) {
        let mut doc = Document::from_text(source);
        let id = aero_core::block_refs::resolve(&doc, "body").unwrap();
        let attrs = doc.blocks().find(id).unwrap().attrs.clone();
        let ancestor_kinds: Vec<_> = aero_core::containers::ancestors(&doc, id)
            .iter()
            .map(|b| (b.kind.clone(), b.attrs.clone(), b.persisted_id.clone()))
            .collect();
        let plan = convert_node(&doc, id, BlockType::Code).unwrap().unwrap();
        assert_eq!(&source[..plan.edit.range.start], &expected[..plan.edit.range.start]);
        let focus = plan.focus_offset;
        apply(&mut doc, plan);
        assert_eq!(doc.to_string(), expected);
        assert!(doc.to_string()[..focus].ends_with("beta"));
        let id = aero_core::block_refs::resolve(&doc, "body").unwrap();
        let code = doc.blocks().find(id).unwrap();
        assert_eq!(code.kind, BlockKind::CodeFence { info: String::new() });
        assert_eq!(code.attrs, attrs);
        assert_eq!(aero_core::containers::local_source(&doc, id).unwrap(), "```\nalpha\nbeta\n```");
        assert_eq!(
            aero_core::containers::ancestors(&doc, id)
                .iter()
                .map(|b| (b.kind.clone(), b.attrs.clone(), b.persisted_id.clone()))
                .collect::<Vec<_>>(),
            ancestor_kinds
        );
        let sibling = aero_core::block_refs::resolve(&doc, "sib").unwrap();
        assert_eq!(aero_core::containers::local_source(&doc, sibling).as_deref(), Some("sibling"));
        assert_eq!(doc.blocks().find(sibling).unwrap().attrs.classes, ["blue"]);
        doc.undo().unwrap();
        assert_eq!(doc.to_string(), source);
        doc.redo().unwrap();
        doc.reparse();
        let id = aero_core::block_refs::resolve(&doc, "body").unwrap();
        let plan = convert_node(&doc, id, BlockType::Text).unwrap().unwrap();
        apply(&mut doc, plan);
        assert_eq!(doc.to_string(), source);
        let id = aero_core::block_refs::resolve(&doc, "body").unwrap();
        assert_eq!(doc.blocks().find(id).unwrap().attrs, attrs);
    }

    #[test]
    fn quote_child_code_conversion_keeps_body_metadata_and_following_siblings() {
        check_nested_code_conversion(
            "> alpha\n> beta {.red align=center} ^body\n>\n> sibling {.blue} ^sib\n\noutside\n",
            "> ```\n> alpha\n> beta\n> ```\n> \n> {.red align=center} ^body\n>\n> sibling {.blue} ^sib\n\noutside\n",
        );
    }

    #[test]
    fn list_child_code_conversion_keeps_body_metadata_and_following_siblings() {
        check_nested_code_conversion(
            "- {aero-owner=item .item} ^item\n\n  alpha\n  beta {.red align=center} ^body\n\n  sibling {.blue} ^sib\n- next ^next\n\noutside\n",
            "- {aero-owner=item .item} ^item\n\n  ```\n  alpha\n  beta\n  ```\n  \n  {.red align=center} ^body\n\n  sibling {.blue} ^sib\n- next ^next\n\noutside\n",
        );
    }

    #[test]
    fn quoted_list_first_child_conversion_retains_item_marker() {
        check_nested_code_conversion(
            "> - alpha\n>   beta {.red align=center} ^body\n>\n>   sibling {.blue} ^sib\n> - next ^next\n\noutside\n",
            "> - ```\n>   alpha\n>   beta\n>   ```\n>   \n>   {.red align=center} ^body\n>\n>   sibling {.blue} ^sib\n> - next ^next\n\noutside\n",
        );
    }

    #[test]
    fn quoted_simple_item_conversion_keeps_external_quote() {
        let source = "> - alpha\n>   beta {.red} ^body\n>\n> sibling ^sib\n\noutside\n";
        let mut doc = Document::from_text(source);
        let us = units(&doc);
        let plan = convert(&doc, &us, 0, BlockType::Code).unwrap();
        apply(&mut doc, plan);
        assert_eq!(
            doc.to_string(),
            "> ```\n> alpha\n> beta\n> ```\n> \n> {.red} ^body\n>\n> sibling ^sib\n\noutside\n"
        );
        let id = aero_core::block_refs::resolve(&doc, "body").unwrap();
        assert_eq!(aero_core::containers::local_source(&doc, id).as_deref(), Some("```\nalpha\nbeta\n```"));
        assert_eq!(doc.blocks().find(id).unwrap().attrs.classes, ["red"]);
        assert_eq!(aero_core::containers::ancestors(&doc, id).len(), 1);
        assert!(aero_core::block_refs::resolve(&doc, "sib").is_ok());
        doc.undo().unwrap();
        assert_eq!(doc.to_string(), source);
    }

    #[test]
    fn code_conversion_uses_each_lines_quote_and_task_prefix() {
        for (source, expected, roundtrip, leaf) in [
            (
                "> alpha\n>beta {.red} ^body\n>\n> sibling ^sib\n",
                "> ```\n> alpha\n> beta\n> ```\n> \n> {.red} ^body\n>\n> sibling ^sib\n",
                "> alpha\n> beta {.red} ^body\n>\n> sibling ^sib\n",
                true,
            ),
            (
                "- [ ] alpha\n  beta {.red} ^body\n\nsibling ^sib\n",
                "```\nalpha\nbeta\n```\n\n{.red} ^body\n\nsibling ^sib\n",
                "alpha\nbeta {.red} ^body\n\nsibling ^sib\n",
                false,
            ),
            (
                "> - [x] alpha\n>   beta {.red} ^body\n>\n> sibling ^sib\n",
                "> ```\n> alpha\n> beta\n> ```\n> \n> {.red} ^body\n>\n> sibling ^sib\n",
                "> alpha\n> beta {.red} ^body\n>\n> sibling ^sib\n",
                false,
            ),
            (
                "- [ ] parent\n\n  > alpha\n  >beta {.red} ^body\n\n  sibling ^sib\n",
                "- [ ] parent\n\n  > ```\n  > alpha\n  > beta\n  > ```\n  > \n  > {.red} ^body\n\n  sibling ^sib\n",
                "- [ ] parent\n\n  > alpha\n  > beta {.red} ^body\n\n  sibling ^sib\n",
                true,
            ),
        ] {
            let mut doc = Document::from_text(source);
            let id = aero_core::block_refs::resolve(&doc, "body").unwrap();
            let attrs = doc.blocks().find(id).unwrap().attrs.clone();
            let plan = if leaf {
                convert_node(&doc, id, BlockType::Code).unwrap().unwrap()
            } else {
                let us = units(&doc);
                let ix = us.iter().position(|u| u.id == id).unwrap();
                convert(&doc, &us, ix, BlockType::Code).unwrap()
            };
            apply(&mut doc, plan);
            assert_eq!(doc.to_string(), expected);
            let id = aero_core::block_refs::resolve(&doc, "body").unwrap();
            let code = doc.blocks().find(id).unwrap();
            assert_eq!(code.kind, BlockKind::CodeFence { info: String::new() });
            assert_eq!(code.attrs, attrs);
            assert_eq!(
                aero_core::containers::local_source(&doc, id).as_deref(),
                Some("```\nalpha\nbeta\n```")
            );
            let sibling = aero_core::block_refs::resolve(&doc, "sib").unwrap();
            assert_eq!(aero_core::containers::local_source(&doc, sibling).as_deref(), Some("sibling"));
            doc.undo().unwrap();
            assert_eq!(doc.to_string(), source);
            doc.redo().unwrap();
            doc.reparse();
            let id = aero_core::block_refs::resolve(&doc, "body").unwrap();
            let plan = convert_node(&doc, id, BlockType::Text).unwrap().unwrap();
            apply(&mut doc, plan);
            assert_eq!(doc.to_string(), roundtrip);
            let id = aero_core::block_refs::resolve(&doc, "body").unwrap();
            assert_eq!(doc.blocks().find(id).unwrap().attrs, attrs);
        }
    }

    #[test]
    fn nested_move_grows_only_recipient_fences_and_preserves_subtree() {
        let moving = ":::highlight {.red}\nchild {.blue} ^kid\n\n```text\n::::::::literal\n\n\n\nend\n```\n:::\n\n^moving";
        let recipient = "> :::highlight {.green}\n> resident ^resident\n> :::\n>\n> ^recipient";
        let source = format!("{moving}\n\n{recipient}\n\ntail\n");
        let mut doc = Document::from_text(&source);
        let from = aero_core::block_refs::resolve(&doc, "moving").unwrap();
        let to = aero_core::block_refs::resolve(&doc, "recipient").unwrap();
        let kid = aero_core::block_refs::resolve(&doc, "kid").unwrap();
        assert!(move_node(&doc, from, kid, false, true).is_none());
        assert!(move_node(&doc, from, from, false, true).is_none());
        let plan = move_node(&doc, from, to, false, true).expect("equal fences must allow nesting");
        apply(&mut doc, plan);
        let expected = format!(
            "\n\n> ::::highlight {{.green}}\n> resident ^resident\n> \n> {}\n> ::::\n>\n> ^recipient\n\ntail\n",
            moving.replace('\n', "\n> ")
        );
        assert_eq!(doc.to_string(), expected);
        let from = aero_core::block_refs::resolve(&doc, "moving").unwrap();
        let to = aero_core::block_refs::resolve(&doc, "recipient").unwrap();
        let kid = aero_core::block_refs::resolve(&doc, "kid").unwrap();
        let parent = doc.blocks().find(to).unwrap();
        assert_eq!(parent.attrs.classes, ["green"]);
        assert!(parent.children.iter().any(|b| b.id == from));
        let moved = doc.blocks().find(from).unwrap();
        assert_eq!(moved.attrs.classes, ["red"]);
        assert!(moved.children.iter().any(|b| b.id == kid));
        assert_eq!(doc.blocks().find(kid).unwrap().attrs.classes, ["blue"]);
        assert_eq!(aero_core::containers::local_source(&doc, kid).as_deref(), Some("child"));
        let code = moved.children.iter().find(|b| matches!(b.kind, BlockKind::CodeFence { .. })).unwrap();
        assert_eq!(
            aero_core::containers::local_source(&doc, code.id).as_deref(),
            Some("```text\n::::::::literal\n\n\n\nend\n```")
        );
        let resident = aero_core::block_refs::resolve(&doc, "resident").unwrap();
        assert!(parent.children.iter().any(|b| b.id == resident));
        doc.undo().unwrap();
        assert_eq!(doc.to_string(), source);
    }

    #[test]
    fn nested_move_without_ids_grows_all_recipient_ancestors_for_sibling_drops() {
        for after in [false, true] {
            let moving = ":::highlight {.red}\nmoving body\n:::";
            let source = format!(
                "{moving}\n\n::::highlight {{.outer}}\n> :::highlight {{.inner}}\n> resident ^resident\n> :::\n::::\n\n^outer\n\noutside\n"
            );
            let mut doc = Document::from_text(&source);
            let from = doc.blocks().roots[0].id;
            let to = aero_core::block_refs::resolve(&doc, "resident").unwrap();
            let plan = move_node(&doc, from, to, after, false).expect("nested sibling drop must be valid");
            apply(&mut doc, plan);
            let piece = moving.replace('\n', "\n> ");
            let middle = if after {
                format!("resident ^resident\n> \n> {piece}")
            } else {
                format!("{piece}\n> \n> resident ^resident")
            };
            assert_eq!(
                doc.to_string(),
                format!(
                    "\n\n:::::highlight {{.outer}}\n> ::::highlight {{.inner}}\n> {middle}\n> ::::\n:::::\n\n^outer\n\noutside\n"
                )
            );
            let outer = aero_core::block_refs::resolve(&doc, "outer").unwrap();
            let outer = doc.blocks().find(outer).unwrap();
            assert_eq!(outer.attrs.classes, ["outer"]);
            let inner = &outer.children[0].children[0];
            assert_eq!(inner.attrs.classes, ["inner"]);
            let moved = &inner.children[usize::from(after)];
            assert_eq!(moved.attrs.classes, ["red"]);
            assert_eq!(moved.children.len(), 1);
            assert_eq!(
                aero_core::containers::local_source(&doc, moved.children[0].id).as_deref(),
                Some("moving body")
            );
            doc.undo().unwrap();
            assert_eq!(doc.to_string(), source);
        }
    }

    #[test]
    fn moving_code_colon_closers_without_ids_grows_recipient_fences() {
        for (literal, fence) in [
            (":::", "::::"),
            ("   :::: \t", ":::::"),
            ("::::::::literal", ":::"),
            (":::::::: literal", ":::"),
            ("    :::::", "::::::"),
            ("\t:::::", "::::::"),
            (":::::\u{a0}", ":::"),
        ] {
            let moving = format!("```text\n{literal}\n```");
            let source = format!("{moving}\n\n:::highlight\ny\n:::\n\nsibling\n");
            let mut doc = Document::from_text(&source);
            let from = doc.blocks().roots[0].id;
            let to = doc.blocks().roots[1].id;
            let plan = move_node(&doc, from, to, false, true).unwrap();
            apply(&mut doc, plan);
            assert_eq!(doc.blocks().roots.len(), 2, "{doc}");
            let recipient = &doc.blocks().roots[0];
            assert_eq!(recipient.kind, BlockKind::Directive { name: "highlight".into() });
            assert_eq!(recipient.children.len(), 2, "{doc}");
            assert_eq!(
                aero_core::containers::local_source(&doc, recipient.children[0].id).as_deref(),
                Some("y")
            );
            let code = &recipient.children[1];
            assert_eq!(code.kind, BlockKind::CodeFence { info: "text".into() });
            assert_eq!(aero_core::containers::local_source(&doc, code.id).as_deref(), Some(moving.as_str()));
            assert_eq!(
                aero_core::containers::local_source(&doc, doc.blocks().roots[1].id).as_deref(),
                Some("sibling")
            );
            assert_eq!(doc.to_string(), format!("\n\n{fence}highlight\ny\n\n{moving}\n{fence}\n\nsibling\n"));
            doc.undo().unwrap();
            assert_eq!(doc.to_string(), source);
        }
    }

    #[test]
    fn moving_code_into_indented_directives_preserves_literal_and_tree() {
        let source = "~~~text\n    :::\n~~~\n\n  ::::highlight\n  :::highlight\ny\n  :::\n  ::::\n";
        let mut doc = Document::from_text(source);
        let from = doc.blocks().roots[0].id;
        let to = doc.blocks().roots[1].children[0].id;
        let plan = move_node(&doc, from, to, false, true).unwrap();
        apply(&mut doc, plan);
        assert_eq!(doc.blocks().roots.len(), 1, "{doc}");
        let outer = &doc.blocks().roots[0];
        assert_eq!(outer.kind, BlockKind::Directive { name: "highlight".into() });
        assert_eq!(outer.children.len(), 1, "{doc}");
        let inner = &outer.children[0];
        assert_eq!(inner.kind, BlockKind::Directive { name: "highlight".into() });
        assert_eq!(inner.children.len(), 2, "{doc}");
        assert_eq!(inner.children[0].kind, BlockKind::Paragraph);
        assert_eq!(doc.slice(doc.range_of(&inner.children[0])).to_string(), "y");
        let code = &inner.children[1];
        assert_eq!(code.kind, BlockKind::CodeFence { info: "text".into() });
        assert_eq!(doc.slice(doc.range_of(code)).to_string(), "~~~text\n      :::\n  ~~~");
        assert_eq!(
            aero_core::containers::local_source(&doc, code.id).as_deref(),
            Some("~~~text\n    :::\n~~~")
        );
        doc.undo().unwrap();
        assert_eq!(doc.to_string(), source);
    }

    #[test]
    fn moving_tabbed_colon_code_keeps_literal_under_directive_columns() {
        let source = "~~~text\n\t:::\n~~~\n\n  ::::highlight\n  :::highlight\ny\n  :::\n  ::::\n";
        let mut doc = Document::from_text(source);
        let from = doc.blocks().roots[0].id;
        let to = doc.blocks().roots[1].children[0].id;
        let plan = move_node(&doc, from, to, false, true).unwrap();
        apply(&mut doc, plan);
        assert_eq!(doc.blocks().roots.len(), 1, "{doc}");
        let outer = &doc.blocks().roots[0];
        assert_eq!(outer.children.len(), 1, "{doc}");
        let inner = &outer.children[0];
        assert_eq!(inner.children.len(), 2, "{doc}");
        let code = &inner.children[1];
        assert_eq!(code.kind, BlockKind::CodeFence { info: "text".into() });
        assert_eq!(doc.slice(doc.range_of(code)).to_string(), "~~~text\n  \t:::\n  ~~~");
        assert_eq!(
            aero_core::containers::local_source(&doc, code.id).as_deref(),
            Some("~~~text\n\t:::\n~~~")
        );
        doc.undo().unwrap();
        assert_eq!(doc.to_string(), source);
    }

    #[test]
    fn moving_indented_code_before_paragraph_preserves_literal_and_undo() {
        let source = "tail\n\n  ~~~text\n    keep\n  ~~~\n";
        let mut doc = Document::from_text(source);
        let from = doc.blocks().roots[1].id;
        let to = doc.blocks().roots[0].id;
        let plan = move_node(&doc, from, to, false, false).unwrap();
        apply(&mut doc, plan);
        assert_eq!(doc.blocks().roots.len(), 2, "{doc}");
        let code = &doc.blocks().roots[0];
        assert_eq!(code.kind, BlockKind::CodeFence { info: "text".into() });
        assert_eq!(doc.slice(doc.range_of(code)).to_string(), "~~~text\n  keep\n~~~");
        let tail = &doc.blocks().roots[1];
        assert_eq!(tail.kind, BlockKind::Paragraph);
        assert_eq!(doc.slice(doc.range_of(tail)).to_string(), "tail");
        assert!(doc.to_string().ends_with("tail\n\n  \n"), "{doc}");
        doc.undo().unwrap();
        assert_eq!(doc.to_string(), source);
    }

    #[test]
    fn moving_code_between_nested_prefixes_keeps_body_and_destination() {
        for (container, prefix) in [
            ("  :::highlight\ny\n  :::", "  "),
            ("  ::::highlight\n    :::highlight\ny\n    :::\n  ::::", "    "),
            ("> - item\n>\n>     :::highlight\n>     y\n>     :::", ">     "),
            ("  ::::highlight\n  > - item\n  >\n  >   :::highlight\n  >   y\n  >   :::\n  ::::", "  >   "),
            ("-\t:::highlight\n\ty\n\t:::", "    "),
        ] {
            let moving = "  ~~~text\n    keep\n  \tmore\n  ~~~";
            let source = format!("{moving}\n\n{container}\n\ntail\n");
            let mut doc = Document::from_text(&source);
            let from = doc.blocks().roots[0].id;
            let to = doc
                .blocks()
                .iter()
                .filter(|(_, b)| matches!(b.kind, BlockKind::Directive { .. }))
                .last()
                .unwrap()
                .1
                .id;
            let recipient = doc.blocks().find(to).unwrap();
            let resident = recipient.children.last().unwrap();
            assert_eq!(aero_core::containers::local_source(&doc, resident.id).as_deref(), Some("y"));
            assert!(
                doc.slice(doc.range_of(recipient)).to_string().trim_end().ends_with(":::"),
                "recipient range {:?}: {:?}; parse: {:?}; source: {source}",
                doc.range_of(recipient),
                doc.slice(doc.range_of(recipient)).to_string(),
                aero_core::parse(&source)
            );
            let plan = move_node(&doc, from, to, false, true).unwrap();
            apply(&mut doc, plan);
            let recipient = doc
                .blocks()
                .iter()
                .filter(|(_, b)| matches!(b.kind, BlockKind::Directive { .. }))
                .last()
                .unwrap()
                .1;
            assert_eq!(recipient.children.len(), 2, "{doc}");
            let code = &recipient.children[1];
            assert_eq!(code.kind, BlockKind::CodeFence { info: "text".into() }, "{doc}");
            assert_eq!(
                doc.slice(doc.range_of(code)).to_string(),
                format!("~~~text\n{prefix}  keep\n{prefix}\tmore\n{prefix}~~~")
            );
            assert_eq!(
                aero_core::containers::local_source(&doc, code.id).as_deref(),
                Some("~~~text\n  keep\n\tmore\n~~~")
            );
            assert_eq!(
                aero_core::containers::local_source(&doc, recipient.children[0].id).as_deref(),
                Some("y")
            );
            assert!(doc.to_string().ends_with("\n\ntail\n"));
            doc.undo().unwrap();
            assert_eq!(doc.to_string(), source);
        }
    }

    #[test]
    fn moving_before_indented_code_preserves_both_fence_offsets() {
        let source = "~~~text\n  moving\n~~~\n\n  ~~~text\n    resident\n  ~~~\n";
        let mut doc = Document::from_text(source);
        let from = doc.blocks().roots[0].id;
        let to = doc.blocks().roots[1].id;
        let plan = move_node(&doc, from, to, false, false).unwrap();
        apply(&mut doc, plan);
        assert_eq!(doc.blocks().roots.len(), 2, "{doc}");
        for (code, expected) in doc.blocks().roots.iter().zip(["  moving", "  resident"]) {
            assert_eq!(code.kind, BlockKind::CodeFence { info: "text".into() });
            assert_eq!(
                aero_core::containers::local_source(&doc, code.id).unwrap(),
                format!("~~~text\n{expected}\n~~~")
            );
        }
        doc.undo().unwrap();
        assert_eq!(doc.to_string(), source);
    }

    #[test]
    fn moving_longer_directive_closer_without_ids_grows_all_ancestors() {
        let moving = ":::highlight\nchild\n::::";
        let source =
            format!("{moving}\n\n::::highlight\n:::highlight\ny\n:::\n\nresident\n::::\n\nsibling\n");
        let mut doc = Document::from_text(&source);
        let from = doc.blocks().roots[0].id;
        let to = doc.blocks().roots[1].children[0].id;
        let plan = move_node(&doc, from, to, false, true).unwrap();
        apply(&mut doc, plan);
        assert_eq!(doc.blocks().roots.len(), 2, "{doc}");
        let outer = &doc.blocks().roots[0];
        assert_eq!(outer.children.len(), 2, "{doc}");
        let inner = &outer.children[0];
        assert_eq!(inner.children.len(), 2, "{doc}");
        assert_eq!(aero_core::containers::local_source(&doc, inner.children[0].id).as_deref(), Some("y"));
        let moved = &inner.children[1];
        assert_eq!(moved.children.len(), 1);
        assert_eq!(aero_core::containers::local_source(&doc, moved.id).as_deref(), Some(moving));
        assert_eq!(aero_core::containers::local_source(&doc, moved.children[0].id).as_deref(), Some("child"));
        assert_eq!(
            aero_core::containers::local_source(&doc, outer.children[1].id).as_deref(),
            Some("resident")
        );
        assert_eq!(
            aero_core::containers::local_source(&doc, doc.blocks().roots[1].id).as_deref(),
            Some("sibling")
        );
        assert_eq!(
            doc.to_string(),
            format!(
                "\n\n::::::highlight\n:::::highlight\ny\n\n{moving}\n:::::\n\nresident\n::::::\n\nsibling\n"
            )
        );
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
    fn g02_paste_replaces_disjoint_children_not_their_containers() {
        let source = ":::highlight {.red custom=one}\nfirst ^first\n:::\n\n^left\n\n:::highlight {.blue custom=two}\nsecond ^second\n:::\n\n^right\n\n<!-- opaque  bytes -->\n";
        let doc = Document::from_text(source);
        let us = units(&doc);
        let mut result = SelectionPlan::units(&doc, &us, 0, 1).replace(&doc, "new ^new").unwrap();
        assert_eq!(
            result.to_string(),
            source.replace("first ^first", "new ^new").replace("second ^second", "")
        );
        result.undo().unwrap();
        assert_eq!(result.to_string(), source);
    }

    #[test]
    fn g02_paste_keeps_list_and_quote_ancestor_prefixes() {
        for (source, first, last, expected) in [
            (
                "- first\n\n  continuation\n\n  - nested\n\noutside\n",
                1,
                1,
                "- first\n\n  new\n  \n  other\n\n  - nested\n\noutside\n",
            ),
            ("> first\n>\n> second\n\noutside\n", 0, 1, "> new\n> \n> other\n>\n> \n\noutside\n"),
        ] {
            let doc = Document::from_text(source);
            let us = units(&doc);
            let mut result =
                SelectionPlan::units(&doc, &us, first, last).replace(&doc, "new\n\nother").unwrap();
            assert_eq!(result.to_string(), expected);
            result.undo().unwrap();
            assert_eq!(result.to_string(), source);
        }
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
        doc.edit_group(delete_units(&doc, &us, 1, 1)).unwrap();
        assert_eq!(doc.to_string(), "aaa\n\nccc\n");
        // 删最后一块：前面的空行也收掉
        doc.reparse();
        let us = units(&doc);
        doc.edit_group(delete_units(&doc, &us, 1, 1)).unwrap();
        assert_eq!(doc.to_string(), "aaa\n");
        // 删第一块
        let mut doc = Document::from_text("aaa\n\nbbb\n");
        let us = units(&doc);
        doc.edit_group(delete_units(&doc, &us, 0, 0)).unwrap();
        assert_eq!(doc.to_string(), "bbb\n");
        // 全删：空文档
        let mut doc = Document::from_text("aaa\n\nbbb\n");
        let us = units(&doc);
        doc.edit_group(delete_units(&doc, &us, 0, 1)).unwrap();
        assert_eq!(doc.to_string(), "");
        // 列表项：整行；紧邻的项不需要空行
        let mut doc = Document::from_text("- a\n- b\n- c\n");
        let us = units(&doc);
        doc.edit_group(delete_units(&doc, &us, 1, 1)).unwrap();
        assert_eq!(doc.to_string(), "- a\n- c\n");
        // 标题与紧跟的段落之间没有空行
        let mut doc = Document::from_text("# T\npara\n");
        let us = units(&doc);
        doc.edit_group(delete_units(&doc, &us, 0, 0)).unwrap();
        assert_eq!(doc.to_string(), "para\n");
        // 删最后一块、且前一行以多字节字符结尾：收空行不能按字节切片（曾 panic 在 `。` 中间）
        for before in ["中文。", "图 🎨", "ascii"] {
            let mut doc = Document::from_text(&format!("{before}\n\n![[x.png]]\n"));
            let us = units(&doc);
            doc.edit_group(delete_units(&doc, &us, 1, 1)).unwrap();
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
    fn move_unit_preserves_unrelated_fenced_code_bytes_and_undo() {
        let untouched = "```text\nkeep\n\n\n\nspaces  \n```\n\n{.blue} ^code\n\n\n\n";
        for (source, from, before, expected) in [
            (format!("{untouched}aaa\n\nbbb\n\nccc\n"), 3, 1, format!("{untouched}ccc\n\naaa\n\nbbb\n")),
            (format!("aaa\n\nbbb\n\n{untouched}ccc\n"), 0, 3, format!("bbb\n\n{untouched}aaa\n\nccc\n")),
        ] {
            let mut doc = Document::from_text(&source);
            let us = units(&doc);
            let plan = move_unit(&doc, &us, from, DropPlace { before, parent: None }).unwrap();
            apply(&mut doc, plan);
            assert_eq!(doc.to_string(), expected);
            let code = aero_core::block_refs::resolve(&doc, "code").unwrap();
            assert_eq!(doc.blocks().find(code).unwrap().attrs.classes, ["blue"]);
            assert_eq!(
                aero_core::containers::local_source(&doc, code).as_deref(),
                Some("```text\nkeep\n\n\n\nspaces  \n```")
            );
            doc.undo().unwrap();
            assert_eq!(doc.to_string(), source);
        }
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
}
