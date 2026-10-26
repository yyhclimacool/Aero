//! 块菜单：右键、`⌘/` 打开，点击外部关闭。颜色从这里进入颜色菜单。

use aero_core::BlockId;
use gpui_kit::component::WindowExt as _;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::ops::{BlockType, DropPlace};
use super::slash::{self, SlashAction};
use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Submenu {
    Convert,
    Layout,
}

pub(super) struct BlockMenu {
    pub(super) block: BlockId,
    revision: u64,
    origin: Point<Pixels>,
    submenu: Option<Submenu>,
}

/// 正在拖的块。松手时按落点重排。
#[derive(Clone, Copy)]
pub(super) struct BlockDrag {
    pub id: BlockId,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct BlockDropHint {
    before: usize,
    parent: Option<usize>,
    node: Option<(BlockId, bool, bool)>,
    receiver: Option<Bounds<Pixels>>,
    origin: Point<Pixels>,
    width: Pixels,
}

pub(super) struct BlockDragGhost {
    text: SharedString,
}

impl BlockDragGhost {
    pub(super) fn new(text: SharedString) -> Self {
        Self { text }
    }
}

impl Render for BlockDragGhost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded(px(6.))
            .shadow_md()
            .bg(cx.theme().popover.opacity(0.92))
            .text_sm()
            .opacity(0.9)
            .child(self.text.clone())
    }
}

fn unwrap_reason(doc: &aero_core::Document, id: BlockId) -> Option<&'static str> {
    let block = doc.blocks().find(id)?;
    (block.persisted_id.is_some() || aero_core::block_refs::aliases(block).next().is_some())
        .then_some("此容器有引用身份，解除会破坏引用范围")
}

fn image_only(source: &str) -> bool {
    if preview::is_image_embed(source) { return true; }
    let analysis = aero_core::markup::analyze(source);
    let images: Vec<_> = analysis.conceal.iter().filter(|open| source[open.start..].starts_with("!["))
        .filter_map(|open| {
            analysis.conceal.iter().find(|close| close.start >= open.end
                && source[close.start..].starts_with(']')
                && (open.end == close.start || analysis.links.iter().any(|(label, _)| *label == (open.end..close.start))))
                .map(|close| open.start..close.end)
        }).collect();
    !images.is_empty() && source.char_indices().all(|(at, ch)| ch.is_whitespace() || images.iter().any(|range| range.contains(&at)))
}

fn layout_reason(doc: &aero_core::Document, id: BlockId, key: &str, value: Option<&str>, width: f32) -> Option<&'static str> {
    let Some(block) = doc.blocks().find(id) else { return Some("目标块已不存在") };
    if key == "indent-step" && matches!(block.kind, BlockKind::ListItem { .. }) {
        return if value == Some("1") {
            ops::list_indent(doc, id).is_none().then_some("没有可接收此项的前一列表项")
        } else {
            ops::list_outdent(doc, id).is_none().then_some("无法无损降低此项层级")
        };
    }
    let prose = |b: &aero_core::Block| {
        let source = doc.slice(doc.content_range(b)).to_string();
        matches!(b.kind, BlockKind::Paragraph | BlockKind::Heading { .. })
            && !image_only(source.trim()) && !preview::is_note_embed(source.trim())
    };
    let supported = prose(block) || (key == "align" && matches!(block.kind, BlockKind::ListItem { .. })
        && (block.children.is_empty() || block.children.iter().any(prose)));
    if !supported { return Some("此块没有可设置对齐或视觉缩进的正文") }
    if key == "indent-step" {
        let current = aero_core::properties::indent(&block.attrs);
        if value == Some("1") {
            if current >= 8 { return Some("视觉缩进最多 8 级") }
            if width - f32::from(current + 1) * 24. < 160. { return Some("缩进后可编辑宽度不足 160 pt") }
        } else if current == 0 { return Some("已是默认视觉缩进") }
    }
    None
}

