use std::collections::HashMap;

use super::*;

const GAP: f32 = 8.;
const LIST_GAP: f32 = 4.;
const OVERSCAN: f32 = 480.;
const PLAIN_UNIT_LIMIT: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum RowKey {
    Root(BlockId),
    Item(BlockId),
    Pending(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowContent {
    Root(usize),
    ListItem { root: usize, item: usize, number: Option<u32> },
    Pending(usize),
}

struct Row {
    key: RowKey,
    content: RowContent,
    units: Range<usize>,
    height: Pixels,
    gap: Pixels,
}

/// Unstyled top-level lists can split at item boundaries. Styled roots and each
/// item's entire subtree stay intact so inherited paint and nested layout are unchanged.
pub(super) struct Viewport {
    dirty: bool,
    pending: Vec<usize>,
    rows: Vec<Row>,
    prefix: Vec<Pixels>,
    unit_rows: Vec<usize>,
    mounted: Vec<usize>,
    pub unit_ranges: Vec<Range<usize>>,
    origin: Pixels,
    height: Pixels,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            dirty: true,
            pending: Vec::new(),
            rows: Vec::new(),
            prefix: vec![px(0.)],
            unit_rows: Vec::new(),
            mounted: Vec::new(),
            unit_ranges: Vec::new(),
            origin: px(40.),
            height: px(0.),
        }
    }
}

impl Viewport {
    fn rebuild(&mut self, doc: &Document, units: &[Unit], pending: &[usize]) {
        let heights: HashMap<_, _> = self.rows.iter().map(|r| (r.key, r.height)).collect();
        self.rows.clear();
        self.unit_rows = vec![0; units.len()];
        self.pending = pending.to_vec();
        let mut pending = pending.iter().copied().peekable();
        let mut unit = 0;
        for (root, block) in doc.blocks().roots.iter().enumerate() {
            if matches!(block.kind, BlockKind::Marker | BlockKind::FrontMatter) {
                continue;
            }
            let range = doc.range_of(block);
            while pending.peek().is_some_and(|off| *off <= range.start) {
                self.push_pending(pending.next().unwrap(), &heights);
            }
            if units.len() > PLAIN_UNIT_LIMIT
                && block.attrs.is_empty()
                && let BlockKind::List { ordered, start } = block.kind
                && block.children.iter().any(|item| matches!(item.kind, BlockKind::ListItem { .. }))
            {
                let items = block
                    .children
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| matches!(item.kind, BlockKind::ListItem { .. }));
                for (index, (item, child)) in items.enumerate() {
                    let content =
                        RowContent::ListItem { root, item, number: ordered.then_some(start + index as u32) };
                    self.push_block(child, content, doc, units, &mut unit, &heights);
                }
                // Only sibling items share the list's 4px gap. The final item
                // participates in the ordinary 8px root flow (or has no tail gap).
                self.rows.last_mut().unwrap().gap = px(GAP);
            } else {
                self.push_block(block, RowContent::Root(root), doc, units, &mut unit, &heights);
            }
        }
        for offset in pending {
            self.push_pending(offset, &heights);
        }
        self.rebuild_prefix();
        self.dirty = false;
    }

    fn push_block(
        &mut self,
        block: &Block,
        content: RowContent,
        doc: &Document,
        units: &[Unit],
        unit: &mut usize,
        heights: &HashMap<RowKey, Pixels>,
    ) {
        let first = *unit;
        let end = doc.range_of(block).end;
        while *unit < units.len()
            && (units[*unit].range.start < end
                || (units[*unit].range.start == end && units[*unit].range.is_empty()))
        {
            self.unit_rows[*unit] = self.rows.len();
            *unit += 1;
        }
        let (key, gap) = match content {
            RowContent::ListItem { .. } => (RowKey::Item(block.id), LIST_GAP),
            _ => (RowKey::Root(block.id), GAP),
        };
        // Cache the body height by stable identity, never by ordinal or including
        // the gap: insertion/reordering can change numbering and the final item.
        let height = heights
            .get(&key)
            .copied()
            .unwrap_or_else(|| px(BODY_FONT_SIZE * BODY_LINE_HEIGHT * (*unit - first).max(1) as f32));
        self.rows.push(Row { key, content, units: first..*unit, height, gap: px(gap) });
    }

    fn push_pending(&mut self, offset: usize, heights: &HashMap<RowKey, Pixels>) {
        self.rows.push(Row {
            key: RowKey::Pending(offset),
            content: RowContent::Pending(offset),
            units: 0..0,
            height: heights
                .get(&RowKey::Pending(offset))
                .copied()
                .unwrap_or(px(BODY_FONT_SIZE * BODY_LINE_HEIGHT)),
            gap: px(GAP),
        });
    }

    fn gap(&self, row: usize) -> Pixels {
        if row + 1 < self.rows.len() { self.rows[row].gap } else { px(0.) }
    }

    fn rebuild_prefix(&mut self) {
        self.prefix.clear();
        self.prefix.push(px(0.));
        for (ix, row) in self.rows.iter().enumerate() {
            self.prefix.push(self.prefix[ix] + row.height + self.gap(ix));
        }
    }

    fn row_at(&self, y: Pixels) -> usize {
        self.prefix.partition_point(|p| *p <= y).saturating_sub(1).min(self.rows.len().saturating_sub(1))
    }

    fn mount(&mut self, top: Pixels, height: Pixels, pinned: &[usize]) {
        self.mounted.clear();
        self.unit_ranges.clear();
        if self.rows.is_empty() {
            return;
        }
        let (first, last) = if self.unit_rows.len() <= PLAIN_UNIT_LIMIT {
            (0, self.rows.len() - 1)
        } else {
            (self.row_at((top - px(OVERSCAN)).max(px(0.))), self.row_at(top + height + px(OVERSCAN)))
        };
        self.mounted.extend(first..=last);
        self.mounted.extend_from_slice(pinned);
        self.mounted.extend(
            self.rows
                .iter()
                .enumerate()
                .filter_map(|(ix, r)| matches!(r.key, RowKey::Pending(_)).then_some(ix)),
        );
        self.mounted.sort_unstable();
        self.mounted.dedup();
        for &ix in &self.mounted {
            let range = self.rows[ix].units.clone();
            if range.is_empty() {
                continue;
            }
            if let Some(last) = self.unit_ranges.last_mut()
                && last.end == range.start
            {
                last.end = range.end;
            } else {
                self.unit_ranges.push(range);
            }
        }
    }
}

