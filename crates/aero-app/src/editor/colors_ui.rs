//! 上色的写回入口与颜色菜单（`ui_design.md` 7.7）。
//!
//! 写回：`⌘⇧H` 应用上次使用的颜色（编辑态给选中的文字、块选中态给选中的块）。
//! 菜单：`⌘/`、`/颜色` 或命令面板打开，9 色 + 默认 × 文字 / 背景 两组，可输入过滤。
//! 菜单打开时焦点借给它自己的过滤输入框，编辑态的控件与选区都留着。
//! 5.9 的选区浮动工具条接上后，`A ▾` 打开的也是 [`Editor::open_color_menu`]。

use aero_core::colors::{self, Inline, Slot};
use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::component::input::Input;

use super::color_menu;
use super::*;
use crate::actions::{CloseColorMenu, ConfirmColorMenu, MoveColorMenu, OpenColorMenu};

/// 颜色菜单要改的东西：编辑态的选区，或一个 / 多个块。菜单打开时就定下来。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    /// 选中的文字（行内颜色）
    Inline,
    /// 整块（编辑态没有选区时是当前块；块选中态是选中的那些）
    Blocks(Vec<BlockId>),
}

/// 颜色菜单态（7.7）。
pub(super) struct ColorMenu {
    target: Target,
    /// 过滤输入框：菜单打开时持有焦点，中文也能输入
    input: Entity<InputState>,
    /// 过滤后的条目编号（`color_menu` 的下标）
    matches: Vec<usize>,
    /// 选中项在 `matches` 里的位置
    selected: usize,
    /// 打开时目标已有的颜色（文字、背景），用来画选中标记
    current: [Option<String>; 2],
    _subscription: Subscription,
}

impl Editor {
    pub(super) fn on_apply_last_color(
        &mut self,
        _: &crate::actions::ApplyLastColor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let last = crate::colors::last(cx);
        if !self.editing_with_keyboard(window, cx) {
            cx.propagate();
            return;
        }
        if !self.apply_color(last.slot, Some(last.color), window, cx) {
            cx.propagate();
        }
    }

    /// 这次按键能不能落到正文上：编辑态要求控件持有焦点且没在输入法组字中。
    pub(super) fn editing_with_keyboard(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(f) = &self.focused else { return true };
        f.textarea.focus_handle(cx).is_focused(window) && !self.input_composing(window, cx)
    }

    /// 上色 / 去色（`color` 为 `None` 时移除这一组）。编辑态作用于选中的文字，否则作用于选中的块。
    /// 返回是否由编辑器处理了这次操作。
    pub(super) fn apply_color(
        &mut self,
        slot: Slot,
        color: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.title_edit.is_some() {
            return false;
        }
        if let Some(color) = color {
            crate::colors::remember(slot, color, cx);
        }
        match self.focused.is_some() {
            true => self.color_selection(slot, color, window, cx),
            false => self.color_selected_blocks(slot, color, window, cx),
        }
    }

    /// 选中的文字：`[文本]{.red}` 就地写回；黄色背景走 `==` 那条路，与 `⌘⇧H` 从前的行为一致。
    fn color_selection(
        &mut self,
        slot: Slot,
        color: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(f) = &self.focused else { return false };
        let selected = f.textarea.read(cx).selected_range();
        let selection = f.range.start + selected.start..f.range.start + selected.end;
        let source = self.doc.to_string();
        match colors::inline_edit(&source, selection.clone(), slot, color) {
            Some(Inline::Highlight) => {
                self.format_inline(aero_core::inline_format::Format::Highlight, window, cx)
            }
            Some(Inline::Edit(edit)) => {
                // 上色后选区仍然盖住原来那段文字，可以接着换颜色
                let body = source[selection].to_owned();
                let at = edit.text.find(&body).map(|i| edit.range.start + i);
                self.slash = None;
                self.wiki = None;
                self.tag_menu = None;
                self.last_edit_at = None;
                let cursor = at.map_or(edit.new_range().end, |at| at + body.len());
                self.structural_edit(edit, cursor, window, cx);
                if let Some(at) = at
                    && let Some(f) = &self.focused
                    && at >= f.range.start
                    && at + body.len() <= f.range.end
                {
                    let selected = at - f.range.start..at + body.len() - f.range.start;
                    f.textarea.update(cx, |ta, cx| ta.set_selected_range(selected, cx));
                }
                cx.notify();
                true
            }
            // 已经是这个颜色、选区跨行、或者写回会改坏结构：什么都不做，但这次按键仍然算处理过
            None => true,
        }
    }

