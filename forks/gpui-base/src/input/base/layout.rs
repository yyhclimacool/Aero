use std::{ops::Range, rc::Rc};

use gpui::{Bounds, Half, Pixels, ShapedLine, TextAlign, point, px};

use super::{WrappingIndent, display_map::LineLayout};

#[derive(Clone, Default)]
pub(crate) struct WhitespaceIndicators {
    pub(crate) space: ShapedLine,
    pub(crate) tab: ShapedLine,
}

#[derive(Clone)]
pub(super) struct LastLayout {
    pub(super) visible_range: Range<usize>,
    pub(super) visible_buffer_lines: Vec<usize>,
    pub(super) visible_line_byte_offsets: Vec<usize>,
    pub(super) visible_top: Pixels,
    pub(super) visible_range_offset: Range<usize>,
    pub(super) lines: Rc<Vec<LineLayout>>,
    pub(super) line_height: Pixels,
    pub(super) wrap_width: Option<Pixels>,
    pub(super) wrapping_indent: WrappingIndent,
    pub(super) line_number_width: Pixels,
    /// Width of one space in the editor font.
    ///
    /// Past the end of a line there are no glyphs to hit-test against, so this is the
    /// step used to measure how far past the end a pointer sits.
    pub(super) space_width: Pixels,
    pub(super) cursor_bounds: Option<Bounds<Pixels>>,
    pub(super) text_align: TextAlign,
    pub(super) content_width: Pixels,
}

impl LastLayout {
    pub(crate) fn line(&self, row: usize) -> Option<&LineLayout> {
        let pos = self.visible_buffer_lines.binary_search(&row).ok()?;
        self.lines.get(pos)
    }

    pub(crate) fn range_bounds(&self, range: &Range<usize>) -> Vec<Bounds<Pixels>> {
        self.range_bounds_with_affinity(range, false)
    }

    pub(crate) fn range_bounds_with_affinity(&self, range: &Range<usize>, affinity: bool) -> Vec<Bounds<Pixels>> {
        if range.start < self.visible_range_offset.start || range.end > self.visible_range_offset.end {
            return Vec::new();
        }
        let mut y = self.visible_top;
        let mut bounds = Vec::new();
        for (&offset, line) in self.visible_line_byte_offsets.iter().zip(self.lines.iter()) {
            if range.start <= offset + line.len()
                && (range.end > offset || (range.is_empty() && range.end == offset))
            {
                let local = range.start.saturating_sub(offset)..range.end.saturating_sub(offset);
                let fragments = if affinity { line.range_bounds_with_affinity(local, self, true) }
                    else { line.range_bounds(local, self) };
                bounds.extend(fragments.into_iter().map(|mut b| {
                    b.origin += point(self.line_number_width, y);
                    b
                }));
            }
            y += line.size(self.line_height).height;
        }
        bounds
    }

    pub(crate) fn alignment_offset(align: TextAlign, width: Pixels, line_width: Pixels) -> Pixels {
        match align {
            TextAlign::Left => px(0.),
            TextAlign::Center => (width - line_width).half().max(px(0.)),
            TextAlign::Right => (width - line_width).max(px(0.)),
        }
    }
}
