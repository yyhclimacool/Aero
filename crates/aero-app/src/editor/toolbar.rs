//! 选区浮动工具条（`ui_design.md` 5.9）：编辑态里选出一段文字后，选区上方浮出的小条。
//!
//! 它是**发现路径**而不是唯一路径——每一项都有等价快捷键（14 节），所以这里只负责出现 / 消失的时机、
//! 按下态，以及把点击转成已有的那几条命令。`A ▾` 打开 7.7 的颜色菜单；`转换 ▾` 打开与 `/` 菜单
//! 同一套块类型。`⋯` 复制块链接，或给当前块补上 `^id`。

use std::ops::Range;
use std::time::Duration;

use aero_core::colors::{self, Slot};
use aero_core::inline_format::Format;
use gpui_kit::component::WindowExt as _;

use super::ops::BlockType;
use super::slash::SlashAction;
use super::*;

/// 键盘改选区后等多久才出现（5.9）。鼠标松手立刻出现，不等它。
const DELAY: Duration = Duration::from_millis(300);
/// 工具条与选区之间留的空，以及它自己的高度（放不下就翻到选区下方）。
const GAP: Pixels = px(8.);
const HEIGHT: Pixels = px(32.);

/// 工具条上挂着的子菜单。开着时工具条本身不消失（5.9）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Submenu {
    Convert,
    More,
}

#[derive(Clone, Copy)]
pub(super) enum IdentityCopy {
    Link,
    Sync,
    Id,
}

impl IdentityCopy {
    fn title(self) -> &'static str {
        match self {
            Self::Link => "复制块链接",
            Self::Sync => "复制同步引用",
            Self::Id => "添加块 ID",
        }
    }
}

/// 工具条态。位置每帧按选区重算，所以这里只记"为哪个选区出现的"。
pub(super) struct Toolbar {
    /// 控件内的选区；选区一变就重新计时
    selection: Range<usize>,
    /// 选区已经带着的格式，用来画按下态
    active: Vec<Format>,
    /// 选区带着文字色或背景色（含 `==`）时 `A ▾` 按下
    colored: bool,
    submenu: Option<Submenu>,
    /// 出现那一刻的滚动位置：滚过一屏就收掉
    scroll: Pixels,
}

