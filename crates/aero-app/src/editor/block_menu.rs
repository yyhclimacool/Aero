//! 块菜单（`ui_design.md` 5.3）：`⠿`、右键、`⌘/` 打开。颜色从这里进入 7.7 的颜色菜单。

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
}

pub(super) struct BlockMenu {
    block: BlockId,
    origin: Point<Pixels>,
    submenu: Option<Submenu>,
}

/// 正在拖的块。松手时按落点重排。
#[derive(Clone, Copy)]
pub(super) struct BlockDrag {
    pub id: BlockId,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) struct BlockDropHint {
    before: usize,
    parent: Option<usize>,
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
        self.block_menu = Some(BlockMenu { block, origin, submenu: None });
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
        Some(BlockDropHint { before, parent, origin: point(x, y), width: (bounds.right() - x).max(px(48.)) })
    }

    pub(super) fn finish_block_drag(
        &mut self,
        drag: &BlockDrag,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dragging_block = false;
        let cursor = window.mouse_position();
        let hint = self.block_drop_hint(cursor);
        self.block_drop = None;
        let Some(hint) = hint else {
            cx.notify();
            return;
        };
        let Some(from) = self.unit_index(drag.id) else {
            cx.notify();
            return;
        };
        let Some(plan) = ops::move_unit(
            &self.doc,
            &self.units,
            from,
            DropPlace { before: hint.before, parent: hint.parent },
        ) else {
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
        let Some(ix) = self.unit_index(id) else { return "块".into() };
        let text = self.doc.slice(self.units[ix].range.clone()).to_string();
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
        let Some(block) = self.doc.blocks().find(id) else { return };
        let offset = self.doc.content_range(block).start;
        self.block_menu = None;
        self.convert_at(offset, target, false, window, cx);
    }

    fn block_menu_color(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.block_menu = None;
        self.open_color_menu(window, cx);
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

    fn block_menu_duplicate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.block_menu.as_ref().map(|menu| menu.block) else { return };
        self.block_menu = None;
        let Some(ix) = self.unit_index(id) else { return };
        let range = self.units[ix].range.clone();
        let text = self.doc.slice(range.clone()).to_string();
        let at = range.end;
        let insert = if text.ends_with('\n') { text } else { format!("\n{text}") };
        if let Err(err) = self.doc.edit(aero_core::Edit::insert(at, insert)) {
            log::warn!("复制块失败: {err}");
            return;
        }
        self.after_doc_change(cx);
        self.focus_at(at + 1, None, window, cx);
    }

    fn block_menu_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.block_menu.as_ref().map(|menu| menu.block) else { return };
        self.block_menu = None;
        if let Some(ix) = self.unit_index(id) {
            self.select_blocks(ix, ix, window, cx);
            self.delete_selected(window, cx);
        }
    }

    fn block_menu_width(&mut self, width: Option<u32>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.block_menu.as_ref().map(|menu| menu.block) else { return };
        self.set_image_width(id, width, window, cx);
        self.block_menu = None;
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
                        match action {
                            "convert" => {
                                if let Some(menu) = &mut this.block_menu {
                                    menu.submenu = Some(Submenu::Convert);
                                }
                                cx.notify();
                            }
                            "color" => this.block_menu_color(window, cx),
                            "source" => {
                                let Some(id) = this.block_menu.as_ref().map(|menu| menu.block) else {
                                    return;
                                };
                                this.block_menu = None;
                                this.toggle_block_source(id, window, cx);
                            }
                            "link" => this.block_menu_copy_link(window, cx),
                            "id" => this.block_menu_add_id(window, cx),
                            "duplicate" => this.block_menu_duplicate(window, cx),
                            "delete" => this.block_menu_delete(window, cx),
                            "small" => this.block_menu_width(Some(preview::IMAGE_WIDTH_SMALL), window, cx),
                            "medium" => this.block_menu_width(Some(preview::IMAGE_WIDTH_MEDIUM), window, cx),
                            "large" => this.block_menu_width(Some(preview::IMAGE_WIDTH_LARGE), window, cx),
                            "original" => this.block_menu_width(None, window, cx),
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
        rows.extend([
            row("block-menu-link", "复制块链接".into(), "link"),
            row("block-menu-id", if has_id.is_some() { "复制 ^id".into() } else { "添加 ^id".into() }, "id"),
            row("block-menu-duplicate", "复制块".into(), "duplicate"),
            row("block-menu-delete", "删除".into(), "delete"),
        ]);
        if image {
            rows.push(row("block-menu-width-small", "宽度 · 小".into(), "small"));
            rows.push(row("block-menu-width-medium", "宽度 · 中".into(), "medium"));
            rows.push(row("block-menu-width-large", "宽度 · 大".into(), "large"));
            rows.push(row("block-menu-width-original", "宽度 · 原始".into(), "original"));
        }
        let convert = menu.submenu == Some(Submenu::Convert);
        Some(
            deferred(
                anchored().position(menu.origin).snap_to_window_with_margin(px(8.)).child(
                    h_flex()
                        .items_start()
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
                        .when(convert, |el| el.child(self.render_block_convert_menu(cx))),
                ),
            )
            .into_any_element(),
        )
    }

    fn render_block_convert_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let hover = cx.theme().list_hover;
        let border = cx.theme().border;
        let popover = cx.theme().popover;
        let rows = slash::ITEMS.iter().filter_map(|item| {
            let SlashAction::Convert(target) = item.action else { return None };
            let label = item.label.to_owned();
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
                    .child(label)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
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