pub(super) fn structure(doc: &Document) -> Vec<(BlockId, BlockKind, aero_core::Attrs, usize)> {
    doc.blocks()
        .iter()
        .map(|(_, block)| (block.id, block.kind.clone(), block.attrs.clone(), block.children.len()))
        .collect()
}

/// Match the actual container padding and list marker column, rather than
/// treating every ancestor as a list level. Built only when structure changes.
pub(super) fn gutter_reaches(doc: &Document) -> HashMap<BlockId, Pixels> {
    fn walk(blocks: &[Block], inset: Pixels, reaches: &mut HashMap<BlockId, Pixels>) {
        for block in blocks {
            // The list's operation pair owns the next column outside the
            // item handles, so the first item cannot cover the whole list.
            let width = if matches!(block.kind, BlockKind::List { .. }) { GUTTER + 24. } else { GUTTER };
            reaches.insert(block.id, px(width) + inset);
            let child = if aero_core::containers::is_editable(&block.kind) {
                px(17.) // 16pt padding plus the container's 1pt border
            } else if matches!(block.kind, BlockKind::ListItem { .. }) {
                px(LIST_INDENT)
            } else {
                px(0.)
            };
            walk(&block.children, inset + child, reaches);
        }
    }
    let mut reaches = HashMap::new();
    walk(&doc.blocks().roots, px(0.), &mut reaches);
    reaches
}

impl Editor {
    /// All unit replacement sites share the selection lookup and viewport invalidation.
    pub(super) fn refresh_units(&mut self) {
        let next = units::units(&self.doc);
        let structure = structure(&self.doc);
        let same_structure = self.unit_structure == structure
            && self.units.len() == next.len()
            && self
                .units
                .iter()
                .zip(&next)
                .all(|(old, new)| old.id == new.id && old.kind == new.kind && old.list == new.list);
        if same_structure {
            for (old, new) in self.units.iter_mut().zip(next) {
                if *old != new {
                    *old = new;
                }
            }
        } else {
            self.gutter_reaches = gutter_reaches(&self.doc);
            self.units = next;
            self.unit_indices = self.units.iter().enumerate().map(|(ix, unit)| (unit.id, ix)).collect();
            self.viewport.dirty = true;
        }
        self.unit_structure = structure;
    }