impl Editor {
    /// 选区变了：空选区立刻收掉，非空选区等 [`DELAY`] 再出现（打字过程中不会闪）。
    /// 子菜单开着时按 5.9 留着工具条，等菜单关掉再按新选区重算。
    pub(super) fn schedule_toolbar(&mut self, cx: &mut Context<Self>) {
        if self.toolbar_pinned() {
            return;
        }
        let Some(selection) = self.textarea_selection(cx).filter(|s| !s.is_empty()) else {
            self.toolbar_task = None;
            self.hide_toolbar(cx);
            return;
        };
        if self.toolbar.as_ref().is_some_and(|bar| bar.selection == selection) {
            return;
        }
        self.toolbar = None;
        self.toolbar_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DELAY).await;
            let _ = this.update(cx, |this, cx| this.show_toolbar(&selection, cx));
        }));
        cx.notify();
    }

    /// 鼠标松手：拖出来的选区立刻给工具条，不用再等。
    pub(super) fn show_toolbar_now(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.textarea_selection(cx).filter(|s| !s.is_empty()) else { return };
        self.toolbar_task = None;
        self.show_toolbar(&selection, cx);
    }

    /// 选区没变过才显示（延迟期间又动了就作废），顺手读一次按下态。
    fn show_toolbar(&mut self, selection: &Range<usize>, cx: &mut Context<Self>) {
        let Some(f) = self.focused.as_ref() else { return };
        if self.textarea_selection(cx).as_ref() != Some(selection) {
            return;
        }
        let range = f.range.start + selection.start..f.range.start + selection.end;
        let source = self.doc.to_string();
        let active = aero_core::inline_format::active(&source, range.clone());
        let colored = colors::inline_color(&source, range.clone(), Slot::Text).is_some()
            || colors::inline_color(&source, range, Slot::Background).is_some();
        let submenu = self
            .toolbar
            .as_ref()
            .and_then(|bar| (bar.selection == *selection).then_some(bar.submenu))
            .flatten();
        self.toolbar = Some(Toolbar {
            selection: selection.clone(),
            active,
            colored,
            submenu,
            scroll: self.scroll.offset().y,
        });
        cx.notify();
    }

    /// 上色 / 加格式之后按新的选区重画：工具条留着，按下态跟着变。
    pub(super) fn refresh_toolbar(&mut self, cx: &mut Context<Self>) {
        match self.textarea_selection(cx).filter(|s| !s.is_empty()) {
            Some(selection) => self.show_toolbar(&selection, cx),
            None if self.toolbar_pinned() => {}
            None => {
                self.hide_toolbar(cx);
            }
        }
    }

    /// 收掉工具条（开始输入、选区消失、退出编辑态、滚动超过一屏）。返回原来是否开着。
    pub(super) fn hide_toolbar(&mut self, cx: &mut Context<Self>) -> bool {
        self.toolbar_task = None;
        if self.toolbar.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }

    /// `Esc`：先关子菜单，再关工具条本身。返回是否吃掉了这次按键。
    pub(super) fn dismiss_toolbar(&mut self, cx: &mut Context<Self>) -> bool {
        if self.close_toolbar_submenu(cx) {
            return true;
        }
        self.hide_toolbar(cx)
    }

    fn close_toolbar_submenu(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(bar) = self.toolbar.as_mut() else { return false };
        if bar.submenu.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }

    fn toolbar_pinned(&self) -> bool {
        self.color_menu.is_some() || self.toolbar.as_ref().is_some_and(|bar| bar.submenu.is_some())
    }

    /// 滚动：离开出现时的位置超过一屏就收掉（5.9）。
    pub(super) fn toolbar_scrolled(&mut self, cx: &mut Context<Self>) {
        let Some(bar) = self.toolbar.as_ref() else { return };
        let screen = self.scroll.bounds().size.height.max(px(1.));
        if (self.scroll.offset().y - bar.scroll).abs() > screen {
            self.hide_toolbar(cx);
        }
    }

    fn textarea_selection(&self, cx: &App) -> Option<Range<usize>> {
        Some(self.focused.as_ref()?.textarea.read(cx).selected_range())
    }

    fn toggle_convert_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.close_color_menu(window, cx);
        let Some(bar) = self.toolbar.as_mut() else { return };
        bar.submenu = match bar.submenu {
            Some(Submenu::Convert) => None,
            _ => Some(Submenu::Convert),
        };
        cx.notify();
    }

    fn toggle_more_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.close_color_menu(window, cx);
        let Some(bar) = self.toolbar.as_mut() else { return };
        bar.submenu = match bar.submenu {
            Some(Submenu::More) => None,
            _ => Some(Submenu::More),
        };
        cx.notify();
    }

    pub(super) fn copy_identity_for(
        &mut self,
        block: BlockId,
        action: IdentityCopy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.io.suspended || self.is_deleted() {
            return;
        }
        self.ensure_current_parse();
        let path = self.path.clone();
        let vault = self.vault.clone();
        let revision = self.doc.version();
        let generation = super::live_sources::generation(vault.id(), cx);
        let overlays = self.identity_overlays(cx);
        cx.spawn_in(window, async move |this, cx| {
            let source_path = path.clone();
            let assignment = cx
                .background_spawn(async move {
                    let (mut resolver, source) =
                        super::live_sources::identity_source(&vault, &source_path, overlays)?;
                    aero_core::block_refs::permanent_in(&mut resolver, &source_path, &source, block)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let result = assignment.and_then(|assignment| {
                    if this.path != path
                        || this.doc.version() != revision
                        || this.io.suspended
                        || this.is_deleted()
                        || super::live_sources::generation(this.vault.id(), cx) != generation
                    {
                        return Err("笔记已变化，请重试".into());
                    }
                    let link = assignment.link(&this.title());
                    if !assignment.edits.is_empty() {
                        this.doc.edit_group(assignment.edits).map_err(|e| e.to_string())?;
                        this.focused = None;
                        this.after_doc_change(cx);
                    }
                    let id = assignment.id;
                    match action {
                        IdentityCopy::Link => {
                            cx.write_to_clipboard(ClipboardItem::new_string(link));
                        }
                        IdentityCopy::Sync => this.copy_blocks(format!("!{link}"), cx),
                        IdentityCopy::Id => {
                            cx.write_to_clipboard(ClipboardItem::new_string(format!("^{id}")))
                        }
                    }
                    Ok(())
                });
                if let Err(error) = result {
                    window.push_notification(
                        gpui_kit::component::notification::Notification::error(error).title(action.title()),
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    fn copy_block_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(block) = self.focused_unit().map(|ix| self.units[ix].id) else { return };
        self.copy_block_link_for(block, window, cx);
        let _ = self.close_toolbar_submenu(cx);
    }

    pub(super) fn copy_block_link_for(
        &mut self,
        block: aero_core::BlockId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.copy_identity_for(block, IdentityCopy::Link, window, cx);
    }

    fn add_block_id(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(block) = self.focused_unit().map(|ix| self.units[ix].id) {
            self.copy_identity_for(block, IdentityCopy::Id, window, cx);
        }
        let _ = self.close_toolbar_submenu(cx);
    }

    fn convert_from_toolbar(&mut self, target: BlockType, window: &mut Window, cx: &mut Context<Self>) {
        let Some(offset) = self.focused.as_ref().map(|f| f.range.start) else { return };
        let _ = self.close_toolbar_submenu(cx);
        self.convert_at(offset, target, false, window, cx);
    }

    /// 工具条浮层：锚在选区首行上方 8 px，顶部放不下就翻到下方；选区滚出视口时不画。
    pub(super) fn render_toolbar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let bar = self.toolbar.as_ref()?;
        let f = self.focused.as_ref()?;
        // 选区跨多行时锚定首行（5.9）
        let anchor_at = bar.selection.start..bar.selection.start;
        let line = f.textarea.read(cx).range_to_bounds(&anchor_at)?;
        let viewport = self.scroll.bounds();
        if line.bottom() < viewport.origin.y || line.origin.y > viewport.bottom_right().y {
            return None;
        }
        let above = line.origin.y - GAP - HEIGHT;
        let y = if above < viewport.origin.y { line.bottom() + GAP } else { above };
        let theme = cx.theme().clone();
        let divider = || div().w(px(1.)).h(px(18.)).bg(theme.border);
        let convert_open = bar.submenu == Some(Submenu::Convert);
        let more_open = bar.submenu == Some(Submenu::More);
        Some(
            deferred(
                anchored().position(point(line.origin.x, y)).snap_to_window_with_margin(px(8.)).child(
                    v_flex()
                        .child(
                            h_flex()
                                .id("selection-toolbar")
                                .debug_selector(|| "selection-toolbar".to_owned())
                                .occlude()
                                .h(HEIGHT)
                                .px(px(4.))
                                .gap(px(2.))
                                .items_center()
                                .bg(theme.popover)
                                .text_color(theme.popover_foreground)
                                .border_1()
                                .border_color(theme.border)
                                .rounded(px(8.))
                                .shadow_lg()
                                .child(self.format_button(Format::Bold, "B", cx))
                                .child(self.format_button(Format::Italic, "I", cx))
                                .child(self.format_button(Format::Strike, "S̶", cx))
                                .child(self.format_button(Format::Code, "<>", cx))
                                .child(divider())
                                .child(
                                    // ⌘K：选区变行内链接
                                    Self::toolbar_button("toolbar-link", "🔗", "链接  ⌘K", false, &theme)
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.edit_link(window, cx);
                                                this.refresh_toolbar(cx);
                                            }),
                                        ),
                                )
                                .child(divider())
                                .child(
                                    Self::toolbar_button(
                                        "toolbar-color",
                                        "A ▾",
                                        "颜色  ⌘/",
                                        bar.colored || self.color_menu.is_some(),
                                        &theme,
                                    )
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _, window, cx| {
                                            cx.stop_propagation();
                                            let _ = this.close_toolbar_submenu(cx);
                                            this.open_color_menu(window, cx);
                                        }),
                                    ),
                                )
                                .child(divider())
                                .child(
                                    Self::toolbar_button(
                                        "toolbar-convert",
                                        "转换 ▾",
                                        "转换块类型",
                                        convert_open,
                                        &theme,
                                    )
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.toggle_convert_menu(window, cx);
                                        }),
                                    ),
                                )
                                .child(divider())
                                .child(
                                    Self::toolbar_button(
                                        "toolbar-more",
                                        "⋯",
                                        "复制块链接、添加 ^id",
                                        more_open,
                                        &theme,
                                    )
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.toggle_more_menu(window, cx);
                                        }),
                                    ),
                                ),
                        )
                        .when(convert_open, |el| el.child(self.render_convert_menu(&theme, cx)))
                        .when(more_open, |el| el.child(self.render_more_menu(&theme, cx))),
                ),
            )
            .into_any_element(),
        )
    }

    fn render_convert_menu(&self, theme: &gpui_kit::component::Theme, cx: &mut Context<Self>) -> AnyElement {
        let current = self.focused_unit().and_then(|ix| super::ops::block_type(&self.units[ix]));
        let rows: Vec<AnyElement> = slash::ITEMS
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let SlashAction::Convert(target) = item.action else { return None };
                let active = current == Some(target);
                Some(
                    h_flex()
                        .id(("toolbar-convert-item", i))
                        .debug_selector(move || format!("toolbar-convert-item-{i}"))
                        .w_full()
                        .h(px(28.))
                        .px_2()
                        .gap_2()
                        .rounded(px(4.))
                        .items_center()
                        .when(active, |el| el.bg(theme.list_active))
                        .hover(|el| el.bg(theme.list_hover))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.convert_from_toolbar(target, window, cx);
                            }),
                        )
                        .child(div().flex_1().min_w_0().text_size(px(13.)).child(item.label))
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(11.))
                                .text_color(theme.muted_foreground)
                                .child(item.hint),
                        )
                        .into_any_element(),
                )
            })
            .collect();
        v_flex()
            .id("toolbar-convert-menu")
            .debug_selector(|| "toolbar-convert-menu".to_owned())
            .occlude()
            .mt(px(4.))
            .w(px(200.))
            .p_1()
            .gap(px(1.))
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .border_1()
            .border_color(theme.border)
            .rounded(px(8.))
            .shadow_lg()
            .children(rows)
            .into_any_element()
    }

    fn render_more_menu(&self, theme: &gpui_kit::component::Theme, cx: &mut Context<Self>) -> AnyElement {
        let has_id = self.focused_unit().and_then(|ix| {
            let id = self.units[ix].id;
            self.doc.blocks().find(id).and_then(|block| block.persisted_id.clone())
        });
        let id_label = if has_id.is_some() { "复制 ^id" } else { "添加 ^id" };
        v_flex()
            .id("toolbar-more-menu")
            .debug_selector(|| "toolbar-more-menu".to_owned())
            .occlude()
            .mt(px(4.))
            .w(px(180.))
            .p_1()
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .border_1()
            .border_color(theme.border)
            .rounded(px(8.))
            .shadow_lg()
            .child(self.more_row("toolbar-copy-link", "复制块链接", cx, |this, window, cx| {
                this.copy_block_link(window, cx);
            }))
            .child(self.more_row("toolbar-add-id", id_label, cx, |this, window, cx| {
                this.add_block_id(window, cx);
            }))
            .into_any_element()
    }

    fn more_row(
        &self,
        id: &'static str,
        label: &'static str,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let hover = cx.theme().list_hover;
        h_flex()
            .id(id)
            .debug_selector(move || id.to_owned())
            .w_full()
            .h(px(28.))
            .px_2()
            .rounded(px(4.))
            .items_center()
            .text_size(px(13.))
            .cursor_pointer()
            .hover(move |el| el.bg(hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    action(this, window, cx);
                }),
            )
            .child(label)
            .into_any_element()
    }

    fn format_button(&self, format: Format, glyph: &'static str, cx: &mut Context<Self>) -> Stateful<Div> {
        let active = self.toolbar.as_ref().is_some_and(|bar| bar.active.contains(&format));
        let (id, tooltip) = match format {
            Format::Bold => ("toolbar-bold", "粗体  ⌘B"),
            Format::Italic => ("toolbar-italic", "斜体  ⌘I"),
            Format::Strike => ("toolbar-strike", "删除线  ⌘⇧S"),
            Format::Code => ("toolbar-code", "行内代码  ⌘E"),
            Format::Highlight => ("toolbar-highlight", "黄色高亮  ⌘⇧H"),
        };
        let theme = cx.theme().clone();
        Self::toolbar_button(id, glyph, tooltip, active, &theme)
            .map(|el| match format {
                Format::Bold => el.font_weight(FontWeight::BOLD),
                Format::Italic => el.italic(),
                Format::Strike => el.line_through(),
                Format::Code => el.font_family(theme.mono_font_family.clone()).text_size(px(11.)),
                Format::Highlight => el,
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.format_inline(format, window, cx);
                    this.refresh_toolbar(cx);
                }),
            )
    }

    /// 一个按钮的外观：按下态用选中底色，悬停有底色，鼠标不改焦点（选区要留着）。
    fn toolbar_button(
        id: &'static str,
        glyph: &'static str,
        tooltip: &'static str,
        active: bool,
        theme: &gpui_kit::component::Theme,
    ) -> Stateful<Div> {
        let (hover, pressed) = (theme.list_hover, theme.list_active);
        div()
            .id(id)
            .debug_selector(move || id.to_owned())
            .h(px(24.))
            .min_w(px(26.))
            .px(px(4.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .text_size(px(13.))
            .cursor_pointer()
            .when(active, |el| el.bg(pressed))
            .hover(move |el| el.bg(hover))
            .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tooltip).build(window, cx))
            .child(glyph)
    }
}

#[cfg(test)]
impl Editor {
    /// 工具条上处于按下态的格式（测试用）。
    pub(super) fn toolbar_active(&self) -> Option<Vec<Format>> {
        self.toolbar.as_ref().map(|bar| bar.active.clone())
    }

    pub(super) fn toolbar_open(&self) -> bool {
        self.toolbar.is_some()
    }

    pub(super) fn toolbar_convert_open(&self) -> bool {
        self.toolbar.as_ref().is_some_and(|bar| bar.submenu == Some(Submenu::Convert))
    }

    pub(super) fn toolbar_more_open(&self) -> bool {
        self.toolbar.as_ref().is_some_and(|bar| bar.submenu == Some(Submenu::More))
    }

    pub(super) fn toolbar_colored(&self) -> bool {
        self.toolbar.as_ref().is_some_and(|bar| bar.colored)
    }
}
