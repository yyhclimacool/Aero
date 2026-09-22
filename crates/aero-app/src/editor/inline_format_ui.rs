use super::*;

impl Editor {
    pub(super) fn on_format_inline(
        &mut self,
        action: &FormatInline,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.editing_with_keyboard(window, cx) || !self.format_inline(action.0, window, cx) {
            cx.propagate();
        }
    }

    /// 给选中的文字加 / 去一种行内格式。返回是否由编辑器处理（否则这次按键交还给文本控件）。
    pub(super) fn format_inline(
        &mut self,
        format: aero_core::inline_format::Format,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.focused.is_none() || self.title_edit.is_some() {
            return false;
        }
        let f = self.focused.as_ref().expect("已确认在编辑态");
        let selected = f.textarea.read(cx).selected_range();
        let selected = f.range.start + selected.start..f.range.start + selected.end;
        let Some(plan) = aero_core::inline_format::plan(&self.doc.to_string(), selected, format) else {
            return true;
        };
        self.slash = None;
        self.wiki = None;
        self.tag_menu = None;
        self.last_edit_at = None;
        self.structural_edit(plan.edit, plan.selection.end, window, cx);
        if let Some(f) = &self.focused
            && plan.selection.start >= f.range.start
            && plan.selection.end <= f.range.end
        {
            let selected = plan.selection.start - f.range.start..plan.selection.end - f.range.start;
            f.textarea.update(cx, |ta, cx| ta.set_selected_range(selected, cx));
        }
        if self.toolbar.is_some() {
            self.refresh_toolbar(cx);
        }
        cx.notify();
        true
    }
}
