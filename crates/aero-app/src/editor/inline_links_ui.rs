use super::*;

impl Editor {
    pub(super) fn on_edit_link(&mut self, _: &EditLink, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = &self.focused else {
            cx.propagate();
            return;
        };
        if self.title_edit.is_some() || !f.textarea.focus_handle(cx).is_focused(window) {
            cx.propagate();
            return;
        }
        if self.input_composing(window, cx) {
            cx.propagate();
            return;
        }
        let f = self.focused.as_ref().unwrap();
        let ta = f.textarea.read(cx);
        let selected = ta.selected_range();
        // 已在半成品 wikilink 中时重新打开现有菜单，不插入另一对括号。
        if selected.is_empty() && wiki::trigger(&ta.value(), ta.cursor()).is_some() {
            self.update_wiki(cx);
            return;
        }
        let selection = f.range.start + selected.start..f.range.start + selected.end;
        let clipboard =
            (!selection.is_empty()).then(|| cx.read_from_clipboard().and_then(|c| c.text())).flatten();
        let Some(plan) =
            aero_core::inline_links::plan(&self.doc.to_string(), selection, clipboard.as_deref())
        else {
            return;
        };
        self.slash = None;
        self.wiki = None;
        self.tag_menu = None;
        self.last_edit_at = None;
        if let Some(edit) = plan.edit {
            self.structural_edit(edit, plan.selection.end, window, cx);
        }
        if let Some(f) = &self.focused
            && plan.selection.start >= f.range.start
            && plan.selection.end <= f.range.end
        {
            let selection = plan.selection.start - f.range.start..plan.selection.end - f.range.start;
            f.textarea.update(cx, |ta, cx| ta.set_selected_range(selection, cx));
        }
        if plan.complete_note {
            self.update_wiki(cx);
        }
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }
}
