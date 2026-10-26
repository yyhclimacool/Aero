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

#[derive(Clone, PartialEq, Eq)]
struct ColorCacheKey {
    epoch: u64,
    revision: u64,
    target: Target,
    whole_block_id: Option<BlockId>,
}

#[derive(Clone)]
pub(super) struct ColorCache {
    key: ColorCacheKey,
    current: [Option<String>; 3],
    invalid: [[bool; 3]; 2],
}

/// 颜色菜单态（7.7）。
pub(super) struct ColorMenu {
    allowed: [bool; 3],
    epoch: u64,
    revision: u64,
    invalid: [[bool; 3]; 2],
    target: Target,
    picker: Entity<ColorPickerState>,
    custom: Option<String>,
    custom_slot: Slot,
    picker_unchanged: bool,
    picker_draft: Option<PickerDraft>,
    _picker_subscription: Subscription,
    _picker_input_subscription: Subscription,
    _picker_popup_subscription: Subscription,
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

struct PickerDraft {
    custom: Option<String>,
    slot: Slot,
    unchanged: bool,
    value: Option<Hsla>,
}

impl ColorMenu {
    fn effective_target(&self) -> Target {
        if self.whole_block && let Some(id) = self.whole_block_id {
            Target::Blocks(vec![id])
        } else {
            self.target.clone()
        }
    }
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
            // An unchanged confirmation must not reserialize unrelated color bytes.
            if attrs == block.attrs {
                continue;
            }
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
        self.ensure_current_parse();
        let explicit_blocks = blocks.is_some();
        let target = if let Some(ids) = blocks {
            Target::Blocks(ids)
        } else {
            match (&self.focused, self.selection) {
                (Some(f), _) => {
                    let selected = f.textarea.read(cx).selected_range();
                    let selection = f.range.start + selected.start..f.range.start + selected.end;
                    match selection.is_empty() {
                        // 编辑态没有选区：菜单改当前块（块菜单的"颜色 ▸"就是这个）
                        true => {
                            let Some(id) = self.focused_block_id() else { return false };
                            Target::Blocks(vec![id])
                        }
                        false => Target::Inline(selection),
                    }
                }
                (None, Some(sel)) => Target::Blocks((sel.first()..=sel.last())
                    .filter_map(|ix| self.units.get(ix))
                    .map(|u| u.id)
                    .collect()),
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
        let cached = self.capture_color_cache(&target, whole_block_id);
        let current = if !explicit_blocks && matches!(&target, Target::Blocks(ids) if ids.len() != 1) {
            [None, None, None]
        } else {
            cached.current
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
        let dark = cx.theme().mode.is_dark();
        let initial = current[1].as_deref().and_then(|raw| crate::colors::token(Slot::Background, raw, dark));
        let picker = cx.new(|cx| {
            let mut picker = ColorPickerState::new(window, cx);
            if let Some(color) = initial {
                picker.set_value(color, window, cx);
            }
            picker
        });
        let picker_subscription =
            cx.subscribe_in(&picker, window, |this, _, event: &ColorPickerEvent, _, cx| {
                let ColorPickerEvent::Change(value) = event;
                if let Some(menu) = &mut this.color_menu {
                    let current = menu.current[slot_index(menu.custom_slot)].as_deref();
                    let raw = menu.custom.as_deref().or(current);
                    let dark = cx.theme().mode.is_dark();
                    menu.picker_unchanged = raw == current && value.is_some()
                        && current.and_then(|raw| crate::colors::token(menu.custom_slot, raw, dark)) == *value;
                    menu.custom = value.map(|color| picker_color(color, raw, dark));
                }
                cx.notify();
            });
        self.slash = None;
        self.wiki = None;
        self.tag_menu = None;
        let picker_input = picker.read(cx).hex_input().clone();
        let picker_input_subscription = cx.subscribe_in(&picker_input, window, |this, input, event, _, cx| {
            if !matches!(event, InputEvent::Change) {
                return;
            }
            let Some(menu) = &mut this.color_menu else { return };
            if !menu.picker.read(cx).hex_dirty() {
                return;
            }
            let Some(raw) = colors::normalize_custom(input.read(cx).value().as_ref()) else { return };
            let dark = cx.theme().mode.is_dark();
            let color = crate::colors::token(menu.custom_slot, &raw, dark);
            if color != menu.picker.read(cx).displayed_color() {
                return;
            }
            // Keep typed channels before the picker reduces them to HSLA.
            menu.custom = Some(raw);
            menu.picker_unchanged = false;
            cx.notify();
        });
        let picker_popup_subscription = ColorPickerState::subscribe_popup(&picker, window, cx, |this, open, value, window, cx| {
            let Some(menu) = &mut this.color_menu else { return };
            if open {
                menu.picker_draft = Some(PickerDraft {
                    custom: menu.custom.clone(),
                    slot: menu.custom_slot,
                    unchanged: menu.picker_unchanged,
                    value,
                });
            } else {
                let Some(draft) = menu.picker_draft.take() else { return };
                menu.custom = draft.custom;
                menu.custom_slot = draft.slot;
                menu.picker_unchanged = draft.unchanged;
                menu.picker.update(cx, |picker, cx| match draft.value {
                    Some(value) => picker.set_value(value, window, cx),
                    None => picker.clear_value(window, cx),
                });
            }
            cx.notify();
        });
        self.color_menu = Some(ColorMenu {
            allowed,
            epoch: self.io.epoch,
            revision: self.doc.version(),
            invalid: cached.invalid,
            target,
            picker,
            custom: None,
            custom_slot: Slot::Background,
            picker_unchanged: false,
            picker_draft: None,
            _picker_subscription: picker_subscription,
            _picker_input_subscription: picker_input_subscription,
            _picker_popup_subscription: picker_popup_subscription,
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

    fn block_colors(&self, id: BlockId) -> [Option<String>; 3] {
        let Some(block) = self.doc.blocks().find(id) else { return [None, None, None] };
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
        if self.color_menu.as_ref().is_some_and(|menu| {
            let picker = menu.picker.read(cx);
            picker.is_open() && picker.hex_input().focus_handle(cx).is_focused(window)
        }) {
            window.dispatch_action(Box::new(gpui_kit::base::input::Enter { secondary: false, shift: false }), cx);
        } else if self.color_menu.is_some() {
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
                menu.picker_unchanged = false;
                if let Some(color) = menu.custom.as_deref().and_then(|raw| {
                    crate::colors::token(menu.custom_slot, raw, cx.theme().mode.is_dark())
                }) {
                    menu.picker.update(cx, |picker, cx| picker.set_value(color, window, cx));
                }
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
        if menu.epoch != self.io.epoch || menu.revision != self.doc.version() {
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
        let target = menu.effective_target();
        if !menu.allowed[slot_index(slot)] {
            return;
        }
        let unchanged = at.is_none() && menu.custom.is_some() && menu.picker_unchanged && !menu.whole_block;
        self.close_color_menu(window, cx);
        if unchanged {
            return;
        }
        match &target {
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

    fn capture_color_cache(&mut self, target: &Target, whole_block_id: Option<BlockId>) -> ColorCache {
        let key = ColorCacheKey {
            epoch: self.io.epoch,
            revision: self.doc.version(),
            target: target.clone(),
            whole_block_id,
        };
        if let Some(cached) = &self.color_cache && cached.key == key {
            return cached.clone();
        }
        #[cfg(test)]
        COLOR_ANALYSES.with(|count| count.set(count.get() + 1));
        let mut cached = ColorCache { key, current: [None, None, None], invalid: [[false; 3]; 2] };
        match target {
            Target::Blocks(ids) => {
                cached.current = ids.first().map(|id| self.block_colors(*id)).unwrap_or_default();
                cached.invalid[0] = self.invalid_block_colors(ids);
            }
            Target::Inline(selected) => {
                if let Some(f) = &self.focused && !self.editing_code() {
                    // Retain block context without copying or parsing the entire document.
                    let range = self.focused_block_id().and_then(|id| self.doc.blocks().find(id))
                        .map(|block| self.doc.range_of(block))
                        .filter(|range| range.start <= selected.start && selected.end <= range.end)
                        .unwrap_or_else(|| f.range.clone());
                    let source = self.doc.slice(range.clone()).to_string();
                    let selected = selected.start - range.start..selected.end - range.start;
                    let read = |slot| colors::inline_color(&source, selected.clone(), slot);
                    cached.current = [read(Slot::Text), read(Slot::Background), None];
                    let analysis = self.body_analysis(f.id, &source, None);
                    for (range, slot) in analysis.invalid_colors {
                        if range.start < selected.end && selected.start < range.end {
                            cached.invalid[0][slot_index(slot)] = true;
                        }
                    }
                }
            }
        }
        if let Some(id) = whole_block_id {
            cached.invalid[1] = self.invalid_block_colors(&[id]);
        }
        self.color_cache = Some(cached.clone());
        cached
    }

    fn invalid_block_colors(&self, ids: &[BlockId]) -> [bool; 3] {
        let mut invalid = [false; 3];
        for id in ids {
            if let Some(block) = self.doc.blocks().find(*id) {
                for slot in [Slot::Text, Slot::Background, Slot::Border] {
                    invalid[slot_index(slot)] |= block.attrs.pairs.iter().any(|(key, value)| {
                        key == slot.key() && colors::CustomColor::parse(value).is_none()
                    });
                }
            }
        }
        invalid
    }

    fn invalid_color_slots(&self, menu: &ColorMenu) -> [bool; 3] {
        if menu.epoch != self.io.epoch || menu.revision != self.doc.version() {
            return [false; 3];
        }
        menu.invalid[usize::from(menu.effective_target() != menu.target)]
    }

    /// 菜单浮层：锚在光标 / 当前块下面，条目按 7.7 的两组网格排，样本用真实颜色画 `A`。
    pub(super) fn render_color_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(menu) = self.color_menu.as_ref() else { return div().into_any_element() };
        let theme = cx.theme().clone();
        let dark = theme.mode.is_dark();
        let last = crate::colors::last(cx);
        let rows = color_menu::rows(&menu.matches);
        let invalid = self.invalid_color_slots(menu);
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
                                        menu.picker_unchanged = false;
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
                    .when(invalid.iter().any(|invalid| *invalid), |el| {
                        let names = ["文字", "背景", "边框"].into_iter().enumerate()
                            .filter_map(|(i, name)| invalid[i].then_some(name)).collect::<Vec<_>>().join("、");
                        el.child(div().id("color-invalid-source")
                            .debug_selector(|| "color-invalid-source".to_owned())
                            .px_2().py_1().text_xs().text_color(theme.muted_foreground)
                            .child(format!("旧{names}色值无效，已忽略；选择新颜色可替换")))
                    })
                    .child(h_flex().gap_1().px_1().child(ColorPicker::new(&menu.picker).label("自定义颜色")))
                    .child(
                        h_flex().gap_1().px_1().children(
                            [("文字", Slot::Text), ("背景", Slot::Background), ("边框", Slot::Border)]
                                .into_iter()
                                .enumerate()
                                .map(|(i, (label, slot))| {
                                    div()
                                        .id(("custom-color-slot", i))
                                        .debug_selector(move || format!("custom-color-slot-{i}"))
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
                                            cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                if let Some(menu) = &mut this.color_menu
                                                    && menu.allowed[slot_index(slot)]
                                                {
                                                    menu.custom_slot = slot;
                                                    menu.picker_unchanged = false;
                                                    if menu.custom.is_none() {
                                                        let color = menu.current[slot_index(slot)].as_deref()
                                                            .and_then(|raw| crate::colors::token(slot, raw, cx.theme().mode.is_dark()));
                                                        menu.picker.update(cx, |picker, cx| match color {
                                                            Some(color) => picker.set_value(color, window, cx),
                                                            None => picker.clear_value(window, cx),
                                                        });
                                                    }
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
            && let Target::Blocks(ids) = menu.effective_target()
            && ids.contains(&id)
            && let Some(color) = &menu.custom
        {
            menu.custom_slot.write(&mut attrs, Some(color));
        }
        attrs
    }

    pub(super) fn inline_color_preview(&self, range: &Range<usize>) -> Option<(Range<usize>, Slot, String)> {
        let menu = self.color_menu.as_ref().filter(|m| m.revision == self.doc.version())?;
        let Target::Inline(selected) = menu.effective_target() else { return None };
        if selected.start < range.start || selected.end > range.end {
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

#[cfg(test)]
thread_local! {
    static COLOR_ANALYSES: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::{Editor, Slot, colors, presenter};
    use gpui_kit::{Entity, Hsla, Styled as _, TestAppContext, VisualTestContext};
    use gpui_kit::component::ActiveTheme as _;

    fn select(editor: &Entity<Editor>, needle: &str, cx: &mut VisualTestContext) {
        cx.update(|window, cx| editor.update(cx, |e, cx| {
            e.focus_block(e.units[0].id, window, cx);
            e.focused.as_ref().unwrap().textarea.update(cx, |ta, cx| {
                let start = ta.value().find(needle).unwrap();
                ta.set_selected_range(start..start + needle.len(), cx);
            });
        }));
        cx.run_until_parked();
    }

    fn open_menu(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
        cx.update(|window, cx| editor.update(cx, |e, cx| {
            assert!(e.open_color_menu(window, cx));
        }));
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn colors_effective_palette_paint_matches_preview(cx: &mut TestAppContext) {
        let (_dir, _vault, _path, editor) = super::super::tests::open(cx, "word\n");
        cx.update(|cx| {
            let e = editor.read(cx);
            for (attrs, text, background) in [
                (".red .blue .bg-green .bg-pink", "red", "green"),
                (".red color=\"#2457A780\" color=bad .bg-green background=\"rgba(1,2,3,0.37500001)\"", "#2457A780", "rgba(1,2,3,0.37500001)"),
                ("color=\"#111111\" color=\"#2457A7\" .bg-blue", "#2457A7", "blue"),
            ] {
                let source = format!("[word]{{{attrs}}}");
                let analysis = aero_core::markup::analyze(&source);
                let markup = e.presenter_markup(&source, &analysis, &presenter::Extra::default(), cx);
                let paint = markup.styles.iter().find(|(range, _)| &source[range.clone()] == "word").unwrap();
                assert_eq!(paint.1.color, crate::colors::token(Slot::Text, text, false));
                assert_eq!(paint.1.background_color, crate::colors::token(Slot::Background, background, false));
            }
        });
    }

    #[gpui_kit::test]
    fn colors_whole_block_preview_cancel_and_one_undo(cx: &mut TestAppContext) {
        let source = "word\n\nother\n";
        let (_dir, _vault, _path, editor) = super::super::tests::open(cx, source);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        select(&editor, "word", cx);
        for confirm in [false, true] {
            open_menu(&editor, cx);
            cx.simulate_input("#2457A780");
            cx.update(|_, cx| editor.update(cx, |e, _| {
                let menu = e.color_menu.as_mut().unwrap();
                menu.whole_block = true;
                let id = menu.whole_block_id.unwrap();
                let attrs = e.preview_attrs(id, &e.doc.blocks().find(id).unwrap().attrs);
                assert_eq!(Slot::Background.color_of(&attrs), Some("#2457A780"));
                assert!(e.inline_color_preview(&(0..4)).is_none());
                assert_eq!(e.doc.to_string(), source);
                assert_eq!(e.preview_attrs(e.units[1].id, &aero_core::Attrs::default()), aero_core::Attrs::default());
            }));
            cx.simulate_keystrokes(if confirm { "enter" } else { "escape" });
            cx.update(|_, cx| {
                let e = editor.read(cx);
                assert!(e.color_menu.is_none());
                if confirm {
                    assert_eq!(e.doc.to_string(), "word {background=\"#2457A780\"}\n\nother\n");
                } else {
                    assert_eq!(e.doc.to_string(), source);
                    assert_eq!(e.preview_attrs(e.units[0].id, &aero_core::Attrs::default()), aero_core::Attrs::default());
                }
            });
        }
        cx.simulate_keystrokes("cmd-z");
        cx.update(|_, cx| assert_eq!(editor.read(cx).doc.to_string(), source));
    }

    #[gpui_kit::test]
    fn colors_picker_preserves_raw_precision_on_unchanged_events(cx: &mut TestAppContext) {
        let (_dir, _vault, _path, editor) = super::super::tests::open(cx, "word\n");
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        select(&editor, "word", cx);
        for raw in ["#2457A780", "#2457A7FF", "rgba(44,44,43,0.375000000123)"] {
            open_menu(&editor, cx);
            cx.simulate_input(raw);
            let picker = cx.update(|_, cx| editor.read(cx).color_menu.as_ref().unwrap().picker.clone());
            let color = crate::colors::token(Slot::Background, raw, false).unwrap();
            cx.update(|window, cx| picker.update(cx, |p, cx| p.select_color(color, window, cx)));
            cx.run_until_parked();
            cx.update(|_, cx| assert_eq!(editor.read(cx).color_menu.as_ref().unwrap().custom.as_deref(), Some(raw)));
            cx.simulate_keystrokes("escape");
        }
    }

    fn open_hex_input(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            let picker = editor.read(cx).color_menu.as_ref().unwrap().picker.clone();
            picker.update(cx, |picker, cx| {
                picker.set_active_tab(1, cx);
                picker.set_open(true, cx);
            });
            window.draw(cx).clear(cx);
            let input = picker.read(cx).hex_input().clone();
            input.update(cx, |input, cx| input.focus(window, cx));
        });
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn colors_picker_native_untouched_hex_enter_and_confirm_is_no_edit(cx: &mut TestAppContext) {
        let source = "word {background=\"rgba(0,0,0,0.375000000123)\" custom=keep}\n";
        let (_dir, _vault, _path, editor) = super::super::tests::open(cx, source);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| editor.update(cx, |e, cx| {
            e.focus_block(e.units[0].id, window, cx);
        }));
        let revision = cx.update(|_, cx| editor.read(cx).doc.version());
        for _ in 0..2 {
            open_menu(&editor, cx);
            open_hex_input(&editor, cx);
            cx.simulate_keystrokes("enter");
            cx.run_until_parked();
            cx.update(|_, cx| assert!(!editor.read(cx).color_menu.as_ref().unwrap().picker.read(cx).is_open()));
            cx.simulate_keystrokes("enter");
            cx.update(|_, cx| {
                let e = editor.read(cx);
                assert!(e.color_menu.is_none());
                assert_eq!(e.doc.to_string(), source);
                assert_eq!(e.doc.version(), revision);
                assert!(!e.doc.can_undo());
            });
        }
    }

    #[gpui_kit::test]
    fn colors_picker_native_hex_input_keeps_all_eight_channels(cx: &mut TestAppContext) {
        let (_dir, _vault, _path, editor) = super::super::tests::open(cx, "word\n");
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        for raw in ["#2457A780", "#2457A7FF"] {
            select(&editor, "word", cx);
            open_menu(&editor, cx);
            cx.update(|window, cx| {
                let picker = editor.read(cx).color_menu.as_ref().unwrap().picker.clone();
                picker.update(cx, |picker, cx| picker.set_value(Hsla::default(), window, cx));
            });
            open_hex_input(&editor, cx);
            cx.simulate_keystrokes("cmd-a");
            cx.simulate_input(raw);
            cx.simulate_keystrokes("enter");
            cx.run_until_parked();
            cx.update(|_, cx| {
                let menu = editor.read(cx).color_menu.as_ref().unwrap();
                assert!(!menu.picker.read(cx).is_open());
                assert_eq!(menu.custom.as_deref(), Some(raw));
            });
            cx.simulate_keystrokes("enter");
            cx.update(|_, cx| assert_eq!(editor.read(cx).doc.to_string(), format!("[word]{{background=\"{raw}\"}}\n")));
            cx.simulate_keystrokes("cmd-z");
            cx.update(|_, cx| assert_eq!(editor.read(cx).doc.to_string(), "word\n"));
        }
    }

    #[gpui_kit::test]
    fn colors_picker_rgba_to_opaque_hex8_native_enter_writes_once(cx: &mut TestAppContext) {
        let source = "word {background=\"rgba(0,0,0,0.375000000123)\" custom=keep}\n";
        let (_dir, _vault, _path, editor) = super::super::tests::open(cx, source);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| editor.update(cx, |e, cx| {
            e.focus_block(e.units[0].id, window, cx);
        }));
        open_menu(&editor, cx);
        open_hex_input(&editor, cx);
        cx.simulate_keystrokes("cmd-a");
        cx.simulate_input("#FF0000FF");
        cx.update(|_, cx| {
            let e = editor.read(cx);
            assert_eq!(e.color_menu.as_ref().unwrap().custom.as_deref(), Some("#FF0000FF"));
            assert_eq!(e.doc.to_string(), source);
            assert!(!e.doc.can_undo());
        });
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        cx.update(|_, cx| {
            let menu = editor.read(cx).color_menu.as_ref().unwrap();
            assert!(!menu.picker.read(cx).is_open());
            assert_eq!(menu.custom.as_deref(), Some("#FF0000FF"));
        });
        cx.update(|_, cx| {
            let picker = editor.read(cx).color_menu.as_ref().unwrap().picker.clone();
            let input = picker.read(cx).hex_input().clone();
            assert_eq!(input.read(cx).value().as_str(), "#FF0000");
            input.update(cx, |_, cx| cx.emit(super::InputEvent::Change));
        });
        cx.update(|_, cx| {
            assert_eq!(editor.read(cx).color_menu.as_ref().unwrap().custom.as_deref(), Some("#FF0000FF"));
        });
        cx.simulate_keystrokes("enter");
        cx.update(|_, cx| {
            let e = editor.read(cx);
            assert!(e.color_menu.is_none());
            assert_eq!(e.doc.to_string(), "word {custom=keep background=\"#FF0000FF\"}\n");
            assert!(e.doc.can_undo());
        });
        cx.simulate_keystrokes("cmd-z");
        cx.update(|_, cx| {
            let e = editor.read(cx);
            assert_eq!(e.doc.to_string(), source);
            assert!(!e.doc.can_undo());
        });
    }

    #[gpui_kit::test]
    fn colors_picker_popup_cancel_then_outer_confirm_and_reopen_is_no_edit(cx: &mut TestAppContext) {
        let source = "word {background=\"rgba(0,0,0,0.375000000123)\" custom=keep}\n";
        let (_dir, _vault, _path, editor) = super::super::tests::open(cx, source);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| editor.update(cx, |e, cx| {
            e.focus_block(e.units[0].id, window, cx);
        }));
        let revision = cx.update(|_, cx| editor.read(cx).doc.version());
        for reopen in [false, true] {
            open_menu(&editor, cx);
            open_hex_input(&editor, cx);
            cx.simulate_keystrokes("cmd-a");
            cx.simulate_input("#FF0000FF");
            cx.update(|_, cx| {
                let e = editor.read(cx);
                assert_eq!(e.color_menu.as_ref().unwrap().custom.as_deref(), Some("#FF0000FF"));
                assert_eq!(e.doc.to_string(), source);
            });
            cx.update(|_, cx| {
                let picker = editor.read(cx).color_menu.as_ref().unwrap().picker.clone();
                picker.update(cx, |picker, cx| picker.set_open(false, cx));
            });
            cx.run_until_parked();
            cx.update(|_, cx| {
                let e = editor.read(cx);
                let menu = e.color_menu.as_ref().unwrap();
                assert!(menu.custom.is_none());
                assert_eq!(menu.picker.read(cx).value(), crate::colors::token(Slot::Background, "rgba(0,0,0,0.375000000123)", false));
                let block = e.doc.blocks().find(e.units[0].id).unwrap();
                assert_eq!(e.preview_attrs(block.id, &block.attrs), block.attrs);
            });
            if reopen {
                open_hex_input(&editor, cx);
                cx.simulate_keystrokes("enter");
            }
            cx.update(|window, cx| {
                let input = editor.read(cx).color_menu.as_ref().unwrap().input.clone();
                input.update(cx, |input, cx| input.focus(window, cx));
            });
            cx.simulate_keystrokes("enter");
            cx.update(|_, cx| {
                let e = editor.read(cx);
                assert!(e.color_menu.is_none());
                assert_eq!(e.doc.to_string(), source);
                assert_eq!(e.doc.version(), revision);
                assert!(!e.doc.can_undo());
            });
        }
    }

    #[gpui_kit::test]
    fn colors_picker_cancel_restores_accepted_draft_before_outer_apply(cx: &mut TestAppContext) {
        let raw = "rgba(0,0,0,0.375000000123)";
        let source = format!("word {{background=\"{raw}\" custom=keep}}\n");
        let (_dir, _vault, _path, editor) = super::super::tests::open(cx, &source);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| editor.update(cx, |e, cx| {
            e.focus_block(e.units[0].id, window, cx);
        }));
        for apply in [false, true] {
            open_menu(&editor, cx);
            open_hex_input(&editor, cx);
            cx.simulate_keystrokes("enter");
            open_hex_input(&editor, cx);
            cx.simulate_keystrokes("cmd-a");
            cx.simulate_input("#FF0000FF");
            cx.update(|window, cx| {
                window.dispatch_action(Box::new(gpui_kit::base::actions::Cancel), cx);
            });
            cx.run_until_parked();
            cx.update(|_, cx| {
                let menu = editor.read(cx).color_menu.as_ref().unwrap();
                assert!(!menu.picker.read(cx).is_open());
                assert_eq!(menu.custom.as_deref(), Some(raw));
                assert!(menu.picker_unchanged);
                assert_eq!(menu.custom_slot, Slot::Background);
            });
            cx.update(|window, cx| {
                let input = editor.read(cx).color_menu.as_ref().unwrap().input.clone();
                input.update(cx, |input, cx| input.focus(window, cx));
            });
            cx.simulate_keystrokes(if apply { "enter" } else { "escape" });
            cx.update(|_, cx| {
                let e = editor.read(cx);
                assert!(e.color_menu.is_none());
                assert_eq!(e.doc.to_string(), source);
                assert!(!e.doc.can_undo());
            });
        }
    }

    #[gpui_kit::test]
    fn colors_picker_slider_cancel_restores_prior_custom_draft_and_slot(cx: &mut TestAppContext) {
        let source = "word {background=\"rgba(0,0,0,0.375000000123)\" custom=keep}\n";
        let (_dir, _vault, _path, editor) = super::super::tests::open(cx, source);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| editor.update(cx, |e, cx| {
            e.focus_block(e.units[0].id, window, cx);
        }));
        open_menu(&editor, cx);
        cx.simulate_input("#00FF00FF");
        open_hex_input(&editor, cx);
        cx.update(|window, cx| {
            let picker = editor.read(cx).color_menu.as_ref().unwrap().picker.clone();
            picker.update(cx, |picker, cx| {
                picker.update_color(crate::colors::token(Slot::Background, "#FF0000FF", false).unwrap(), window, cx);
            });
        });
        cx.update(|_, cx| editor.update(cx, |e, cx| {
            let menu = e.color_menu.as_mut().unwrap();
            menu.custom_slot = Slot::Text;
            menu.picker.update(cx, |picker, cx| picker.set_open(false, cx));
        }));
        cx.update(|_, cx| {
            let menu = editor.read(cx).color_menu.as_ref().unwrap();
            assert_eq!(menu.custom.as_deref(), Some("#00FF00FF"));
            assert_eq!(menu.custom_slot, Slot::Background);
            assert!(!menu.picker_unchanged);
            assert_eq!(menu.picker.read(cx).value(), crate::colors::token(Slot::Background, "#00FF00FF", false));
        });
        open_hex_input(&editor, cx);
        cx.simulate_keystrokes("enter enter");
        cx.update(|_, cx| {
            let e = editor.read(cx);
            assert!(e.color_menu.is_none());
            assert_eq!(e.doc.to_string(), "word {custom=keep background=\"#00FF00FF\"}\n");
        });
        cx.simulate_keystrokes("cmd-z");
        cx.update(|_, cx| {
            let e = editor.read(cx);
            assert_eq!(e.doc.to_string(), source);
            assert!(!e.doc.can_undo());
        });
    }

    #[gpui_kit::test]
    fn colors_picker_rgb_changes_keep_original_alpha_representation(cx: &mut TestAppContext) {
        let (_dir, _vault, _path, editor) = super::super::tests::open(cx, "word\n");
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        select(&editor, "word", cx);
        for raw in ["rgba(44,44,43,0.375000000123)", "#2457A780"] {
            open_menu(&editor, cx);
            cx.simulate_input(raw);
            let picker = cx.update(|_, cx| editor.read(cx).color_menu.as_ref().unwrap().picker.clone());
            let color = crate::colors::token(Slot::Background, raw, false).unwrap();
            let changed = Hsla { h: (color.h + 0.2) % 1., ..color };
            cx.update(|window, cx| picker.update(cx, |p, cx| p.update_color(changed, window, cx)));
            cx.run_until_parked();
            cx.update(|_, cx| {
                let e = editor.read(cx);
                let value = e.color_menu.as_ref().unwrap().custom.as_deref().unwrap();
                assert_ne!(value, raw);
                if raw.starts_with('#') {
                    assert!(value.starts_with('#') && value.len() == 9 && value.ends_with("80"), "{value}");
                } else {
                    assert!(value.ends_with(",0.375000000123)"), "{value}");
                }
            });
            cx.simulate_keystrokes("escape");
        }
    }

    #[gpui_kit::test]
    fn colors_picker_format_only_is_no_edit_and_real_alpha_change_is_not_rounded(cx: &mut TestAppContext) {
        let raw = "rgba(44,44,43,0.375000000123)";
        let source = format!("word {{background=\"{raw}\"}}\n");
        let (_dir, _vault, _path, editor) = super::super::tests::open(cx, &source);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| editor.update(cx, |e, cx| {
            e.focus_block(e.units[0].id, window, cx);
        }));
        open_menu(&editor, cx);
        let picker = cx.update(|_, cx| editor.read(cx).color_menu.as_ref().unwrap().picker.clone());
        let color = crate::colors::token(Slot::Background, raw, false).unwrap();
        cx.update(|_, cx| assert_eq!(picker.read(cx).value(), Some(color)));
        cx.update(|window, cx| picker.update(cx, |p, cx| {
            p.set_active_tab(1, cx);
            p.set_active_tab(0, cx);
            p.select_color(color, window, cx);
        }));
        cx.run_until_parked();
        cx.update(|window, cx| editor.update(cx, |e, cx| e.confirm_color_menu(None, window, cx)));
        cx.update(|_, cx| assert_eq!(editor.read(cx).doc.to_string(), source));
        open_menu(&editor, cx);
        let picker = cx.update(|_, cx| editor.read(cx).color_menu.as_ref().unwrap().picker.clone());
        let changed = Hsla { a: f32::from_bits(color.a.to_bits() + 1), ..color };
        cx.update(|window, cx| picker.update(cx, |p, cx| p.update_color(changed, window, cx)));
        cx.run_until_parked();
        cx.update(|_, cx| {
            let e = editor.read(cx);
            let value = e.color_menu.as_ref().unwrap().custom.as_deref().unwrap();
            assert_ne!(value, raw);
            assert_eq!(colors::CustomColor::parse(value).unwrap().alpha, changed.a);
            assert_eq!(e.doc.to_string(), source);
        });
        cx.update(|window, cx| editor.update(cx, |e, cx| e.confirm_color_menu(None, window, cx)));
        cx.simulate_keystrokes("cmd-z");
        cx.update(|_, cx| assert_eq!(editor.read(cx).doc.to_string(), source));
    }

    #[gpui_kit::test]
    fn colors_picker_channel_switch_loads_existing_hex8_without_edit(cx: &mut TestAppContext) {
        let source = "word {color=\"#2457a780\" background=\"#ABCDEF\" custom=keep}\n";
        let (_dir, _vault, _path, editor) = super::super::tests::open(cx, source);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| editor.update(cx, |e, cx| {
            e.focus_block(e.units[0].id, window, cx);
        }));
        open_menu(&editor, cx);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let slot = cx.debug_bounds("custom-color-slot-0").unwrap();
        cx.simulate_click(slot.center(), Default::default());
        cx.run_until_parked();
        let picker = cx.update(|_, cx| editor.read(cx).color_menu.as_ref().unwrap().picker.clone());
        let color = crate::colors::token(Slot::Text, "#2457a780", false).unwrap();
        cx.update(|_, cx| assert_eq!(picker.read(cx).value(), Some(color)));
        cx.update(|window, cx| picker.update(cx, |p, cx| {
            p.set_active_tab(1, cx);
            p.select_color(color, window, cx);
        }));
        cx.run_until_parked();
        cx.update(|window, cx| editor.update(cx, |e, cx| e.confirm_color_menu(None, window, cx)));
        cx.update(|_, cx| assert_eq!(editor.read(cx).doc.to_string(), source));
    }

    #[gpui_kit::test]
    fn colors_inline_custom_commit_save_reopen_matches_preview(cx: &mut TestAppContext) {
        let (dir, vault, path, editor) = super::super::tests::open(cx, "before word after\n");
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        select(&editor, "word", cx);
        open_menu(&editor, cx);
        cx.simulate_input("#2457A780");
        cx.update(|window, cx| editor.update(cx, |e, cx| {
            e.color_menu.as_mut().unwrap().custom_slot = Slot::Text;
            e.confirm_color_menu(None, window, cx);
        }));
        cx.executor().advance_clock(std::time::Duration::from_millis(600));
        cx.run_until_parked();
        let saved = std::fs::read_to_string(dir.path().join("a.md")).unwrap();
        assert_eq!(saved, "before [word]{color=\"#2457A780\"} after\n");
        let (reopened, _) = Editor::load(vault, path).unwrap();
        cx.update(|_, cx| {
            let e = editor.read(cx);
            for source in [e.doc.to_string(), reopened.to_string()] {
                let analysis = aero_core::markup::analyze(&source);
                let markup = e.presenter_markup(&source, &analysis, &presenter::Extra::default(), cx);
                assert!(markup.styles.iter().any(|(range, style)| {
                    &source[range.clone()] == "word"
                        && style.color == crate::colors::token(Slot::Text, "#2457A780", false)
                }));
            }
        });
        open_menu(&editor, cx);
        cx.update(|_, cx| assert_eq!(editor.read(cx).color_menu.as_ref().unwrap().current[0].as_deref(), Some("#2457A780")));
    }

    #[gpui_kit::test]
    fn colors_list_item_default_does_not_leak_to_deeper_text(cx: &mut TestAppContext) {
        let source = "- {aero-owner=item .red}\n\n  ~~direct~~\n\n  - ~~nested~~\n\n  > ~~quote~~\n\n  :::highlight\n  ~~highlight~~\n\n  ~~explicit~~ {.green}\n  :::\n";
        let (_dir, _vault, _path, editor) = super::super::tests::open(cx, source);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.run_until_parked();
        cx.update(|_, cx| editor.update(cx, |e, cx| {
            let blocks: Vec<_> = e.doc.blocks().iter().map(|(_, b)| b.clone()).collect();
            let red = crate::colors::token(Slot::Text, "red", false).unwrap();
            let mut direct_count = 0;
            let mut nested_count = 0;
            for block in blocks.iter().filter(|b| matches!(b.kind, aero_core::BlockKind::Paragraph | aero_core::BlockKind::BlockQuote | aero_core::BlockKind::Directive { .. } | aero_core::BlockKind::List { .. })) {
                let ancestors = aero_core::containers::ancestors(&e.doc, block.id);
                let direct = matches!(block.kind, aero_core::BlockKind::Paragraph) && ancestors.len() == 2;
                let inherited = direct.then_some(red);
                let expected = crate::colors::of(&block.attrs, cx).text.or(inherited).unwrap_or(cx.theme().foreground);
                let mut rendered = e.render_block(block, 0, inherited, cx).unwrap();
                let boundary = rendered.downcast_mut::<gpui_kit::Div>().expect("each block needs a foreground inheritance boundary");
                assert_eq!(boundary.style().text.color, Some(expected), "{:?}", block.kind);
                direct_count += usize::from(direct);
                nested_count += usize::from(!direct);
            }
            assert_eq!(direct_count, 1);
            assert!(nested_count >= 5);
        }));
    }

    #[gpui_kit::test]
    fn colors_invalid_analysis_reused_across_real_menu_interactions(cx: &mut TestAppContext) {
        let source = "**word**{color=\"#oops\"} after\n\nother\n";
        let (_dir, _vault, _path, editor) = super::super::tests::open(cx, source);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        select(&editor, "**word**{color=\"#oops\"} after", cx);
        open_menu(&editor, cx);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("color-invalid-source").is_some());
        let captured = super::COLOR_ANALYSES.with(|count| count.get());
        assert!(captured > 0);
        for _ in 0..3 {
            let sample = cx.debug_bounds("color-sample-红色").unwrap();
            cx.simulate_mouse_move(sample.center(), None, Default::default());
            cx.simulate_keystrokes("right left");
            cx.update(|window, cx| window.draw(cx).clear(cx));
            assert_eq!(super::COLOR_ANALYSES.with(|count| count.get()), captured);
        }
        let switch = cx.debug_bounds("color-apply-block").unwrap();
        cx.simulate_click(switch.center(), Default::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("color-invalid-source").is_none());
        cx.simulate_click(switch.center(), Default::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("color-invalid-source").is_some());
        cx.simulate_keystrokes("escape");
        open_menu(&editor, cx);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(super::COLOR_ANALYSES.with(|count| count.get()), captured);
        cx.simulate_keystrokes("escape");
        select(&editor, "after", cx);
        open_menu(&editor, cx);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("color-invalid-source").is_none());
        assert!(super::COLOR_ANALYSES.with(|count| count.get()) > captured);
        cx.simulate_keystrokes("escape");
        select(&editor, "word", cx);
        open_menu(&editor, cx);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let before_change = super::COLOR_ANALYSES.with(|count| count.get());
        cx.simulate_keystrokes("escape");
        select(&editor, "#oops", cx);
        cx.simulate_input("#123456");
        select(&editor, "word", cx);
        open_menu(&editor, cx);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("color-invalid-source").is_none());
        assert!(super::COLOR_ANALYSES.with(|count| count.get()) > before_change);
        cx.simulate_keystrokes("escape");
        cx.update(|_, cx| editor.update(cx, |e, cx| e.reload(source.as_bytes(), e.stamp, cx)));
        select(&editor, "word", cx);
        let before_reload = super::COLOR_ANALYSES.with(|count| count.get());
        open_menu(&editor, cx);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("color-invalid-source").is_some());
        assert!(super::COLOR_ANALYSES.with(|count| count.get()) > before_reload);
        cx.simulate_keystrokes("escape");
        let captured = super::COLOR_ANALYSES.with(|count| count.get());
        let revision = cx.update(|_, cx| editor.read(cx).doc.version());
        cx.update(|_, cx| editor.update(cx, |e, cx| {
            e.reload(source.replace("#oops", "#123456").as_bytes(), e.stamp, cx);
            assert_eq!(e.doc.version(), revision);
        }));
        select(&editor, "word", cx);
        open_menu(&editor, cx);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("color-invalid-source").is_none());
        assert!(super::COLOR_ANALYSES.with(|count| count.get()) > captured);
    }

    #[gpui_kit::test]
    fn colors_invalid_inline_attribute_has_notice_without_source_edit(cx: &mut TestAppContext) {
        let source = "before **word**{color=\"#oops\" .bg-blue} after\n";
        let (_dir, _vault, _path, editor) = super::super::tests::open(cx, source);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        select(&editor, "word", cx);
        open_menu(&editor, cx);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("color-invalid-source").is_some());
        cx.update(|_, cx| assert_eq!(editor.read(cx).doc.to_string(), source));
    }

    #[gpui_kit::test]
    fn colors_invalid_old_attribute_has_notice_without_source_edit(cx: &mut TestAppContext) {
        let source = "word {color=\"#oops\" .red background=\"rgba(1,2,3,2)\"}\n";
        let (_dir, _vault, _path, editor) = super::super::tests::open(cx, source);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| editor.update(cx, |e, cx| {
            let id = e.units[0].id;
            assert!(e.open_color_menu_for_blocks(vec![id], window, cx));
        }));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("color-invalid-source").is_some());
        cx.update(|_, cx| assert_eq!(editor.read(cx).doc.to_string(), source));
    }
}

fn picker_color(color: Hsla, raw: Option<&str>, dark: bool) -> String {
    if let Some(raw) = raw
        && crate::colors::token(Slot::Background, raw, dark) == Some(color)
    {
        return raw.to_owned();
    }
    let rgba = gpui_kit::Rgba::from(color);
    let byte = |value: f32| (value * 255.).round().clamp(0., 255.) as u8;
    let (red, green, blue) = (byte(rgba.r), byte(rgba.g), byte(rgba.b));
    let original = raw.and_then(colors::CustomColor::parse);
    if raw.is_some_and(|raw| raw.starts_with('#') && raw.len() == 9)
        && byte(rgba.a) as f32 / 255. == rgba.a
    {
        return format!("#{red:02X}{green:02X}{blue:02X}{:02X}", byte(rgba.a));
    }
    if rgba.a < 1. || raw.is_some_and(|raw| raw.starts_with("rgba(")) {
        let alpha = if original.is_some_and(|original| original.alpha == rgba.a) {
            raw.and_then(|raw| raw.strip_suffix(')'))
                .and_then(|raw| raw.rsplit(',').next())
                .map(str::trim)
                .map(str::to_owned)
                .unwrap_or_else(|| rgba.a.to_string())
        } else {
            rgba.a.to_string()
        };
        format!("rgba({red},{green},{blue},{alpha})")
    } else {
        format!("#{red:02X}{green:02X}{blue:02X}")
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