    pub(super) fn prepare_viewport(&mut self, window: &Window) {
        let mut pending: Vec<_> = self
            .empty_slots
            .iter()
            .filter(|slot| self.available_empty_slot(slot))
            .map(|slot| slot.range.start)
            .collect();
        pending.extend(self.focused.as_ref().filter(|f| f.id.is_none()).map(|f| f.range.start));
        pending.sort_unstable();
        pending.dedup();
        if self.viewport.dirty || self.viewport.pending != pending {
            let offset = self.scroll.offset();
            let top = -offset.y - self.viewport.origin;
            let anchor = if self.reveal.is_none() && top >= px(0.) && !self.viewport.rows.is_empty() {
                let row = self.viewport.row_at(top);
                Some((self.viewport.rows[row].key, self.viewport.prefix[row]))
            } else {
                None
            };
            self.viewport.rebuild(&self.doc, &self.units, &pending);
            if let Some((key, before)) = anchor
                && let Some(row) = self.viewport.rows.iter().position(|row| row.key == key)
            {
                let delta = self.viewport.prefix[row] - before;
                self.scroll.set_offset(point(offset.x, (offset.y - delta).min(px(0.))));
            }
        }
        let mut pinned = Vec::with_capacity(2);
        for id in [self.focused.as_ref().and_then(|f| f.id), self.reveal].into_iter().flatten() {
            let row =
                self.unit_index(id).and_then(|ix| self.viewport.unit_rows.get(ix).copied()).or_else(|| {
                    self.viewport
                        .rows
                        .iter()
                        .position(|r| r.key == RowKey::Root(id) || r.key == RowKey::Item(id))
                });
            if let Some(row) = row {
                pinned.push(row);
            }
        }
        let height = if self.scroll.bounds().size.height > px(0.) {
            self.scroll.bounds().size.height
        } else {
            window.viewport_size().height
        };
        let top = -self.scroll.offset().y - self.viewport.origin;
        self.viewport.height = height;
        self.viewport.mount(top, height, &pinned);
    }

    pub(super) fn render_viewport(&self, cx: &mut Context<Self>) -> AnyElement {
        // Small documents keep the original direct-child flex layout, including
        // roots temporarily covered by a multi-block source textarea.
        let plain = self.units.len() <= PLAIN_UNIT_LIMIT;
        let mut children = Vec::new();
        let mut measured = Vec::new();
        let mut next = 0;
        for &ix in &self.viewport.mounted {
            if next < ix {
                children.push(
                    div()
                        .w_full()
                        .flex_none()
                        .h(self.viewport.prefix[ix] - self.viewport.prefix[next])
                        .into_any_element(),
                );
                measured.push(None);
            }
            let row = &self.viewport.rows[ix];
            let body = match row.content {
                RowContent::Root(root) => self.render_block(&self.doc.blocks().roots[root], 0, None, cx),
                RowContent::ListItem { root, item, number } => {
                    let list = &self.doc.blocks().roots[root];
                    let body = self.render_list_item(&list.children[item], number, 0, None, cx);
                    // Keep the list-level entry at its first item without
                    // mounting the entire subtree in a virtualized document.
                    Some(if item == 0 {
                        self.with_gutter(list.id, 0, body, cx)
                    } else {
                        let active = self.block_menu.as_ref().map(|menu| menu.block).or(self.hovered_block)
                            == Some(list.id);
                        div()
                            .w_full()
                            .when(active, |el| el.bg(cx.theme().list_hover.opacity(0.3)))
                            .child(body)
                            .into_any_element()
                    })
                }
                RowContent::Pending(offset) => Some(self.render_empty_slot(offset, cx)),
            };
            if plain {
                if let Some(body) = body {
                    children.push(body);
                    measured.push(Some((ix, px(0.))));
                }
                next = ix + 1;
                continue;
            }
            // Padding belongs to the row, not the virtual spacer: adjacent roots
            // retain the original 8px gap, including at disjoint range boundaries.
            let gap = self.viewport.gap(ix);
            children.push(div().w_full().flex_none().pb(gap).children(body).into_any_element());
            measured.push(Some((ix, gap)));
            next = ix + 1;
        }
        if next < self.viewport.rows.len() {
            children.push(
                div()
                    .w_full()
                    .flex_none()
                    .h(self.viewport.prefix[self.viewport.rows.len()] - self.viewport.prefix[next])
                    .into_any_element(),
            );
            measured.push(None);
        }
        let this = cx.weak_entity();
        let scroll_offset = self.scroll.offset();
        let revealing = self.reveal.is_some();
        v_flex()
            .w_full()
            .gap(px(if plain { GAP } else { 0. }))
            .children(children)
            .on_children_prepainted(move |bounds, window, cx| {
                let _ = this.update(cx, |editor, cx| {
                    editor.measure_viewport(&measured, &bounds, scroll_offset, revealing, window, cx);
                });
            })
            .into_any_element()
    }