impl Editor {
    fn block_layout_width(&self, id: BlockId) -> f32 {
        let indent = self.doc.blocks().find(id).map_or(0., |b| f32::from(aero_core::properties::indent(&b.attrs)) * 24.);
        if let Some((_, bounds)) = self.unit_bounds.iter().find(|(block, _)| *block == id) {
            return f32::from(bounds.size.width) + indent;
        }
        let insets: f32 = aero_core::containers::ancestors(&self.doc, id).iter().map(|b| {
            let padding = if aero_core::containers::is_editable(&b.kind) { 34. }
                else if matches!(b.kind, BlockKind::ListItem { .. }) { 28. } else { 0. };
            padding + f32::from(aero_core::properties::indent(&b.attrs)) * 24.
        }).sum();
        (f32::from(self.scroll.bounds().size.width) - GUTTER - 48.).min(COLUMN_WIDTH) - insets
    }

    pub(super) fn in_block_handle(&self, id: BlockId, cursor: Point<Pixels>) -> bool {
        self.unit_bounds.iter().find(|(block, _)| *block == id).is_some_and(|(_, bounds)| {
            let reach = self.gutter_reaches.get(&id).copied().unwrap_or(px(GUTTER));
            let left = bounds.left() - reach;
            let list = self.doc.blocks().find(id).is_some_and(|b| matches!(b.kind, BlockKind::List { .. }));
            cursor.x >= left
                && cursor.x < left + px(if list { 20. } else { GUTTER })
                && cursor.y >= bounds.top() + px(4.)
                && cursor.y < bounds.top() + px(if list { 48. } else { 24. })
        })
    }

    pub(super) fn in_block_operation_area(&self, id: BlockId, cursor: Point<Pixels>) -> bool {
        self.unit_bounds.iter().find(|(block, _)| *block == id).is_some_and(|(_, bounds)| {
            let reach = self.gutter_reaches.get(&id).copied().unwrap_or(px(GUTTER));
            let list = self.doc.blocks().find(id).is_some_and(|b| matches!(b.kind, BlockKind::List { .. }));
            let bottom = if list { bounds.bottom().max(bounds.top() + px(48.)) } else { bounds.bottom() };
            cursor.x >= bounds.left() - reach
                && cursor.x <= bounds.right()
                && cursor.y >= bounds.top()
                && cursor.y <= bottom
        })
    }

    pub(super) fn open_block_menu_for_current(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.color_menu.is_some() {
            return self.close_color_menu(window, cx);
        }
        if self.block_menu.is_some() {
            self.block_menu = None;
            cx.notify();
            return true;
        }
        let id = self.focused_block_id().or_else(|| self.selection_head());
        let Some(id) = id else { return false };
        self.open_block_menu(id, point(px(16.), px(48.)), window, cx);
        true
    }