    /// 块选中态：给选中的每一块上色，选区保持不变。
    fn color_selected_blocks(
        &mut self,
        slot: Slot,
        color: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(sel) = self.selection else { return false };
        let ids: Vec<BlockId> =
            (sel.first()..=sel.last()).filter_map(|ix| self.units.get(ix)).map(|u| u.id).collect();
        if !self.color_blocks(&ids, slot, color, cx) {
            return true;
        }
        // 上色不改块结构，选区还是原来那几个下标
        if !self.units.is_empty() {
            let last_ix = self.units.len() - 1;
            self.select_blocks(sel.first().min(last_ix), sel.last().min(last_ix), window, cx);
        }
        true
    }

    /// 给这些块上色：属性列表写在块末，多块合成一个撤销组（7.7）。返回文档是否真的改了。
    fn color_blocks(
        &mut self,
        ids: &[BlockId],
        slot: Slot,
        color: Option<&str>,
        cx: &mut Context<Self>,
    ) -> bool {
        let source = self.doc.to_string();
        // 从后往前：`edit_group` 顺序应用，后面的编辑不能挪动前面那一条的坐标
        let mut edits = Vec::new();
        for id in ids.iter().rev() {
            let Some(block) = self.doc.blocks().find(*id) else { continue };
            let range = self.doc.range_of(block);
            if let Some(edit) = colors::block_edit(&source, range, slot, color) {
                edits.push(edit);
            }
        }
        if edits.is_empty() {
            return false;
        }
        if let Err(err) = self.doc.edit_group(edits) {
            log::error!("color edit rejected: {err}");
            return false;
        }
        self.last_edit_at = None;
        self.after_doc_change(cx);
        true
    }

    /// 编辑态里当前那一块。空段落（还没成块）时没有。
    fn focused_block_id(&self) -> Option<BlockId> {
        let f = self.focused.as_ref()?;
        f.id.or_else(|| self.focused_unit().map(|at| self.units[at].id))
    }

    /// 把菜单里的一项直接应用到当前块——`/红色`、`/蓝色背景` 走的就是这条路（7.1 / 7.7）。
    pub(super) fn apply_menu_color(&mut self, ix: usize, cx: &mut Context<Self>) {
        let item = color_menu::item(ix);
        let Some(id) = self.focused_block_id() else { return };
        self.color_blocks(&[id], item.slot, item.color, cx);
        if let Some(color) = item.color {
            crate::colors::remember(item.slot, color, cx);
        }
    }

    // ---- 颜色菜单（7.7） ----

