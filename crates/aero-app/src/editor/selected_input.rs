//! Route the first input in block selection to its actual text editor.

use super::*;

impl Editor {
    pub(super) fn selected_edit_unit(&self, cx: &App) -> Option<usize> {
        let editable = |unit: &units::Unit| {
            !matches!(unit.kind, BlockKind::ThematicBreak | BlockKind::MathBlock)
                && !self.keeps_rendered_image(unit.id, cx)
                && !self.unit_is_pdf_embed(unit.id)
                && !(self.hides_markers(cx)
                    && !self.block_shows_source(unit.id)
                    && preview::is_note_embed(self.doc.slice(unit.range.clone()).to_string().trim()))
        };
        if let Some(id) = self.selected_node {
            let range = self.doc.range_of(self.doc.blocks().find(id)?);
            let from = self.units.partition_point(|unit| unit.range.start < range.start);
            self.units[from..]
                .iter()
                .take_while(|unit| unit.range.start <= range.end)
                .position(|unit| {
                    (unit.id == id
                        || aero_core::containers::ancestors(&self.doc, unit.id).iter().any(|b| b.id == id))
                        && editable(unit)
                })
                .map(|index| from + index)
        } else {
            let index = self.selection?.head;
            self.units.get(index).filter(|unit| editable(unit)).map(|_| index)
        }
    }

    pub(super) fn selected_input(&self, cx: &Context<Self>) -> impl IntoElement {
        let editor = cx.entity();
        let focus = self.focus_handle.clone();
        canvas(
            |_, _, _| (),
            move |bounds, _, window, cx| {
                window.handle_input(&focus, ElementInputHandler::new(bounds, editor), cx);
            },
        )
        .absolute()
        .top_0()
        .left_0()
    }

    fn begin_selected_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<TextareaState>> {
        if self.io.suspended || self.block_menu.is_some() || self.color_menu.is_some() {
            return None;
        }
        if self.focused.is_none() {
            self.ensure_current_parse();
            let ix = self.selected_edit_unit(cx)?;
            self.focus_unit(ix, self.units[ix].range.len(), window, cx);
        }
        self.focused.as_ref().map(|f| f.textarea.clone())
    }
}

impl EntityInputHandler for Editor {
    fn accepts_text_input(&self, _: &mut Window, cx: &mut Context<Self>) -> bool {
        !self.io.suspended
            && self.block_menu.is_none()
            && self.color_menu.is_none()
            && (self.focused.is_some() || self.selected_edit_unit(cx).is_some())
    }

    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let input = self.focused.as_ref()?.textarea.clone();
        input.update(cx, |ta, cx| ta.text_for_range(range, adjusted, window, cx))
    }

    fn selected_text_range(
        &mut self,
        ignore_disabled_input: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        if let Some(input) = self.focused.as_ref().map(|f| f.textarea.clone()) {
            return input.update(cx, |ta, cx| ta.selected_text_range(ignore_disabled_input, window, cx));
        }
        self.selected_edit_unit(cx).map(|_| UTF16Selection { range: 0..0, reversed: false })
    }

    fn marked_text_range(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<Range<usize>> {
        let input = self.focused.as_ref()?.textarea.clone();
        input.update(cx, |ta, cx| ta.marked_text_range(window, cx))
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = self.focused.as_ref().map(|f| f.textarea.clone()) {
            input.update(cx, |ta, cx| ta.unmark_text(window, cx));
        }
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.focused.is_some().then_some(range).flatten();
        if (!text.is_empty() || self.focused.is_some())
            && let Some(input) = self.begin_selected_input(window, cx)
        {
            input.update(cx, |ta, cx| ta.replace_text_in_range(range, text, window, cx));
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selection: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.focused.is_some().then_some(range).flatten();
        if (!text.is_empty() || self.focused.is_some())
            && let Some(input) = self.begin_selected_input(window, cx)
        {
            input.update(cx, |ta, cx| ta.replace_and_mark_text_in_range(range, text, selection, window, cx));
        }
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        if let Some(input) = self.focused.as_ref().map(|f| f.textarea.clone()) {
            let bounds = input.read(cx).input_bounds();
            return input.update(cx, |ta, cx| ta.bounds_for_range(range, bounds, window, cx));
        }
        let id = self.units[self.selected_edit_unit(cx)?].id;
        Some(self.unit_bounds.iter().find(|(block, _)| *block == id).map_or(bounds, |(_, bounds)| *bounds))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let input = self.focused.as_ref()?.textarea.clone();
        input.update(cx, |ta, cx| ta.character_index_for_point(point, window, cx))
    }
}