    fn measure_viewport(
        &mut self,
        measured: &[Option<(usize, Pixels)>],
        bounds: &[Bounds<Pixels>],
        rendered_offset: Point<Pixels>,
        revealing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let scroll_unchanged = self.scroll.offset() == rendered_offset;
        let mut changed = false;
        if let Some(first) = bounds.first()
            && scroll_unchanged
        {
            let origin = first.top() - self.scroll.bounds().top() - rendered_offset.y;
            changed |= (origin - self.viewport.origin).abs() > px(0.5);
            self.viewport.origin = origin;
        }
        let top = -rendered_offset.y - self.viewport.origin;
        let anchor = self.viewport.row_at(top);
        let old_y = self.viewport.prefix[anchor];
        for (measurement, bounds) in measured.iter().zip(bounds) {
            let Some((ix, gap)) = measurement else { continue };
            let height = (bounds.size.height - *gap).max(px(0.));
            let row = &mut self.viewport.rows[*ix];
            if (row.height - height).abs() > px(0.1) {
                row.height = height;
                changed = true;
            }
        }
        if changed {
            self.viewport.rebuild_prefix();
            if !revealing && scroll_unchanged && top >= px(0.) {
                let delta = self.viewport.prefix[anchor] - old_y;
                let offset = self.scroll.offset();
                self.scroll.set_offset(point(offset.x, (offset.y - delta).min(px(0.))));
            }
        }
        // A reveal or scroll clamp may have changed the handle during prepaint.
        // Always follow up with the newly visible range, without extending a pin
        // through the intervening document.
        if changed || !scroll_unchanged || self.viewport.height != self.scroll.bounds().size.height {
            cx.on_next_frame(window, |_, _, cx| cx.notify());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RowContent, RowKey, Viewport};
    use crate::editor::units;
    use aero_core::{BlockKind, Document, Edit};
    use gpui_kit::px;

    #[test]
    fn large_list_rows_keep_item_ids_numbering_empty_items_and_nested_units() {
        let source = format!(
            "Before.\n\n7. outer\n   - nested\n7. [ ] task\n7. \n\n^list\n\nAfter.\n\n{}",
            "Padding.\n\n".repeat(70)
        );
        let doc = Document::from_text(&source);
        let units = units::units(&doc);
        let mut viewport = Viewport::default();
        viewport.rebuild(&doc, &units, &[]);
        assert_eq!(viewport.rows.len(), 75);
        assert_eq!(&viewport.unit_rows[..6], [0, 1, 1, 2, 3, 4]);
        assert_eq!(viewport.rows[1].units, 1..3, "nested content stays in its outer item row");
        assert_eq!(viewport.rows[3].units, 4..5, "empty items own their zero-length unit");
        let list = &doc.blocks().roots[1];
        for i in 0..3 {
            assert_eq!(viewport.rows[i + 1].key, RowKey::Item(list.children[i].id));
            assert_eq!(
                viewport.rows[i + 1].content,
                RowContent::ListItem { root: 1, item: i, number: Some(7 + i as u32) }
            );
            assert_eq!(viewport.gap(i + 1), px(if i == 2 { 8. } else { 4. }));
        }
        assert_eq!(list.children[1].kind, BlockKind::ListItem { task: Some(false) });
        viewport.mount(px(5000.), px(100.), &[viewport.unit_rows[2]]);
        assert!(viewport.unit_ranges.contains(&(1..3)), "pinning a nested unit mounts only its item");
    }

    #[test]
    fn styled_list_roots_and_deep_item_subtrees_stay_atomic() {
        let styled =
            format!("7. first\n7. second\n\n{{.red .bg-blue key=value}}\n\n{}", "Padding.\n\n".repeat(70));
        let doc = Document::from_text(&styled);
        assert!(!doc.blocks().roots[0].attrs.is_empty());
        let mut viewport = Viewport::default();
        viewport.rebuild(&doc, &units::units(&doc), &[]);
        assert_eq!(viewport.rows[0].content, RowContent::Root(0));
        assert_eq!(viewport.rows[0].units, 0..2);

        let nested = format!("- outer\n{}- next\n", "  - nested\n".repeat(1000));
        let doc = Document::from_text(&nested);
        viewport.rebuild(&doc, &units::units(&doc), &[]);
        assert_eq!(viewport.rows.len(), 2, "never split inside an item container");
        assert_eq!(viewport.rows[0].units, 0..1001);
        assert_eq!(viewport.rows[1].content, RowContent::ListItem { root: 0, item: 1, number: None });
        assert_eq!(viewport.gap(1), px(0.), "no root gap after the document's last item");
    }

    #[test]
    fn measured_item_heights_follow_ids_across_insert_delete_and_reorder() {
        let source = (0..80).map(|i| format!("7. Item {i}.\n")).collect::<String>();
        let mut doc = Document::from_text(&source);
        let mut viewport = Viewport::default();
        viewport.rebuild(&doc, &units::units(&doc), &[]);
        assert_eq!(viewport.rows.len(), 80);
        for (i, row) in viewport.rows.iter_mut().enumerate() {
            row.height = px(40. + i as f32);
        }
        let heights: std::collections::HashMap<_, _> =
            viewport.rows.iter().map(|row| (row.key, row.height)).collect();
        let inserted = "7. New item.\n";
        for edit in [Edit::insert(0, inserted), Edit::delete(0..inserted.len())] {
            doc.edit(edit).unwrap();
            doc.reparse();
            viewport.rebuild(&doc, &units::units(&doc), &[]);
            for row in &viewport.rows {
                if let Some(height) = heights.get(&row.key) {
                    assert_eq!(&row.height, height);
                }
            }
        }
        let last = doc.blocks().roots[0].children.last().unwrap();
        let key = RowKey::Item(last.id);
        let mut range = doc.range_of(last);
        // Parser ranges exclude the trailing newline; a move must carry the whole line.
        if doc.slice(range.end..doc.len()).to_string().starts_with('\n') {
            range.end += 1;
        }
        let text = doc.slice(range.clone()).to_string();
        doc.edit_group(vec![Edit::delete(range), Edit::insert(0, text)]).unwrap();
        doc.reparse();
        viewport.rebuild(&doc, &units::units(&doc), &[]);
        assert_eq!(viewport.rows[0].key, key);
        for (i, row) in viewport.rows.iter().enumerate() {
            assert_eq!(row.height, heights[&row.key]);
            assert_eq!(row.content, RowContent::ListItem { root: 0, item: i, number: Some(7 + i as u32) });
            assert_eq!(viewport.prefix[i + 1] - viewport.prefix[i], row.height + viewport.gap(i));
        }
        assert_eq!(viewport.gap(0), px(4.));
        assert_eq!(viewport.gap(79), px(0.));
    }

    #[test]
    fn measured_root_heights_survive_edits() {
        let mut doc = Document::from_text("First paragraph.\n\nSecond paragraph.\n");
        let mut viewport = Viewport::default();
        viewport.rebuild(&doc, &units::units(&doc), &[]);
        viewport.rows[0].height = px(142.);
        viewport.rows[1].height = px(76.);
        doc.edit(Edit::insert(5, " edited")).unwrap();
        doc.reparse();
        viewport.rebuild(&doc, &units::units(&doc), &[]);
        assert_eq!(viewport.rows[0].height, px(142.));
        assert_eq!(viewport.rows[1].height, px(76.));
        assert_eq!(viewport.prefix, [px(0.), px(150.), px(226.)]);
    }

    #[test]
    fn nested_units_map_to_their_intact_root_and_markers_are_skipped() {
        let doc = Document::from_text("Before.\n\n^before\n\n- outer\n  - nested\n- next\n\nAfter.\n");
        let units = units::units(&doc);
        let mut viewport = Viewport::default();
        viewport.rebuild(&doc, &units, &[]);
        assert_eq!(viewport.rows.len(), 3);
        assert_eq!(viewport.rows[1].units, 1..4);
        assert_eq!(viewport.unit_rows, [0, 1, 1, 1, 2]);
        viewport.mount(px(0.), px(100.), &[2]);
        assert_eq!(viewport.unit_ranges.as_slice(), std::slice::from_ref(&(0..5)));
    }
}