    /// 打开颜色菜单。编辑态有选区就改那段文字，没有选区就改当前块；块选中态改选中的块。
    /// 菜单开着时再按一次就关掉（`⌘/` 是开关）。
    pub(super) fn open_color_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.title_edit.is_some() {
            return false;
        }
        // ⌘/ 是开关：已经开着就关掉
        if self.close_color_menu(window, cx) {
            return true;
        }
        let source = self.doc.to_string();
        let (target, current) = match (&self.focused, self.selection) {
            (Some(f), _) => {
                let selected = f.textarea.read(cx).selected_range();
                let selection = f.range.start + selected.start..f.range.start + selected.end;
                match selection.is_empty() {
                    // 编辑态没有选区：菜单改当前块（块菜单的"颜色 ▸"就是这个）
                    true => {
                        let Some(id) = self.focused_block_id() else { return false };
                        (Target::Blocks(vec![id]), self.block_colors(id, &source))
                    }
                    false => {
                        let read = |slot| colors::inline_color(&source, selection.clone(), slot);
                        (Target::Inline, [read(Slot::Text), read(Slot::Background)])
                    }
                }
            }
            (None, Some(sel)) => {
                let ids: Vec<BlockId> =
                    (sel.first()..=sel.last()).filter_map(|ix| self.units.get(ix)).map(|u| u.id).collect();
                let current = match ids.as_slice() {
                    // 多块选中时不显示选中标记：它们的颜色未必一样
                    [id] => self.block_colors(*id, &source),
                    _ => [None, None],
                };
                (Target::Blocks(ids), current)
            }
            (None, None) => return false,
        };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("过滤颜色"));
        let subscription = cx.subscribe_in(&input, window, Self::on_color_menu_input);
        self.slash = None;
        self.wiki = None;
        self.tag_menu = None;
        self.color_menu = Some(ColorMenu {
            target,
            input: input.clone(),
            matches: color_menu::filter(""),
            selected: 0,
            current,
            _subscription: subscription,
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
        true
    }

    fn block_colors(&self, id: BlockId, source: &str) -> [Option<String>; 2] {
        let Some(block) = self.doc.blocks().find(id) else { return [None, None] };
        let range = self.doc.range_of(block);
        let read = |slot| colors::block_color(source, range.clone(), slot);
        [read(Slot::Text), read(Slot::Background)]
    }

    pub(super) fn on_open_color_menu(
        &mut self,
        _: &OpenColorMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.editing_with_keyboard(window, cx) || !self.open_color_menu(window, cx) {
            cx.propagate();
        }
    }

    /// 关菜单并把焦点还给正文（编辑态回控件、块选中态回容器）。
    pub(super) fn close_color_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.color_menu.take().is_none() {
            return false;
        }
        match &self.focused {
            Some(f) => f.textarea.update(cx, |state, cx| state.focus(window, cx)),
            None => self.focus_handle.focus(window, cx),
        }
        cx.notify();
        true
    }

    pub(super) fn on_close_color_menu(
        &mut self,
        _: &CloseColorMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.close_color_menu(window, cx) {
            cx.propagate();
        }
    }

    pub(super) fn on_move_color_menu(
        &mut self,
        action: &MoveColorMenu,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.color_menu.as_mut() else {
            cx.propagate();
            return;
        };
        menu.selected = color_menu::step(&menu.matches, menu.selected, action.0 as isize, action.1 as isize);
        cx.notify();
    }

    pub(super) fn on_confirm_color_menu(
        &mut self,
        _: &ConfirmColorMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.color_menu.is_some() {
            self.confirm_color_menu(None, window, cx);
        } else {
            cx.propagate();
        }
    }

    fn on_color_menu_input(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                let Some(menu) = self.color_menu.as_mut() else { return };
                let query = menu.input.read(cx).value().to_string();
                menu.matches = color_menu::filter(&query);
                menu.selected = 0;
                cx.notify();
            }
            InputEvent::PressEnter { .. } => self.confirm_color_menu(None, window, cx),
            // 点到别处：菜单关掉，焦点留在用户点的地方
            InputEvent::Blur => {
                if self.color_menu.take().is_some() {
                    cx.notify();
                }
            }
            InputEvent::Focus => {}
        }
    }

    /// 应用菜单里的一项（`at` 是 `matches` 里的位置，`None` 用当前选中项），然后关掉菜单。
    fn confirm_color_menu(&mut self, at: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.color_menu.as_ref() else { return };
        let Some(&ix) = at.or(Some(menu.selected)).and_then(|at| menu.matches.get(at)) else { return };
        let item = color_menu::item(ix);
        let target = menu.target.clone();
        self.close_color_menu(window, cx);
        match &target {
            Target::Inline => {
                self.color_selection(item.slot, item.color, window, cx);
            }
            Target::Blocks(ids) => {
                self.color_blocks(ids, item.slot, item.color, cx);
            }
        }
        if let Some(color) = item.color {
            crate::colors::remember(item.slot, color, cx);
        }
        cx.notify();
    }

    /// 菜单浮层：锚在光标 / 当前块下面，条目按 7.7 的两组网格排，样本用真实颜色画 `A`。
    pub(super) fn render_color_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(menu) = self.color_menu.as_ref() else { return div().into_any_element() };
        let theme = cx.theme().clone();
        let dark = theme.mode.is_dark();
        let last = crate::colors::last(cx);
        let rows = color_menu::rows(&menu.matches);
        let mut body: Vec<AnyElement> = Vec::new();
        let mut previous: Option<Slot> = None;
        for cells in rows {
            let slot = color_menu::item(menu.matches[cells[0]]).slot;
            if previous != Some(slot) {
                if previous.is_some() {
                    body.push(div().h(px(1.)).my_1().bg(theme.border).into_any_element());
                }
                let title = match slot {
                    Slot::Text => "文字",
                    Slot::Background => "背景",
                };
                body.push(
                    div()
                        .px_1()
                        .pb(px(2.))
                        .text_size(px(11.))
                        .text_color(theme.muted_foreground)
                        .child(title)
                        .into_any_element(),
                );
                previous = Some(slot);
            }
            body.push(
                h_flex()
                    .w_full()
                    .gap(px(2.))
                    .children(cells.into_iter().map(|at| self.render_color_sample(at, dark, cx)))
                    .into_any_element(),
            );
        }
        let sample = crate::colors::token(last.slot, last.color, dark);
        deferred(
            anchored().position(self.color_menu_position(cx)).snap_to_window_with_margin(px(8.)).child(
                v_flex()
                    .id("color-menu")
                    .debug_selector(|| "color-menu".to_owned())
                    .key_context("ColorMenu")
                    .on_action(cx.listener(Self::on_move_color_menu))
                    .on_action(cx.listener(Self::on_confirm_color_menu))
                    .on_action(cx.listener(Self::on_close_color_menu))
                    .occlude()
                    .w(px(268.))
                    .p_1()
                    .bg(theme.popover)
                    .text_color(theme.popover_foreground)
                    .border_1()
                    .border_color(theme.border)
                    .rounded(px(8.))
                    .shadow_lg()
                    // 「上次使用」那一行：⌘⇧H 不开菜单也能直接用它
                    .child(
                        h_flex()
                            .id("color-menu-last")
                            .w_full()
                            .h(px(28.))
                            .px_2()
                            .gap_2()
                            .items_center()
                            .rounded(px(4.))
                            .cursor_pointer()
                            .hover(|el| el.bg(theme.list_hover))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    this.close_color_menu(window, cx);
                                    this.apply_color(last.slot, Some(last.color), window, cx);
                                }),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(11.))
                                    .text_color(theme.muted_foreground)
                                    .child("上次使用"),
                            )
                            .child(Self::color_glyph(last.slot, sample, &theme))
                            .child(div().flex_1().min_w_0().text_size(px(12.)).child(last_name(last)))
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(11.))
                                    .text_color(theme.muted_foreground)
                                    .child("⌘⇧H"),
                            ),
                    )
                    .child(div().h(px(1.)).my_1().bg(theme.border))
                    .child(div().px_1().pb_1().child(Input::new(&menu.input).xsmall()))
                    .children(body)
                    .when(menu.matches.is_empty(), |el| {
                        el.child(
                            div()
                                .px_2()
                                .py_1()
                                .text_size(px(12.))
                                .text_color(theme.muted_foreground)
                                .child("没有匹配的颜色"),
                        )
                    }),
            ),
        )
        .into_any_element()
    }

    /// 一个颜色样本：真实颜色画的 `A` + 短名，当前已用的那个打勾。
    fn render_color_sample(&self, at: usize, dark: bool, cx: &Context<Self>) -> AnyElement {
        let Some(menu) = self.color_menu.as_ref() else { return div().into_any_element() };
        let theme = cx.theme().clone();
        let ix = menu.matches[at];
        let item = color_menu::item(ix);
        let color = item.color.and_then(|color| crate::colors::token(item.slot, color, dark));
        let slot_ix = match item.slot {
            Slot::Text => 0,
            Slot::Background => 1,
        };
        let checked = menu.current[slot_ix].as_deref() == item.color;
        let active = at == menu.selected;
        v_flex()
            .id(("color-sample", at))
            .debug_selector(move || format!("color-sample-{}", color_menu::full_name(ix)))
            .w(px(50.))
            .py(px(4.))
            .gap(px(1.))
            .items_center()
            .rounded(px(4.))
            .cursor_pointer()
            .when(active, |el| el.bg(theme.list_active))
            .hover(|el| el.bg(theme.list_hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| this.confirm_color_menu(Some(at), window, cx)),
            )
            .child(Self::color_glyph(item.slot, color, &theme))
            .child(
                h_flex()
                    .gap(px(1.))
                    .items_center()
                    .text_size(px(11.))
                    .when(!checked, |el| el.text_color(theme.muted_foreground))
                    .child(color_menu::label(ix))
                    .when(checked, |el| el.child(div().text_size(px(9.)).child("✓"))),
            )
            .into_any_element()
    }

    /// 一个条目的样本，`/` 菜单里的颜色候选也用它。
    pub(super) fn color_swatch(ix: usize, dark: bool, cx: &Context<Self>) -> Div {
        let item = color_menu::item(ix);
        let color = item.color.and_then(|color| crate::colors::token(item.slot, color, dark));
        Self::color_glyph(item.slot, color, cx.theme())
    }

    /// 样本里的 `A`：文字组染前景色，背景组铺底色（`ui_design.md` 7.7）。
    fn color_glyph(slot: Slot, color: Option<Hsla>, theme: &gpui_kit::component::Theme) -> Div {
        let mut glyph = div()
            .w(px(20.))
            .h(px(20.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .text_size(px(13.))
            .child("A");
        match (slot, color) {
            (Slot::Text, Some(color)) => glyph = glyph.text_color(color),
            (Slot::Background, Some(color)) => glyph = glyph.bg(color),
            // 「默认」样本：描边表示"没有颜色"
            (_, None) => glyph = glyph.border_1().border_color(theme.border),
        }
        glyph
    }

    /// 菜单锚点：编辑态跟着光标，块选中态贴在选区头那一块下面（它的矩形由 `render_unit` 每帧记下）。
    fn color_menu_position(&self, cx: &Context<Self>) -> Point<Pixels> {
        let Some(f) = self.focused.as_ref() else {
            let fallback = self.scroll.bounds().origin + point(px(24. + GUTTER), px(8.));
            return self
                .head_bounds
                .get()
                .map_or(fallback, |bounds| bounds.bottom_left() + point(px(0.), px(4.)));
        };
        let ta = f.textarea.read(cx);
        match ta.cursor_layout() {
            Some((bounds, line_height)) => bounds.origin + point(px(0.), line_height + px(4.)),
            None => {
                let b = ta.input_bounds();
                b.origin + point(px(0.), b.size.height + px(4.))
            }
        }
    }
}

#[cfg(test)]
impl Editor {
    /// 菜单里当前列出的条目（完整名字，按显示顺序）。
    pub(super) fn color_menu_names(&self) -> Vec<String> {
        let Some(menu) = self.color_menu.as_ref() else { return Vec::new() };
        menu.matches.iter().map(|&ix| color_menu::full_name(ix)).collect()
    }

    /// 菜单里选中那一项的名字。
    pub(super) fn color_menu_selected(&self) -> Option<String> {
        let menu = self.color_menu.as_ref()?;
        menu.matches.get(menu.selected).map(|&ix| color_menu::full_name(ix))
    }

    /// 菜单打开时读到的已有颜色（文字、背景）：画选中标记用的就是它。
    pub(super) fn color_menu_current(&self) -> [Option<String>; 2] {
        self.color_menu.as_ref().map(|menu| menu.current.clone()).unwrap_or_default()
    }
}

/// 「上次使用」那一行显示的名字。
fn last_name(last: crate::colors::Last) -> String {
    let ix = (0..color_menu::COUNT)
        .find(|&ix| {
            let item = color_menu::item(ix);
            item.slot == last.slot && item.color == Some(last.color)
        })
        .unwrap_or(0);
    color_menu::full_name(ix)
}
