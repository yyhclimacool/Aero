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

#[derive(Clone, Copy, PartialEq)]
pub(super) struct BlockDropHint {
    before: usize,
    parent: Option<usize>,
    node: Option<(BlockId, bool, bool)>,
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

impl Editor {
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
            let ordinary = self.unit_index(id).is_some()
                && !aero_core::containers::ancestors(&self.doc, id)
                    .iter()
                    .any(|b| aero_core::containers::is_editable(&b.kind));
            if !ordinary {
                return Some(BlockDropHint {
                    before: 0,
                    parent: None,
                    node: Some((id, after, into)),
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
        } else if let Some(from) = self.unit_index(drag.id) {
            ops::move_unit(
                &self.doc,
                &self.units,
                from,
                DropPlace { before: hint.before, parent: hint.parent },
            )
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
        let Some(block) = self.doc.blocks().find(menu.block) else { return };
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
            rows.push(row("block-menu-unwrap", "解除容器".into(), "unwrap"));
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
        let supported =
            self.block_menu.as_ref().and_then(|m| self.doc.blocks().find(m.block)).is_some_and(|b| {
                matches!(
                    b.kind,
                    aero_core::BlockKind::Paragraph
                        | aero_core::BlockKind::Heading { .. }
                        | aero_core::BlockKind::ListItem { .. }
                )
            });
        let rows = [
            ("左对齐", "align", Some("left")),
            ("居中", "align", Some("center")),
            ("右对齐", "align", Some("right")),
            ("两端对齐", "align", Some("justify")),
            ("增加视觉缩进", "indent-step", Some("1")),
            ("减少视觉缩进", "indent-step", Some("-1")),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (label, key, value))| {
            div()
                .id(("block-layout", i))
                .px_2()
                .py_1()
                .text_sm()
                .rounded(px(4.))
                .opacity(if supported { 1. } else { 0.4 })
                .child(label)
                .when(supported, |el| {
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
