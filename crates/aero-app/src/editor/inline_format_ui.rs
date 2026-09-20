use super::*;

impl Editor {
    pub(super) fn on_format_inline(
        &mut self,
        action: &FormatInline,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(f) = &self.focused else {
            cx.propagate();
            return;
        };
        if self.title_edit.is_some()
            || !f.textarea.focus_handle(cx).is_focused(window)
            || self.input_composing(window, cx)
        {
            cx.propagate();
            return;
        }
        let f = self.focused.as_ref().unwrap();
        let selected = f.textarea.read(cx).selected_range();
        let selected = f.range.start + selected.start..f.range.start + selected.end;
        let Some(plan) = aero_core::inline_format::plan(&self.doc.to_string(), selected, action.0) else {
            return;
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
        cx.notify();
    }
}
