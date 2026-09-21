use super::*;
use crate::backlinks::BacklinksView;

impl Editor {
    pub fn connect_backlinks(&mut self, view: Entity<BacklinksView>, cx: &mut Context<Self>) {
        self.backlinks_subscription = Some(cx.observe(&view, |_, _, cx| cx.notify()));
        let editor = cx.entity();
        let selected_block = self.selected_reference(cx);
        view.update(cx, |view, cx| view.set_editor(&editor, selected_block, cx));
        self.backlinks = Some(view);
    }

    pub fn selected_reference(&self, cx: &App) -> Option<String> {
        let block = if let Some(focused) = &self.focused {
            let offset = focused.range.start + focused.textarea.read(cx).selected_range().end;
            self.doc.block_at(offset.min(focused.range.end.saturating_sub(1)))?
        } else {
            let selection = self.selection.filter(|s| s.anchor == s.head)?;
            let id = self.units.get(selection.head)?.id;
            self.doc.blocks().iter().find(|(_, block)| block.id == id)?.1
        };
        let id = block.persisted_id.as_deref()?;
        aero_core::block_refs::resolve(&self.doc, id).ok()?;
        Some(id.into())
    }

    pub(super) fn render_backlink_count(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let view = self.backlinks.as_ref()?;
        Some(
            div()
                .id("backlink-count")
                .debug_selector(|| "backlink-count".into())
                .mb_3()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .cursor_pointer()
                .child(view.read(cx).count_label(cx))
                .on_click(cx.listener(|_, _, _, cx| cx.emit(EditorEvent::BacklinksRequested)))
                .into_any_element(),
        )
    }
}
