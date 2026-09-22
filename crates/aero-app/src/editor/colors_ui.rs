//! 上色的写回入口（`ui_design.md` 7.7）。
//!
//! 现在只有 `⌘⇧H`「应用上次使用的颜色」这一条路：编辑态给选中的文字上色，块选中态给选中的块上色。
//! 颜色菜单与选区浮动工具条接上后调用的也是 [`Editor::apply_color`]。

use aero_core::colors::{self, Inline, Slot};

use super::*;

impl Editor {
    pub(super) fn on_apply_last_color(
        &mut self,
        _: &crate::actions::ApplyLastColor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let last = crate::colors::last(cx);
        if !self.apply_color(last.slot, Some(last.color), window, cx) {
            cx.propagate();
        }
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
        if !f.textarea.focus_handle(cx).is_focused(window) || self.input_composing(window, cx) {
            return false;
        }
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

    /// 选中的块：属性列表写在块末。多块选中时对每一块各写一次，合成一个撤销组（7.7）。
    fn color_selected_blocks(
        &mut self,
        slot: Slot,
        color: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(sel) = self.selection else { return false };
        let source = self.doc.to_string();
        // 从后往前：`edit_group` 顺序应用，后面的编辑不能挪动前面那一条的坐标
        let mut edits = Vec::new();
        for ix in (sel.first()..=sel.last()).rev() {
            let Some(unit) = self.units.get(ix) else { continue };
            let Some(block) = self.doc.blocks().find(unit.id) else { continue };
            let range = self.doc.range_of(block);
            if let Some(edit) = colors::block_edit(&source, range, slot, color) {
                edits.push(edit);
            }
        }
        if edits.is_empty() {
            return true;
        }
        if let Err(err) = self.doc.edit_group(edits) {
            log::error!("color edit rejected: {err}");
            return true;
        }
        self.last_edit_at = None;
        self.after_doc_change(cx);
        // 上色不改块结构，选区还是原来那几个下标
        if !self.units.is_empty() {
            let last_ix = self.units.len() - 1;
            self.select_blocks(sel.first().min(last_ix), sel.last().min(last_ix), window, cx);
        }
        true
    }
}
