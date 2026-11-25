//! Session-only styles for blank paragraphs, until Markdown has a body to own them.

use super::*;

pub(super) struct EmptySlot {
    pub range: Range<usize>,
    pub attrs: aero_core::Attrs,
}

/// Style each text block created by the first commit. Existing pasted block
/// properties take precedence; identities are never inherited from the source.
pub(super) fn styled_input(
    text: &str,
    cursor: usize,
    inherited: &aero_core::Attrs,
) -> Result<(String, usize), String> {
    let mut doc = Document::from_text(text);
    let mut edits = Vec::new();
    fn collect(
        blocks: &[Block],
        doc: &Document,
        inherited: &aero_core::Attrs,
        edits: &mut Vec<Edit>,
    ) -> Result<(), String> {
        for block in blocks {
            if matches!(block.kind, BlockKind::Paragraph | BlockKind::Heading { .. }) {
                let mut attrs = inherited.clone();
                attrs.id = None;
                aero_core::properties::set(&mut attrs, "aero-aliases", None);
                aero_core::properties::set(&mut attrs, "aero-owner", None);
                // An explicit color on pasted text overrides the inherited channel.
                use aero_core::colors::Slot;
                for slot in [Slot::Text, Slot::Background, Slot::Border] {
                    if slot.color_of(&block.attrs).is_some() {
                        slot.write(&mut attrs, None);
                    }
                }
                for (key, _) in &block.attrs.pairs {
                    aero_core::properties::set(&mut attrs, key, None);
                }
                attrs.merge(block.attrs.clone());
                let mut seen = HashSet::new();
                attrs.classes.retain(|class| seen.insert(class.clone()));
                if let Some(edit) =
                    aero_core::properties::edit(doc, block.id, &attrs, block.persisted_id.as_deref())?
                {
                    edits.push(edit);
                }
            } else {
                collect(&block.children, doc, inherited, edits)?;
            }
        }
        Ok(())
    }
    collect(&doc.blocks().roots, &doc, inherited, &mut edits)?;
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
    let mut cursor = cursor;
    for edit in &edits {
        // Keep a caret at the end of body text before the new hidden suffix.
        if edit.range.start < cursor {
            cursor = if cursor <= edit.range.end {
                edit.range.start
            } else {
                (cursor as isize + edit.delta()) as usize
            };
        }
    }
    doc.edit_group(edits).map_err(|err| err.to_string())?;
    Ok((doc.to_string(), cursor))
}

pub(super) fn in_container_gap(doc: &Document, container: &Block, offset: usize) -> bool {
    if !aero_core::containers::is_editable(&container.kind)
        || !container.children.iter().any(|block| block.kind != BlockKind::Marker)
    {
        return false;
    }
    let range = doc.range_of(container);
    let at_quote_end = offset == range.end && matches!(container.kind, BlockKind::BlockQuote);
    range.start < offset
        && (offset < range.end || at_quote_end)
        && container.children.iter().filter(|block| block.kind != BlockKind::Marker).all(|child| {
            let range = doc.range_of(child);
            offset < range.start
                || offset >= range.end
                    && !(offset == range.end && matches!(child.kind, BlockKind::BlockQuote))
        })
}

impl Editor {
    pub(super) fn available_empty_slot(&self, slot: &EmptySlot) -> bool {
        if !slot.range.is_empty() || unit_at(&self.units, slot.range.start).is_some() {
            return false;
        }
        let at = slot.range.start;
        if self.doc.blocks().iter().any(|(_, block)| in_container_gap(&self.doc, block, at)) {
            return true;
        }
        // Undoing creation can collapse an annotation onto the previous
        // paragraph. Only an actual reserved blank line gets a viewport row.
        let before = self.doc.slice(0..at);
        let mut chars = before.chars_at(before.len()).reversed();
        let blank_before = at == 0 || (chars.next() == Some('\n') && chars.next() == Some('\n'));
        blank_before && self.doc.slice(at..self.doc.len()).chars().next().is_none_or(|c| c == '\n')
    }

    pub(super) fn shift_empty_slots(&mut self, edits: &[aero_core::text::EditSummary]) {
        for edit in edits {
            for slot in &mut self.empty_slots {
                edit.shift_range(&mut slot.range);
            }
        }
        // Keep the expanded span after first input as well: inverse edits then
        // collapse it back to the same blank slot with its original style.
        // In particular, a paste's last paragraph may have explicit overrides.
        self.empty_slots.retain(|slot| slot.range.end <= self.doc.len());
    }

    pub(super) fn render_empty_slot(&self, offset: usize, cx: &Context<Self>) -> AnyElement {
        if self.focused.as_ref().is_some_and(|f| f.id.is_none() && f.range.start == offset) {
            return self.render_textarea(None, cx);
        }
        let attrs = self
            .empty_slots
            .iter()
            .find(|slot| slot.range.is_empty() && slot.range.start == offset)
            .map(|slot| &slot.attrs);
        let mut el = div()
            .id(("empty-paragraph", offset))
            .debug_selector(|| format!("empty-paragraph-{offset}"))
            .w_full()
            .h(px(BODY_FONT_SIZE * BODY_LINE_HEIGHT))
            .cursor_text()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    this.focus_at(offset, None, window, cx);
                    cx.stop_propagation();
                }),
            );
        if let Some(attrs) = attrs {
            el = Self::painted(el, crate::colors::of(attrs, cx));
        }
        el.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::styled_input;

    #[test]
    fn multiline_commit_styles_each_paragraph_and_keeps_pasted_overrides() {
        let text = "第一行\n第二行\n\n末段 {.blue align=right}";
        let attrs = aero_core::Attrs::parse_inner(".red align=center indent=2 custom=value").unwrap();
        let (styled, cursor) = styled_input(text, text.len(), &attrs).unwrap();
        assert_eq!(
            styled,
            "第一行\n第二行 {.red align=center indent=2 custom=value}\n\n末段 {.blue indent=2 custom=value align=right}"
        );
        assert_eq!(&styled[..cursor], "第一行\n第二行 {.red align=center indent=2 custom=value}\n\n末段");
    }
}