    pub(super) fn open_block_menu(
        &mut self,
        block: BlockId,
        origin: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = self.close_color_menu(window, cx);
        self.slash = None;
        self.ensure_current_parse();
        self.block_menu = Some(BlockMenu { block, revision: self.doc.version(), origin, submenu: None });
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    pub(super) fn close_block_menu(&mut self, cx: &mut Context<Self>) -> bool {
        if self.block_menu.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }

    pub(super) fn remember_unit_bounds(&mut self, id: BlockId, bounds: Bounds<Pixels>) {
        if let Some((_, stored)) = self.unit_bounds.iter_mut().find(|(stored_id, _)| *stored_id == id) {
            *stored = bounds;
        } else {
            self.unit_bounds.push((id, bounds));
        }
    }

    /// 拖拽刚越过阈值时记下落点。此时活动拖拽还没登记，不能 `notify`，否则这一帧会把提示清掉。
    pub(super) fn preview_block_drag(&mut self, cursor: Point<Pixels>) {
        self.dragging_block = true;
        self.block_drop = self.block_drop_hint(cursor);
    }

    pub(super) fn track_block_drag(&mut self, cursor: Point<Pixels>, cx: &mut Context<Self>) {
        if !self.dragging_block {
            return;
        }
        let hint = self.block_drop_hint(cursor);
        if hint != self.block_drop {
            self.block_drop = hint;
            cx.notify();
        }
    }

    fn block_drop_hint(&self, cursor: Point<Pixels>) -> Option<BlockDropHint> {
        // Hit the deepest visible tree boundary at this x/y. A container's
        // inner gutter means insert inside; its outer edge means a sibling.
        let node = self
            .unit_bounds
            .iter()
            .filter_map(|(id, bounds)| {
                let block = self.doc.blocks().find(*id)?;
                if cursor.y < bounds.top() || cursor.y > bounds.bottom() {
                    return None;
                }
                if cursor.x < bounds.left() && aero_core::containers::ancestors(&self.doc, *id).iter().any(|ancestor| {
                    aero_core::containers::is_editable(&ancestor.kind) && self.unit_bounds.iter().any(|(parent, area)| {
                        *parent == ancestor.id && cursor.x >= area.left() && cursor.y >= area.top() && cursor.y <= area.bottom()
                    })
                }) { return None; }
                let container = aero_core::containers::is_editable(&block.kind);
                let structural = container
                    || matches!(
                        block.kind,
                        aero_core::BlockKind::List { .. } | aero_core::BlockKind::ListItem { .. }
                    );
                if cursor.x < bounds.left() - px(if structural { 96. } else { 48. }) {
                    return None;
                }
                Some((*id, *bounds, container, aero_core::containers::ancestors(&self.doc, *id).len()))
            })
            .max_by_key(|(_, _, _, depth)| *depth);
        if let Some((id, bounds, container, _)) = node {
            let into = container
                && cursor.x >= bounds.left()
                && cursor.y > bounds.top() + px(8.)
                && cursor.y < bounds.bottom() - px(8.);
            let after = cursor.y >= bounds.top() + bounds.size.height / 2.;
            let ordinary = !container && self.unit_index(id).is_some()
                && !aero_core::containers::ancestors(&self.doc, id)
                    .iter()
                    .any(|b| aero_core::containers::is_editable(&b.kind));
            if !ordinary {
                return Some(BlockDropHint {
                    before: 0,
                    parent: None,
                    node: Some((id, after, into)),
                    receiver: into.then_some(bounds),
                    origin: point(
                        bounds.left() + if into { px(16.) } else { px(0.) },
                        if after || into { bounds.bottom() } else { bounds.top() },
                    ),
                    width: (bounds.size.width - if into { px(16.) } else { px(0.) }).max(px(48.)),
                });
            }
        }
        let mut hovered: Option<(usize, Bounds<Pixels>)> = None;
        let mut nearest: Option<(usize, Bounds<Pixels>, Pixels)> = None;
        for (ix, unit) in self.units.iter().enumerate() {
            let Some(bounds) =
                self.unit_bounds.iter().find(|(id, _)| *id == unit.id).map(|(_, bounds)| *bounds)
            else {
                continue;
            };
            if cursor.y >= bounds.top() && cursor.y <= bounds.bottom() {
                hovered = Some((ix, bounds));
                break;
            }
            let distance =
                if cursor.y < bounds.top() { bounds.top() - cursor.y } else { cursor.y - bounds.bottom() };
            if nearest.as_ref().is_none_or(|(_, _, best)| distance < *best) {
                nearest = Some((ix, bounds, distance));
            }
        }
        let (ix, bounds) = hovered.or_else(|| nearest.map(|(ix, bounds, _)| (ix, bounds)))?;
        let before = if cursor.y < bounds.top() + bounds.size.height / 2. { ix } else { ix + 1 };
        let nest = self.units[ix].list.is_some() && cursor.x >= bounds.right() - px(24.);
        let parent = nest.then_some(ix);
        let y = if nest || before > ix { bounds.bottom() } else { bounds.top() };
        let x = if nest { bounds.left() + px(24.) } else { bounds.left() };
        Some(BlockDropHint {
            before,
            parent,
            node: None,
            receiver: nest.then(|| {
                ops::list_item_for(&self.doc, self.units[ix].id)
                    .and_then(|id| self.unit_bounds.iter().find(|(block, _)| *block == id).map(|(_, bounds)| *bounds))
                    .unwrap_or(bounds)
            }),
            origin: point(x, y),
            width: (bounds.right() - x).max(px(48.)),
        })
    }

    pub(super) fn finish_block_drag(
        &mut self,
        drag: &BlockDrag,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ensure_current_parse();
        self.dragging_block = false;
        if drag.revision != self.doc.version() {
            self.block_drop = None;
            cx.notify();
            return;
        }
        let cursor = window.mouse_position();
        let hint = self.block_drop_hint(cursor);
        self.block_drop = None;
        let Some(hint) = hint else {
            cx.notify();
            return;
        };
        let plan = if let Some((to, after, into)) = hint.node {
            ops::move_node(&self.doc, drag.id, to, after, into)
        } else if let Some(parent) =
            hint.parent.filter(|_| self.unit_index(drag.id).is_none_or(|ix| self.units[ix].list.is_some()))
        {
            self.units.get(parent).and_then(|to| {
                let target = aero_core::containers::ancestors(&self.doc, to.id)
                    .into_iter()
                    .rev()
                    .find(|b| matches!(b.kind, BlockKind::ListItem { .. }))?
                    .id;
                ops::move_node(&self.doc, drag.id, target, false, true)
            })
        } else if let Some(from) = self.unit_index(drag.id) {
            if aero_core::containers::ancestors(&self.doc, drag.id).iter().any(|b| {
                aero_core::containers::is_editable(&b.kind) || matches!(b.kind, BlockKind::List { .. })
            }) {
                let index = hint.before.min(self.units.len().saturating_sub(1));
                self.units.get(index).and_then(|to| {
                    ops::move_node(&self.doc, drag.id, to.id, hint.before >= self.units.len(), false)
                })
            } else {
                ops::move_unit(
                    &self.doc,
                    &self.units,
                    from,
                    DropPlace { before: hint.before, parent: hint.parent },
                )
            }
        } else {
            let index = hint.before.min(self.units.len().saturating_sub(1));
            self.units.get(index).and_then(|to| {
                ops::move_node(&self.doc, drag.id, to.id, hint.before >= self.units.len(), false)
            })
        };
        let Some(plan) = plan else {
            cx.notify();
            return;
        };
        if let Err(err) = self.doc.edit(plan.edit) {
            log::warn!("拖拽重排失败: {err}");
            cx.notify();
            return;
        }
        self.focused = None;
        self.block_menu = None;
        self.after_doc_change(cx);
        if let Some(ix) = super::units::unit_at(&self.units, plan.focus_offset) {
            let id = self.units[ix].id;
            self.select_blocks(ix, ix, window, cx);
            self.pulse_block(id, cx);
        }
    }

    pub(super) fn block_preview_text(&self, id: BlockId) -> SharedString {
        let Some(block) = self.doc.blocks().find(id) else { return "块".into() };
        let text = self.doc.slice(self.doc.content_range(block)).to_string();
        let line = text.lines().find(|line| !line.trim().is_empty()).unwrap_or("块");
        let line = line.trim();
        if line.chars().count() > 32 {
            format!("{}…", line.chars().take(32).collect::<String>()).into()
        } else if line.is_empty() {
            "块".into()
        } else {
            line.to_owned().into()
        }
    }

    pub(super) fn render_block_drop_line(&self, cx: &App) -> Option<AnyElement> {
        let hint = self.block_drop?;
        let color = cx.theme().accent;
        if let Some(bounds) = hint.receiver {
            return Some(deferred(anchored().position(bounds.origin).child(
                div().debug_selector(|| "block-drop-receiver".into())
                    .w(bounds.size.width).h(bounds.size.height).border_2().border_color(color)
                    .bg(color.opacity(0.12)).rounded(px(6.)),
            )).into_any_element());
        }
        Some(
            deferred(
                anchored().position(hint.origin).child(
                    div().debug_selector(|| "block-drop-line".into()).w(hint.width).h(px(2.)).bg(color),
                ),
            )
            .into_any_element(),
        )
    }

    fn block_menu_convert(&mut self, target: BlockType, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.block_menu.as_ref() else { return };
        let id = menu.block;
        if menu.revision != self.doc.version() {
            self.block_menu = None;
            return;
        }
        let plan = ops::convert_node(&self.doc, id, target);
        self.block_menu = None;
        match plan {
            Ok(Some(plan)) => self.apply_plan(plan, window, cx),
            Err(error) => {
                window.push_notification(gpui_kit::component::notification::Notification::warning(error), cx)
            }
            _ => {}
        }
    }

    fn block_menu_color(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.block_menu.as_ref().map(|m| m.block) else { return };
        self.block_menu = None;
        self.open_color_menu_for_blocks(vec![id], window, cx);
    }

    fn block_menu_copy_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.block_menu.as_ref().map(|menu| menu.block) else { return };
        self.copy_block_link_for(id, window, cx);
        self.block_menu = None;
        cx.notify();
    }

