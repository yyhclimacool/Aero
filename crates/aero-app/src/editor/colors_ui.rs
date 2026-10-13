//! 上色的写回入口与颜色菜单（`ui_design.md` 7.7）。
//!
//! 写回：`⌘⇧H` 应用上次使用的颜色（编辑态给选中的文字、块选中态给选中的块）。
//! 菜单：`⌘/`、`/颜色` 或命令面板打开，9 色 + 默认 × 文字 / 背景 两组，可输入过滤。
//! 菜单打开时焦点借给它自己的过滤输入框，编辑态的控件与选区都留着。
//! 5.9 的选区浮动工具条上的 `A ▾` 打开的也是 [`Editor::open_color_menu`]。

use aero_core::colors::{self, Inline, Slot};
use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
use gpui_kit::component::input::Input;

use super::color_menu;
use super::*;
use crate::actions::{CloseColorMenu, ConfirmColorMenu, MoveColorMenu, OpenColorMenu};

/// 颜色菜单要改的东西：编辑态的选区，或一个 / 多个块。菜单打开时就定下来。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    /// 选中的文字（行内颜色）
    Inline(Range<usize>),
    /// 整块（编辑态没有选区时是当前块；块选中态是选中的那些）
    Blocks(Vec<BlockId>),
}

/// 颜色菜单态（7.7）。
pub(super) struct ColorMenu {
    allowed: [bool; 3],
    revision: u64,
    target: Target,
    picker: Entity<ColorPickerState>,
    custom: Option<String>,
    custom_slot: Slot,
    _picker_subscription: Subscription,
    /// 过滤输入框：菜单打开时持有焦点，中文也能输入
    input: Entity<InputState>,
    /// 过滤后的条目编号（`color_menu` 的下标）
    matches: Vec<usize>,
    /// 选中项在 `matches` 里的位置
    selected: usize,
    /// 打开时目标已有的颜色（文字、背景），用来画选中标记
    current: [Option<String>; 3],
    /// 选区盖住整块时可以改去给块上色。`None` 表示这次菜单没有这个开关。
    whole_block_id: Option<BlockId>,
    whole_block: bool,
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
        if !self.apply_color(last.slot, Some(&last.color), window, cx) {
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
                // 上色后选区仍然盖住被上色的正文（扩大选区时里面的标记已经收掉）
                let painted = colors::painted_content(&edit.text);
                let at = edit.range.start + painted.start;
                let len = painted.end - painted.start;
                self.slash = None;
                self.wiki = None;
                self.tag_menu = None;
                self.last_edit_at = None;
                self.structural_edit(edit, at + len, window, cx);
                if let Some(f) = &self.focused
                    && at >= f.range.start
                    && at + len <= f.range.end
                {
                    let selected = at - f.range.start..at + len - f.range.start;
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
        // 从后往前：`edit_group` 顺序应用，后面的编辑不能挪动前面那一条的坐标
        let mut edits = Vec::new();
        for id in ids.iter().rev() {
            if ids.iter().any(|parent| {
                *parent != *id
                    && aero_core::containers::ancestors(&self.doc, *id).iter().any(|b| b.id == *parent)
            }) {
                continue;
            }
            let Some(block) = self.doc.blocks().find(*id) else { continue };
            let mut attrs = block.attrs.clone();
            slot.write(&mut attrs, color);
            if let Ok(Some(edit)) =
                aero_core::properties::edit(&self.doc, *id, &attrs, block.persisted_id.as_deref())
            {
                edits.push(edit);
            }
        }
        if edits.is_empty() {
            return false;
        }
        edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
        if let Err(err) = self.doc.edit_group(edits) {
            log::error!("color edit rejected: {err}");
            return false;
        }
        self.last_edit_at = None;
        self.after_doc_change(cx);
        true
    }

    /// 编辑态里当前那一块。空段落（还没成块）时没有。
    pub(super) fn focused_block_id(&self) -> Option<BlockId> {
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
        self.open_color_menu_with_target(None, window, cx)
    }

    pub(super) fn open_color_menu_for_blocks(
        &mut self,
        ids: Vec<BlockId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.open_color_menu_with_target(Some(ids), window, cx)
    }

    fn open_color_menu_with_target(
        &mut self,
        blocks: Option<Vec<BlockId>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // ⌘/ 是开关：已经开着就关掉
        if self.close_color_menu(window, cx) {
            return true;
        }
        let source = self.doc.to_string();
        self.ensure_current_parse();
        let (target, current) = if let Some(ids) = blocks {
            let current = ids.first().map(|id| self.block_colors(*id, &source)).unwrap_or([None, None, None]);
            (Target::Blocks(ids), current)
        } else {
            match (&self.focused, self.selection) {
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
                            (
                                Target::Inline(selection.clone()),
                                [read(Slot::Text), read(Slot::Background), None],
                            )
                        }
                    }
                }
                (None, Some(sel)) => {
                    let ids: Vec<BlockId> = (sel.first()..=sel.last())
                        .filter_map(|ix| self.units.get(ix))
                        .map(|u| u.id)
                        .collect();
                    let current = match ids.as_slice() {
                        // 多块选中时不显示选中标记：它们的颜色未必一样
                        [id] => self.block_colors(*id, &source),
                        _ => [None, None, None],
                    };
                    (Target::Blocks(ids), current)
                }
                (None, None) => return false,
            }
        };
        let whole_block_id = match &self.focused {
            Some(f) if matches!(target, Target::Inline(_)) => {
                let selected = f.textarea.read(cx).selected_range();
                let len = f.textarea.read(cx).value().len();
                (selected.start == 0 && selected.end == len && len > 0)
                    .then(|| self.focused_block_id())
                    .flatten()
            }
            _ => None,
        };
        let allowed = [Slot::Text, Slot::Background, Slot::Border].map(|slot| match &target {
            Target::Inline(_) => slot != Slot::Border,
            Target::Blocks(ids) => ids.iter().all(|id| {
                self.doc.blocks().find(*id).is_some_and(|b| match slot {
                    Slot::Text => matches!(
                        b.kind,
                        BlockKind::Paragraph
                            | BlockKind::Heading { .. }
                            | BlockKind::ListItem { .. }
                            | BlockKind::CodeFence { .. }
                    ),
                    Slot::Background => true,
                    Slot::Border => aero_core::containers::is_editable(&b.kind),
                })
            }),
        });
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("搜索颜色或输入 HEX / rgba"));
        let subscription = cx.subscribe_in(&input, window, Self::on_color_menu_input);
        let picker = cx.new(|cx| ColorPickerState::new(window, cx));
        let picker_subscription =
            cx.subscribe_in(&picker, window, |this, _, event: &ColorPickerEvent, _, cx| {
                let ColorPickerEvent::Change(value) = event;
                if let Some(menu) = &mut this.color_menu {
                    menu.custom = value.map(|color| {
                        let rgba = gpui_kit::Rgba::from(color);
                        let byte = |v: f32| (v * 255.).round().clamp(0., 255.) as u8;
                        if rgba.a < 1. {
                            format!("rgba({},{},{},{})", byte(rgba.r), byte(rgba.g), byte(rgba.b), rgba.a)
                        } else {
                            format!("#{:02X}{:02X}{:02X}", byte(rgba.r), byte(rgba.g), byte(rgba.b))
                        }
                    });
                }
                cx.notify();
            });
        self.slash = None;
        self.wiki = None;
        self.tag_menu = None;
        self.color_menu = Some(ColorMenu {
            allowed,
            revision: self.doc.version(),
            target,
            picker,
            custom: None,
            custom_slot: Slot::Background,
            _picker_subscription: picker_subscription,
            input: input.clone(),
            matches: color_menu::filter("")
                .into_iter()
                .filter(|ix| allowed[slot_index(color_menu::item(*ix).slot)])
                .collect(),
            selected: 0,
            current,
            whole_block_id,
            whole_block: false,
            _subscription: subscription,
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
        true
    }

    fn block_colors(&self, id: BlockId, source: &str) -> [Option<String>; 3] {
        let Some(block) = self.doc.blocks().find(id) else { return [None, None, None] };
        let _ = source;
        [
            Slot::Text.color_of(&block.attrs).map(str::to_owned),
            Slot::Background.color_of(&block.attrs).map(str::to_owned),
            Slot::Border.color_of(&block.attrs).map(str::to_owned),
        ]
    }

    pub(super) fn on_open_color_menu(
        &mut self,
        _: &OpenColorMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.input_composing(window, cx) {
            cx.propagate();
            return;
        }
        // 颜色菜单已经开着：⌘/ 把它关掉。否则 ⌘/ 打开块菜单，颜色从「颜色 ▸」进去。
        if self.color_menu.is_some() {
            let _ = self.close_color_menu(window, cx);
            return;
        }
        if !self.open_block_menu_for_current(window, cx) {
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
        if self.toolbar.is_some() {
            self.refresh_toolbar(cx);
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
                menu.matches = color_menu::filter(&query)
                    .into_iter()
                    .filter(|ix| menu.allowed[slot_index(color_menu::item(*ix).slot)])
                    .collect();
                menu.custom = colors::normalize_custom(&query);
                menu.selected = 0;
                cx.notify();
            }
            InputEvent::PressEnter { .. } => self.confirm_color_menu(None, window, cx),
            // 点到别处：菜单关掉，焦点留在用户点的地方
            InputEvent::Blur => {}
            InputEvent::Focus => {}
        }
    }

    /// 应用菜单里的一项（`at` 是 `matches` 里的位置，`None` 用当前选中项），然后关掉菜单。
    fn confirm_color_menu(&mut self, at: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.color_menu.as_ref() else { return };
        if menu.revision != self.doc.version() {
            self.close_color_menu(window, cx);
            window.push_notification(
                gpui_kit::component::notification::Notification::error("目标内容已变化，请重新打开颜色菜单"),
                cx,
            );
            return;
        }
        let (slot, color) = if at.is_none() && menu.custom.is_some() {
            (menu.custom_slot, menu.custom.clone())
        } else {
            let Some(&ix) = at.or(Some(menu.selected)).and_then(|at| menu.matches.get(at)) else { return };
            let item = color_menu::item(ix);
            (item.slot, item.color.map(str::to_owned))
        };
        let target = menu.target.clone();
        if !menu.allowed[slot_index(slot)] {
            return;
        }
        let whole = menu.whole_block.then_some(menu.whole_block_id).flatten();
        self.close_color_menu(window, cx);
        match &target {
            Target::Inline(_) if let Some(id) = whole => {
                self.color_blocks(&[id], slot, color.as_deref(), cx);
            }
            Target::Inline(range) => {
                if let Some(f) = &self.focused
                    && range.start >= f.range.start
                    && range.end <= f.range.end
                {
                    let selected = range.start - f.range.start..range.end - f.range.start;
                    f.textarea.update(cx, |ta, cx| ta.set_selected_range(selected, cx));
                } else {
                    return;
                }
                self.color_selection(slot, color.as_deref(), window, cx);
            }
            Target::Blocks(ids) => {
                self.color_blocks(ids, slot, color.as_deref(), cx);
            }
        }
        if let Some(color) = &color {
            crate::colors::remember(slot, color, cx);
        }
        if self.toolbar.is_some() {
            self.refresh_toolbar(cx);
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
                    Slot::Border => "边框",
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
        let sample = crate::colors::token(last.slot, &last.color, dark);
        let last_label = last_name(&last);
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
                    .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                        if this.color_menu.as_ref().is_some_and(|menu| !menu.picker.read(cx).is_open()) {
                            this.close_color_menu(window, cx);
                        }
                    }))
                    .when(menu.whole_block_id.is_some(), |el| {
                        let on = menu.whole_block;
                        el.child(
                            h_flex()
                                .id("color-apply-block")
                                .debug_selector(|| "color-apply-block".to_owned())
                                .w_full()
                                .h(px(28.))
                                .px_2()
                                .items_center()
                                .rounded(px(4.))
                                .cursor_pointer()
                                .hover(|el| el.bg(theme.list_hover))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        cx.stop_propagation();
                                        if let Some(menu) = &mut this.color_menu {
                                            menu.whole_block = !menu.whole_block;
                                        }
                                        cx.notify();
                                    }),
                                )
                                .child(if on { "应用到整块  ✓" } else { "应用到整块" }),
                        )
                    })
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
                                    if let Some(menu) = &mut this.color_menu {
                                        menu.custom_slot = last.slot;
                                        menu.custom = Some(last.color.clone());
                                    }
                                    this.confirm_color_menu(None, window, cx);
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
                            .child(div().flex_1().min_w_0().text_size(px(12.)).child(last_label))
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
                    .child(h_flex().gap_1().px_1().child(ColorPicker::new(&menu.picker).label("自定义颜色")))
                    .child(
                        h_flex().gap_1().px_1().children(
                            [("文字", Slot::Text), ("背景", Slot::Background), ("边框", Slot::Border)]
                                .into_iter()
                                .enumerate()
                                .map(|(i, (label, slot))| {
                                    div()
                                        .id(("custom-color-slot", i))
                                        .text_xs()
                                        .px_2()
                                        .py_1()
                                        .rounded(px(4.))
                                        .opacity(if menu.allowed[slot_index(slot)] { 1. } else { 0.35 })
                                        .when(slot == menu.custom_slot, |el| el.bg(theme.list_hover))
                                        .child(label)
                                        .cursor_pointer()
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                if let Some(menu) = &mut this.color_menu
                                                    && menu.allowed[slot_index(slot)]
                                                {
                                                    menu.custom_slot = slot;
                                                }
                                                cx.notify();
                                            }),
                                        )
                                }),
                        ),
                    )
                    .when(menu.custom.is_some(), |el| {
                        el.child(
                            div()
                                .id("custom-color-apply")
                                .px_2()
                                .py_1()
                                .text_sm()
                                .cursor_pointer()
                                .child(format!("应用 {}", menu.custom.as_deref().unwrap_or_default()))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.confirm_color_menu(None, window, cx);
                                    }),
                                ),
                        )
                    })
                    .children(body)
                    .when(menu.matches.is_empty(), |el| {
                        el.child(
                            div().px_2().py_1().text_size(px(12.)).text_color(theme.muted_foreground).child(
                                if menu.custom.is_none() && {
                                    let value = menu.input.read(cx).value();
                                    value.starts_with('#') || value.to_ascii_lowercase().starts_with("rgb")
                                } {
                                    "色值无效：请输入 #RRGGBB、#RRGGBBAA 或 rgba(r,g,b,0–1)"
                                } else {
                                    "没有匹配的颜色"
                                },
                            ),
                        )
                    }),
            ),
        )
        .into_any_element()
    }

    pub(super) fn preview_attrs(&self, id: BlockId, attrs: &aero_core::Attrs) -> aero_core::Attrs {
        let mut attrs = self
            .focused
            .as_ref()
            .filter(|f| f.id == Some(id) && f.range.is_empty())
            .and_then(|f| f.empty_attrs.clone())
            .unwrap_or_else(|| attrs.clone());
        if let Some(menu) = &self.color_menu
            && menu.revision == self.doc.version()
            && let Target::Blocks(ids) = &menu.target
            && ids.contains(&id)
            && let Some(color) = &menu.custom
        {
            menu.custom_slot.write(&mut attrs, Some(color));
        }
        attrs
    }

    pub(super) fn inline_color_preview(&self, range: &Range<usize>) -> Option<(Range<usize>, Slot, String)> {
        let menu = self.color_menu.as_ref().filter(|m| m.revision == self.doc.version())?;
        let Target::Inline(selected) = &menu.target else { return None };
        if menu.whole_block || selected.start < range.start || selected.end > range.end {
            return None;
        }
        Some((
            selected.start - range.start..selected.end - range.start,
            menu.custom_slot,
            menu.custom.clone()?,
        ))
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
            Slot::Border => 2,
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
            (Slot::Border, Some(color)) => glyph = glyph.border_1().border_color(color),
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
        self.color_menu
            .as_ref()
            .map(|menu| [menu.current[0].clone(), menu.current[1].clone()])
            .unwrap_or_default()
    }
}

/// 「上次使用」那一行显示的名字。
fn last_name(last: &crate::colors::Last) -> String {
    if colors::CustomColor::parse(&last.color).is_some() {
        return last.color.clone();
    }
    let ix = (0..color_menu::COUNT)
        .find(|&ix| {
            let item = color_menu::item(ix);
            item.slot == last.slot && item.color == Some(last.color.as_str())
        })
        .unwrap_or(0);
    color_menu::full_name(ix)
}

fn slot_index(slot: Slot) -> usize {
    match slot {
        Slot::Text => 0,
        Slot::Background => 1,
        Slot::Border => 2,
    }
}
