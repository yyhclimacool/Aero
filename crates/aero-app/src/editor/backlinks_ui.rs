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

    pub(crate) fn reference_snapshot(&self) -> (FileStamp, aero_core::Snapshot) {
        (self.stamp, self.doc.snapshot())
    }

    pub fn show_references(&mut self, cx: &mut Context<Self>) {
        self.reveal = None;
        if let Some(bounds) = self.references_bounds.get() {
            let offset = self.scroll.offset();
            let delta = bounds.top() - self.scroll.bounds().top() - px(16.);
            self.scroll.set_offset(point(offset.x, (offset.y - delta).min(px(0.))));
        }
        cx.notify();
    }

    pub(super) fn render_references(&self) -> Option<AnyElement> {
        let view = self.backlinks.as_ref()?;
        let bounds = self.references_bounds.clone();
        Some(
            div()
                .on_children_prepainted(move |children, _, _| bounds.set(children.first().copied()))
                .id("document-references")
                .debug_selector(|| "document-references".into())
                .w_full()
                .min_w_0()
                .pt_4()
                .pb_8()
                .child(view.clone())
                .into_any_element(),
        )
    }
}