    fn block_menu_add_id(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.block_menu.as_ref().map(|menu| menu.block) else { return };
        match self.ensure_id_for(id, window, cx) {
            Ok(block_id) => cx.write_to_clipboard(ClipboardItem::new_string(format!("^{block_id}"))),
            Err(error) => window.push_notification(
                gpui_kit::component::notification::Notification::error(error).title("添加块 ID"),
                cx,
            ),
        }
        self.block_menu = None;
        cx.notify();
    }

    fn block_menu_copy(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.block_menu.as_ref().map(|menu| menu.block) else { return };
        self.block_menu = None;
        let Some(range) = aero_core::properties::owned_range(&self.doc, id) else { return };
        let text = aero_core::containers::standalone(&self.doc, range);
        self.copy_blocks(text, cx);
        cx.notify();
    }

    fn block_menu_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.block_menu.as_ref().map(|menu| menu.block) else { return };
        self.block_menu = None;
        let Some(range) = aero_core::properties::owned_range(&self.doc, id) else { return };
        let at = range.start;
        self.structural_edit(aero_core::Edit::delete(range), at, window, cx);
    }

    fn block_menu_cut(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.block_menu.take() else { return };
        if menu.revision != self.doc.version() {
            return;
        }
        if let Some(range) = aero_core::properties::owned_range(&self.doc, menu.block) {
            self.cut_blocks(range, window, cx);
        }
    }

    fn block_menu_property(
        &mut self,
        key: &str,
        value: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.block_menu.take() else { return };
        if menu.revision != self.doc.version() {
            return;
        }
        if layout_reason(&self.doc, menu.block, key, value, self.block_layout_width(menu.block)).is_some() {
            return;
        }
        let Some(block) = self.doc.blocks().find(menu.block) else { return };
        if key == "indent-step" && matches!(block.kind, BlockKind::ListItem { .. }) {
            let plan = if value == Some("1") { ops::list_indent(&self.doc, menu.block) }
                else { ops::list_outdent(&self.doc, menu.block) };
            if let Some(plan) = plan { self.apply_plan(plan, window, cx); }
            return;
        }
        let mut attrs = block.attrs.clone();
        if key == "indent-step" {
            let current = aero_core::properties::indent(&attrs) as i32;
            let next = (current + value.and_then(|v| v.parse::<i32>().ok()).unwrap_or(0)).clamp(0, 8);
            aero_core::properties::set(&mut attrs, "indent", (next > 0).then(|| next.to_string()).as_deref());
        } else {
            aero_core::properties::set(&mut attrs, key, value);
        }
        if let Ok(Some(edit)) =
            aero_core::properties::edit(&self.doc, menu.block, &attrs, block.persisted_id.as_deref())
        {
            let at = self.doc.content_range(block).start;
            self.structural_edit(edit, at, window, cx);
        }
    }

    fn block_menu_container(&mut self, unwrap: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.block_menu.take() else { return };
        if menu.revision != self.doc.version() {
            return;
        }
        if unwrap && unwrap_reason(&self.doc, menu.block).is_some() {
            return;
        }
        let edit = if unwrap {
            aero_core::containers::unwrap(&self.doc, menu.block)
        } else {
            aero_core::containers::wrap(&self.doc, menu.block, true)
        };
        match edit {
            Ok(edit) => {
                let at = edit.range.start;
                self.structural_edit(edit, at, window, cx);
            }
            Err(error) => {
                window.push_notification(gpui_kit::component::notification::Notification::error(error), cx)
            }
        }
    }

    pub(super) fn render_block_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.block_menu.as_ref()?;
        let theme = cx.theme().clone();
        let block = menu.block;
        let has_id = self.doc.blocks().find(block).and_then(|b| b.persisted_id.clone());
        let image = self
            .doc
            .blocks()
            .find(block)
            .map(|b| self.doc.slice(self.doc.content_range(b)).to_string())
            .is_some_and(|text| preview::is_image_embed(text.trim()));
        let prose = self.doc.blocks().find(block).is_some_and(|b| {
            matches!(b.kind, BlockKind::Paragraph | BlockKind::Heading { .. } | BlockKind::BlockQuote)
                && !image
        });
        let row = |id: &'static str, label: String, action: &'static str| {
            div()
                .id(id)
                .debug_selector(move || id.to_owned())
                .w_full()
                .px_2()
                .py_1()
                .rounded(px(4.))
                .cursor_pointer()
                .hover(|style| style.bg(theme.list_hover))
                .text_sm()
                .child(label)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        if this.block_menu.as_ref().is_some_and(|m| m.revision != this.doc.version()) {
                            this.close_block_menu(cx);
                            return;
                        }
                        match action {
                            "convert" => {
                                if let Some(menu) = &mut this.block_menu {
                                    menu.submenu = Some(Submenu::Convert);
                                }
                                cx.notify();
                            }
                            "color" => this.block_menu_color(window, cx),
                            "cut" => this.block_menu_cut(window, cx),
                            "layout" => {
                                if let Some(menu) = &mut this.block_menu {
                                    menu.submenu = Some(Submenu::Layout);
                                }
                                cx.notify();
                            }
                            "highlight" => this.block_menu_container(false, window, cx),
                            "unwrap" => this.block_menu_container(true, window, cx),
                            "source" => {
                                let Some(id) = this.block_menu.as_ref().map(|menu| menu.block) else {
                                    return;
                                };
                                this.block_menu = None;
                                this.toggle_block_source(id, window, cx);
                            }
                            "link" => this.block_menu_copy_link(window, cx),
                            "id" => this.block_menu_add_id(window, cx),
                            "copy" => this.block_menu_copy(window, cx),
                            "sync-copy" => {
                                if let Some(id) = this.block_menu.as_ref().map(|m| m.block) {
                                    this.copy_sync_for(id, window, cx);
                                }
                            }
                            "sync-insert" | "sync-replace" => {
                                if let Some(id) = this.block_menu.as_ref().map(|m| m.block)
                                    && let Some(range) = aero_core::properties::owned_range(&this.doc, id)
                                {
                                    this.open_sync_picker(
                                        if action == "sync-replace" {
                                            range.clone()
                                        } else {
                                            range.end..range.end
                                        },
                                        action == "sync-replace",
                                        window,
                                        cx,
                                    );
                                }
                            }
                            "delete" => this.block_menu_delete(window, cx),
                            _ => {}
                        }
                    }),
                )
        };
        let mut rows = vec![
            row("block-menu-convert", "转换为 ▸".into(), "convert"),
            row("block-menu-color", "颜色 ▸".into(), "color"),
        ];
        if prose {
            let label = if self.block_shows_source(block) { "完成源码" } else { "编辑源码" };
            rows.push(row("block-menu-source", label.into(), "source"));
        }
        rows.push(row("block-menu-layout", "对齐与缩进 ▸".into(), "layout"));
        rows.push(row("block-menu-highlight", "包裹为高亮块".into(), "highlight"));
        if self.doc.blocks().find(block).is_some_and(|b| aero_core::containers::is_editable(&b.kind)) {
            let reason = unwrap_reason(&self.doc, block);
            rows.push(row("block-menu-unwrap", "解除容器".into(), if reason.is_some() { "disabled" } else { "unwrap" })
                .when_some(reason, |el, reason| el.opacity(0.4).cursor_default().tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(reason).build(window, cx)
                })));
        }
        rows.extend([
            row("block-menu-link", "复制块链接".into(), "link"),
            row("block-menu-id", if has_id.is_some() { "复制 ^id".into() } else { "添加 ^id".into() }, "id"),
            row("block-menu-copy", "复制块".into(), "copy"),
            row("block-menu-cut", "剪切块".into(), "cut"),
            row("block-menu-sync-copy", "复制同步引用".into(), "sync-copy"),
            row("block-menu-sync-insert", "插入同步引用…".into(), "sync-insert"),
            row("block-menu-delete", "删除".into(), "delete"),
        ]);
        let convert = menu.submenu == Some(Submenu::Convert);
        if self.doc.blocks().find(block).is_some_and(|b| {
            preview::is_note_embed(self.doc.slice(self.doc.content_range(b)).to_string().trim())
        }) {
            rows.push(row("block-menu-sync-replace", "更换同步来源…".into(), "sync-replace"));
        }
        let layout = menu.submenu == Some(Submenu::Layout);
        Some(
            deferred(
                anchored().position(menu.origin).snap_to_window_with_margin(px(8.)).child(
                    h_flex()
                        .items_start()
                        .capture_any_mouse_down(cx.listener(|this, _, _, cx| {
                            this.close_block_menu(cx);
                        }))
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.close_block_menu(cx);
                        }))
                        .child(
                            v_flex()
                                .id("block-menu")
                                .debug_selector(|| "block-menu".into())
                                .occlude()
                                .w(px(180.))
                                .p_1()
                                .gap(px(2.))
                                .bg(theme.popover)
                                .text_color(theme.popover_foreground)
                                .border_1()
                                .border_color(theme.border)
                                .rounded(px(8.))
                                .shadow_lg()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .children(rows),
                        )
                        .when(convert, |el| el.child(self.render_block_convert_menu(cx)))
                        .when(layout, |el| el.child(self.render_block_layout_menu(cx))),
                ),
            )
            .into_any_element(),
        )
    }

    fn render_block_layout_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let block = self.block_menu.as_ref().unwrap().block;
        let list = self.doc.blocks().find(block).is_some_and(|b| matches!(b.kind, BlockKind::ListItem { .. }));
        let width = self.block_layout_width(block);
        let rows = [
            ("左对齐", "align", Some("left")),
            ("居中", "align", Some("center")),
            ("右对齐", "align", Some("right")),
            ("两端对齐", "align", Some("justify")),
            (if list { "增加列表层级" } else { "增加视觉缩进" }, "indent-step", Some("1")),
            (if list { "减少列表层级" } else { "减少视觉缩进" }, "indent-step", Some("-1")),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (label, key, value))| {
            let reason = layout_reason(&self.doc, block, key, value, width);
            div()
                .id(("block-layout", i))
                .debug_selector(move || format!("block-layout-{i}"))
                .px_2()
                .py_1()
                .text_sm()
                .rounded(px(4.))
                .opacity(if reason.is_none() { 1. } else { 0.4 })
                .when_some(reason, |el, reason| el.tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(reason).build(window, cx)
                }))
                .child(label)
                .when(reason.is_none(), |el| {
                    el.cursor_pointer().hover(|s| s.bg(cx.theme().list_hover)).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.block_menu_property(key, value, window, cx);
                        }),
                    )
                })
        });
        v_flex()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .w(px(180.))
            .p_1()
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .rounded(px(8.))
            .shadow_lg()
            .children(rows)
            .into_any_element()
    }

    fn render_block_convert_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let hover = cx.theme().list_hover;
        let border = cx.theme().border;
        let popover = cx.theme().popover;
        let rows = slash::ITEMS.iter().filter_map(|item| {
            let SlashAction::Convert(target) = item.action else { return None };
            let label = item.label.to_owned();
            let reason =
                self.block_menu.as_ref().and_then(|m| ops::conversion_reason(&self.doc, m.block, target));
            Some(
                div()
                    .id(SharedString::from(format!("block-menu-type-{label}")))
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .text_sm()
                    .opacity(if reason.is_some() { 0.4 } else { 1. })
                    .when_some(reason, |el, reason| {
                        el.tooltip(move |window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(reason).build(window, cx)
                        })
                    })
                    .child(label)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            if reason.is_some() {
                                return;
                            }
                            this.block_menu_convert(target, window, cx);
                        }),
                    ),
            )
        });
        v_flex()
            .debug_selector(|| "block-menu-convert-list".into())
            .occlude()
            .w(px(160.))
            .ml_1()
            .p_1()
            .bg(popover)
            .border_1()
            .border_color(border)
            .rounded(px(8.))
            .shadow_lg()
            .children(rows)
            .into_any_element()
    }
}

