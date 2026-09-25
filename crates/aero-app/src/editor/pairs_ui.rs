use super::*;

impl Editor {
    pub(super) fn observe_pair_key(
        &mut self,
        event: &KeystrokeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing_code() {
            return;
        }
        let Some(f) = &mut self.focused else { return };
        f.pairs.pending = None;
        if !f.textarea.focus_handle(cx).is_focused(window) || self.picker_open {
            return;
        }
        let modifiers = event.keystroke.modifiers;
        if modifiers.platform || modifiers.control || modifiers.alt || self.input_composing(window, cx) {
            return;
        }
        let key = event.keystroke.key_char.as_deref().unwrap_or(&event.keystroke.key);
        if !matches!(key, "*" | "=" | "[" | "]" | "(" | ")" | "\"" | "`" | "$" | "backspace") {
            return;
        }
        let f = self.focused.as_mut().unwrap();
        let ta = f.textarea.read(cx);
        f.pairs.pending = Some(pairs::Pending {
            key: key.to_owned(),
            selection: ta.selected_range(),
            before: ta.value().to_string(),
        });
    }

    /// 在 Document 接收修改前补齐原生输入；Document 只记录最终结果一次。
    pub(super) fn complete_pairs(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.focused.as_ref().is_none_or(|f| f.pairs.is_idle()) {
            return false;
        }
        if self.input_composing(window, cx) {
            if let Some(f) = &mut self.focused {
                f.pairs.pending = None;
            }
            return false;
        }
        let Some(f) = &mut self.focused else { return false };
        let old = self.doc.slice(f.range.clone()).to_string();
        let ta = f.textarea.read(cx);
        let new = ta.value().to_string();
        let selection = ta.selected_range();
        if let Some(plan) = f.pairs.complete(&old, &new, selection) {
            f.textarea.update(cx, |ta, cx| {
                if let Some(edit) = minimal_edit(0, &new, &plan.text) {
                    ta.set_selected_range(edit.range, cx);
                    ta.replace(edit.text, window, cx);
                }
                ta.set_selected_range(plan.cursor..plan.cursor, cx);
            });
            // 跳过只是光标移动，不产生保存或撤销记录；菜单仍需跟随光标关闭。
            self.last_edit_at = None;
            self.update_wiki(cx);
            self.update_tags(cx);
            return true;
        }
        if let Some(edit) = minimal_edit(0, &old, &new) {
            f.pairs.adjust(&edit);
        }
        false
    }
}