#[cfg(test)]
mod interaction_tests {
    use aero_core::{BlockKind, Document};
    use super::{layout_reason, unwrap_reason};

    #[test]
    fn phase2_visual_indent_and_alignment_guards() {
        let doc = Document::from_text("text {indent=7}\n");
        let id = doc.blocks().roots[0].id;
        assert!(layout_reason(&doc, id, "indent-step", Some("1"), 352.).is_none());
        assert!(layout_reason(&doc, id, "indent-step", Some("1"), 351.).is_some());
        assert!(layout_reason(&doc, id, "indent-step", Some("-1"), 180.).is_none());
        for source in ["text {indent=8}\n", "text {indent=999}\n"] {
            let doc = Document::from_text(source);
            assert!(layout_reason(&doc, doc.blocks().roots[0].id, "indent-step", Some("1"), 900.).is_some());
            assert_eq!(doc.to_string(), source);
        }
        for source in ["![x](pic.png)\n", "![[note]]\n", ":::highlight\ntext\n:::\n", "> text\n", "```rs\nx\n```\n"] {
            let doc = Document::from_text(source);
            assert!(layout_reason(&doc, doc.blocks().roots[0].id, "align", Some("center"), 720.).is_some(), "{source}");
        }
        for source in ["text\n", "###### title\n", "- body\n"] {
            let doc = Document::from_text(source);
            let block = doc.blocks().iter().find(|(_, b)| !matches!(b.kind, BlockKind::List { .. })).unwrap().1;
            assert!(layout_reason(&doc, block.id, "align", Some("justify"), 720.).is_none());
        }
    }

    #[test]
    fn phase2_unwrap_uses_persisted_identity_not_backlinks() {
        for source in [":::highlight\nbody\n:::\n", "> body\n"] {
            let doc = Document::from_text(source);
            assert!(unwrap_reason(&doc, doc.blocks().roots[0].id).is_none());
        }
        for source in [":::highlight\nbody\n:::\n\n^box\n", ":::highlight\nbody\n:::\n\n{aero-aliases=old}\n"] {
            let doc = Document::from_text(source);
            assert!(unwrap_reason(&doc, doc.blocks().roots[0].id).is_some());
        }
    }
}
