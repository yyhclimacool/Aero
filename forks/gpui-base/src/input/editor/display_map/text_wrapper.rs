use gpui::Half;
use std::borrow::Cow;
use std::ops::Range;

use gpui::{
    App, Boundary, Bounds, Font, FontStyle, FontWeight, Pixels, Point, ShapedLine, SharedString, Size,
    TextAlign, TextRun, Window, WindowTextSystem, point, px, size,
};
use ropey::Rope;
use smallvec::SmallVec;
use sum_tree::{Bias, Dimensions, SumTree};
use unicode_segmentation::UnicodeSegmentation;

use crate::input::{
    Point as TreeSitterPoint, RopeExt,
    layout::{LastLayout, WhitespaceIndicators},
};

/// Controls how soft-wrapped continuation lines are indented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WrappingIndent {
    /// Continuation lines start flush-left at the full editor width.
    None,
    /// Continuation lines keep the same indentation as the first line.
    #[default]
    Same,
}

/// A line with soft wrapped lines info.
#[derive(Debug, Clone)]
pub(crate) struct LineItem {
    /// The byte length of the line, without the end `\n`.
    len: usize,
    /// Number of leading characters of the line reserved as indentation for continuation wrapped
    /// lines, when [`WrappingIndent::Same`] is used.
    ///
    /// Zero when [`WrappingIndent::None`] is used or the line is not wrapped.
    pub(crate) indent: u32,
    /// When hard breaks split a buffer line, each segment starts a fresh indent group:
    /// (first visual row, leading characters reserved by its soft continuations).
    /// Empty preserves the ordinary single-indent layout.
    pub(crate) indent_segments: SmallVec<[(usize, u32); 1]>,
    /// The soft wrapped lines relative byte range (0..len) of this line (Include first line).
    ///
    /// Not contains the line end `\n`.
    pub(crate) wrapped_lines: SmallVec<[Range<usize>; 1]>,
}

impl LineItem {
    /// Get the bytes length of this line.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Get number of soft wrapped lines of this line (include the first line).
    #[inline]
    pub(crate) fn lines_len(&self) -> usize {
        self.wrapped_lines.len()
    }
}

/// Summary of a subtree of [`LineItem`]s, maintained incrementally by the [`SumTree`].
#[derive(Debug, Clone)]
pub(crate) struct LineSummary {
    /// Number of buffer lines.
    buffer_rows: usize,
    /// Number of wrap rows (sum of each line's `lines_len()`).
    wrap_rows: usize,
    /// Sum of byte lengths of the buffer lines (without the trailing `\n`).
    bytes: usize,
    /// Byte length of the longest line in this subtree.
    max_line_len: usize,
    /// Buffer row (relative to this subtree) of the first line achieving `max_line_len`.
    longest_row: usize,
}

impl sum_tree::Summary for LineSummary {
    type Context<'a> = &'a ();

    fn zero(_: &()) -> Self {
        LineSummary { buffer_rows: 0, wrap_rows: 0, bytes: 0, max_line_len: 0, longest_row: 0 }
    }

    fn add_summary(&mut self, other: &Self, _: &()) {
        // Keep the leftmost row that achieves a strictly greater length
        if other.max_line_len > self.max_line_len {
            self.longest_row = self.buffer_rows + other.longest_row;
            self.max_line_len = other.max_line_len;
        }
        self.buffer_rows += other.buffer_rows;
        self.wrap_rows += other.wrap_rows;
        self.bytes += other.bytes;
    }
}

impl sum_tree::Item for LineItem {
    type Summary = LineSummary;

    fn summary(&self, _: &()) -> LineSummary {
        LineSummary {
            buffer_rows: 1,
            wrap_rows: self.lines_len(),
            bytes: self.len(),
            max_line_len: self.len(),
            longest_row: 0,
        }
    }
}

/// Cursor dimension counting buffer rows.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct BufferRows(pub usize);

impl<'a> sum_tree::Dimension<'a, LineSummary> for BufferRows {
    fn zero(_: &()) -> Self {
        BufferRows(0)
    }

    fn add_summary(&mut self, summary: &'a LineSummary, _: &()) {
        self.0 += summary.buffer_rows;
    }
}

/// Cursor dimension counting wrap rows.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct WrapRows(pub usize);

impl<'a> sum_tree::Dimension<'a, LineSummary> for WrapRows {
    fn zero(_: &()) -> Self {
        WrapRows(0)
    }

    fn add_summary(&mut self, summary: &'a LineSummary, _: &()) {
        self.0 += summary.wrap_rows;
    }
}

/// How a styled range differs from the wrapper's base font.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct FontOverride {
    pub(crate) weight: Option<FontWeight>,
    pub(crate) style: Option<FontStyle>,
    pub(crate) family: Option<SharedString>,
    pub(crate) object: Option<Size<Pixels>>,
}

/// Shapes whole display lines. Characters are not measured one at a time: an isolated `，`
/// falls back to a different font than inside a CJK run and comes out half as wide.
struct Measure {
    text: WindowTextSystem,
    base: Font,
    font_size: Pixels,
}

impl Measure {
    /// The base font with `overrides` applied in order, later ones winning.
    fn font<'a>(&self, overrides: impl Iterator<Item = &'a FontOverride>) -> Font {
        let mut font = self.base.clone();
        for o in overrides {
            if let Some(weight) = o.weight {
                font.weight = weight;
            }
            if let Some(style) = o.style {
                font.style = style;
            }
            if let Some(family) = &o.family {
                font.family = family.clone();
            }
        }
        font
    }

    fn space(&self) -> Pixels {
        let font_id = self.text.resolve_font(&self.base);
        self.text.layout_width(font_id, self.font_size, ' ')
    }
}

/// Batch GPUI's `LineLayout::x_for_index`: the first glyph in run/glyph order with
/// `glyph.index >= index`, or the line width if none exists. Glyph indexes need not be
/// sorted (RTL and font runs can overlap). A glyph at or below an earlier index can
/// never be the first match, so retain only strictly increasing prefix maxima.
/// Binary searching those records preserves first-match semantics for arbitrary query
/// order in O(glyphs + queries * log(glyphs)) time, without sorting the glyphs.
fn batch_x_for_indices<'a>(
    glyphs: impl Iterator<Item = &'a gpui::ShapedGlyph>,
    width: Pixels,
    indices: impl IntoIterator<Item = usize>,
) -> Vec<Pixels> {
    let mut first_matches: Vec<(usize, Pixels)> = Vec::new();
    for glyph in glyphs {
        if first_matches.last().is_none_or(|&(index, _)| glyph.index > index) {
            first_matches.push((glyph.index, glyph.position.x));
        }
    }
    indices
        .into_iter()
        .map(|index| {
            let at = first_matches.partition_point(|&(glyph_index, _)| glyph_index < index);
            first_matches.get(at).map_or(width, |&(_, x)| x)
        })
        .collect()
}

/// Wrap measured graphemes at word/CJK opportunities, allowing unbreakable runs to overflow.
/// `chars` holds each grapheme's source byte index, first character, and measured width.
fn wrap_chars(chars: &[(usize, char, Pixels)], space: Pixels, wrap_width: Pixels) -> Vec<Boundary> {
    const MAX_INDENT: u32 = 256;
    let mut out = Vec::new();
    let mut width = px(0.);
    let mut first_non_whitespace = None;
    let mut indent = None;
    let mut last_candidate = 0;
    let mut last_candidate_width = px(0.);
    let mut last_wrap = 0;
    let mut prev_c = '\0';
    for (k, &(_, c, item_width)) in chars.iter().enumerate() {
        if c == '\n' {
            continue;
        }
        if legal_break(prev_c, c) && first_non_whitespace.is_some() {
            last_candidate = k;
            last_candidate_width = width;
        }
        if c != ' ' && first_non_whitespace.is_none() {
            first_non_whitespace = Some(k);
        }
        width += item_width;
        if width > wrap_width && last_candidate > last_wrap {
            if let (None, Some(first)) = (indent, first_non_whitespace) {
                indent = Some(MAX_INDENT.min((first - last_wrap) as u32));
            }
            last_wrap = last_candidate;
            width -= last_candidate_width;
            last_candidate = 0;
            if let Some(indent) = indent {
                width += space * indent as f32;
            }
            out.push(Boundary { ix: chars[last_wrap].0, next_indent: indent.unwrap_or(0) });
        }
        prev_c = c;
    }
    out
}

fn legal_break(left: char, right: char) -> bool {
    !right.is_whitespace()
        && !matches!(left, '\u{00a0}' | '\u{202f}' | '\u{2011}')
        && !no_break_after(left)
        && !no_break_before(right)
        && (left.is_whitespace() || !is_word_char(right) || !is_word_char(left))
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x9fff | 0xf900..=0xfaff | 0x20000..=0x323af
        | 0x3040..=0x30ff | 0xac00..=0xd7af)
}

/// Characters GPUI keeps attached to the previous word.
fn is_word_char(c: char) -> bool {
    (c.is_alphanumeric() && !is_cjk(c))
        || matches!(c, '\u{00C0}'..='\u{024F}' | '\u{0400}'..='\u{04FF}' | '\u{1E00}'..='\u{1EFF}')
        || matches!(c, '\u{0300}'..='\u{036F}' | '\u{0980}'..='\u{09FF}')
        || matches!(
            c,
            '-' | '_' | '.' | '\'' | '’' | '‘' | '$' | '%' | '@' | '#' | '^' | '~' | ',' | '=' | ':' | ';'
        )
        || matches!(c, '!' | ')' | ']' | '}' | '"' | '”' | '»' | '…' | '⋯')
        || matches!(c, '\u{202F}' | '\u{00A0}' | '\u{2011}')
}

/// Closing punctuation and small kana that may not start a line.
fn no_break_before(c: char) -> bool {
    matches!(
        c,
        '。' | '，'
            | '、'
            | '．'
            | '：'
            | '；'
            | '？'
            | '！'
            | '）'
            | '」'
            | '』'
            | '】'
            | '〕'
            | '〉'
            | '》'
            | '］'
            | '｝'
            | '”'
            | '’'
            | '…'
            | '‥'
            | '・'
            | '〜'
            | 'ー'
            | '々'
            | 'ぁ'
            | 'ぃ'
            | 'ぅ'
            | 'ぇ'
            | 'ぉ'
            | 'っ'
            | 'ゃ'
            | 'ゅ'
            | 'ょ'
            | 'ァ'
            | 'ィ'
            | 'ゥ'
            | 'ェ'
            | 'ォ'
            | 'ッ'
            | 'ャ'
            | 'ュ'
            | 'ョ'
            | '%'
            | ','
            | '.'
            | ':'
            | ';'
            | '?'
            | '!'
            | ')'
            | ']'
            | '}'
    )
}

/// Opening punctuation that may not end a line.
fn no_break_after(c: char) -> bool {
    matches!(c, '（' | '「' | '『' | '【' | '〔' | '〈' | '《' | '［' | '｛' | '“' | '‘' | '(' | '[' | '{')
}

/// Used to prepare the text with soft wrap to be get lines to displayed in the Editor.
///
/// After use lines to calculate the scroll size of the Editor.
pub(crate) struct TextWrapper {
    text: Rope,
    font: Font,
    font_size: Pixels,
    /// If is none, it means the text is not wrapped
    wrap_width: Option<Pixels>,
    /// Document ranges that are not drawn. They must not consume wrap width, or the
    /// visible line breaks diverge from a preview that never includes those bytes.
    conceal: Vec<Range<usize>>,
    /// Concealed source ranges that end a visual line without changing the rope.
    hard_breaks: Vec<Range<usize>>,
    /// Ranges drawn in another weight, slant or family than `font`.
    fonts: Vec<(Range<usize>, FontOverride)>,
    wrapping_indent: WrappingIndent,
    /// The lines by split \n
    pub(crate) lines: SumTree<LineItem>,

    _initialized: bool,
}

#[allow(unused)]
impl TextWrapper {
    pub(crate) fn new(font: Font, font_size: Pixels, wrap_width: Option<Pixels>) -> Self {
        Self {
            text: Rope::new(),
            font,
            font_size,
            wrap_width,
            conceal: Vec::new(),
            hard_breaks: Vec::new(),
            fonts: Vec::new(),
            wrapping_indent: WrappingIndent::default(),
            lines: SumTree::new(&()),
            _initialized: false,
        }
    }

    #[inline]
    pub(crate) fn set_default_text(&mut self, text: &Rope) {
        self.text = text.clone();
    }

    /// Get reference to the rope text.
    #[inline]
    pub(crate) fn text(&self) -> &Rope {
        &self.text
    }

    /// Get the total number of lines including wrapped lines.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.lines.summary().wrap_rows
    }

    /// Get the total number of buffer lines.
    #[inline]
    pub(crate) fn lines_count(&self) -> usize {
        self.lines.summary().buffer_rows
    }

    /// Get the 0-based row index of the longest line (by byte length).
    #[inline]
    pub(crate) fn longest_row(&self) -> usize {
        self.lines.summary().longest_row
    }

    /// Get the line item by buffer row index.
    #[inline]
    pub(crate) fn line(&self, row: usize) -> Option<&LineItem> {
        let mut cursor = self.lines.cursor::<BufferRows>(&());
        cursor.seek(&BufferRows(row), Bias::Right);
        cursor.item()
    }

    /// Iterate buffer lines in order.
    #[inline]
    pub(crate) fn iter_lines(&self) -> impl Iterator<Item = &LineItem> {
        self.lines.iter()
    }

    /// First wrap row of buffer line `row`. Returns the total wrap row count if `row` is
    /// out of range.
    pub(crate) fn buffer_line_to_first_wrap_row(&self, row: usize) -> usize {
        let mut cursor = self.lines.cursor::<Dimensions<BufferRows, WrapRows>>(&());
        cursor.seek(&BufferRows(row), Bias::Right);
        cursor.start().1.0
    }

    /// Wrap row range of buffer line `row`.
    pub(crate) fn buffer_line_to_wrap_row_range(&self, row: usize) -> Range<usize> {
        let mut cursor = self.lines.cursor::<Dimensions<BufferRows, WrapRows>>(&());
        cursor.seek(&BufferRows(row), Bias::Right);
        let start = cursor.start().1.0;
        let len = cursor.item().map(|l| l.lines_len()).unwrap_or(0);
        start..start + len
    }

    /// Buffer line containing wrap row `wrap_row`, clamped to the last line.
    pub(crate) fn wrap_row_to_buffer_line(&self, wrap_row: usize) -> usize {
        let mut cursor = self.lines.cursor::<Dimensions<WrapRows, BufferRows>>(&());
        cursor.seek(&WrapRows(wrap_row), Bias::Right);
        match cursor.item() {
            Some(_) => cursor.start().1.0,
            None => self.lines_count().saturating_sub(1),
        }
    }

    pub(crate) fn set_wrap_width(&mut self, wrap_width: Option<Pixels>, cx: &mut App) {
        if wrap_width == self.wrap_width {
            return;
        }

        self.wrap_width = wrap_width;
        self.update_all(&self.text.clone(), cx);
    }

    /// Hidden ranges or styled ranges changed. Rewrap so line breaks follow the visible,
    /// styled text.
    pub(crate) fn set_conceal(
        &mut self,
        conceal: Vec<Range<usize>>,
        mut hard_breaks: Vec<Range<usize>>,
        fonts: Vec<(Range<usize>, FontOverride)>,
        cx: &mut App,
    ) {
        hard_breaks.retain(|range| !range.is_empty());
        hard_breaks.sort_unstable_by_key(|range| (range.start, range.end));
        hard_breaks.dedup();
        if conceal == self.conceal && hard_breaks == self.hard_breaks && fonts == self.fonts {
            return;
        }
        self.conceal = conceal;
        self.hard_breaks = hard_breaks;
        self.fonts = fonts;
        self.update_all(&self.text.clone(), cx);
    }

    pub(crate) fn set_wrapping_indent(&mut self, wrapping_indent: WrappingIndent, cx: &mut App) {
        if wrapping_indent == self.wrapping_indent {
            return;
        }

        self.wrapping_indent = wrapping_indent;
        self.update_all(&self.text.clone(), cx);
    }

    pub(crate) fn set_font(&mut self, font: Font, font_size: Pixels, cx: &mut App) {
        if self.font.eq(&font) && self.font_size == font_size {
            return;
        }

        self.font = font;
        self.font_size = font_size;
        self.update_all(&self.text.clone(), cx);
    }

    pub(crate) fn prepare_if_need(&mut self, text: &Rope, cx: &mut App) -> bool {
        if self._initialized {
            return false;
        }
        self._initialized = true;
        self.update_all(text, cx);
        true
    }

    /// Soft-wrap `line` as it is drawn: bytes hidden by `conceal` take no room and styled
    /// ranges are measured in their own font. Boundary indexes stay in `line`'s own
    /// coordinates so the rest of the display map can keep using buffer offsets.
    fn wrap_presented_line(
        measure: &Measure,
        line: &str,
        line_start: usize,
        conceal: &[Range<usize>],
        fonts: &[(Range<usize>, FontOverride)],
        wrap_width: Pixels,
    ) -> Vec<Boundary> {
        let line_end = line_start + line.len();
        let overlaps = |range: &Range<usize>| range.start < line_end && range.end > line_start;
        let conceal: SmallVec<[&Range<usize>; 4]> = conceal.iter().filter(|range| overlaps(range)).collect();
        let fonts: SmallVec<[&(Range<usize>, FontOverride); 4]> =
            fonts.iter().filter(|(range, _)| overlaps(range)).collect();

        let objects: Vec<_> = fonts
            .iter()
            .filter_map(|(range, font)| font.object.map(|size| (range.clone(), size, 0)))
            .collect();
        let conceal: Vec<_> = conceal.into_iter().cloned().collect();
        let (display, map) = ConcealMap::present(line, line_start, &conceal, &objects);
        let mut runs: Vec<TextRun> = Vec::new();
        for (at, ch) in display.char_indices() {
            let abs = line_start + map.to_buffer(at);
            let font =
                measure.font(fonts.iter().filter(|(range, _)| range.contains(&abs)).map(|(_, font)| font));
            match runs.last_mut() {
                Some(run) if run.font == font => run.len += ch.len_utf8(),
                _ => runs.push(TextRun {
                    len: ch.len_utf8(),
                    font,
                    color: gpui::black(),
                    ..Default::default()
                }),
            }
        }
        if display.is_empty() {
            return Vec::new();
        }
        let drawn: Vec<_> = display
            .grapheme_indices(true)
            .map(|(at, cluster)| (map.to_buffer(at), cluster.chars().next().unwrap(), at))
            .collect();
        let layout = measure.text.layout_line(&display, measure.font_size, &runs, None);
        let positions = batch_x_for_indices(
            layout.runs.iter().flat_map(|run| run.glyphs.iter()),
            layout.width,
            drawn.iter().map(|&(_, _, at)| at).chain(std::iter::once(display.len())),
        );
        let chars: Vec<(usize, char, Pixels)> = drawn
            .iter()
            .zip(positions.windows(2))
            .map(|(&(i, ch, at), xs)| {
                let width = map
                    .objects
                    .iter()
                    .find(|(r, _, _)| r.start == at)
                    .map_or(xs[1] - xs[0], |(_, size, _)| size.width);
                (i, ch, width)
            })
            .collect();
        wrap_chars(&chars, measure.space(), wrap_width)
    }

    /// Update the text wrapper and recalculate the wrapped lines.
    ///
    /// If the `text` is the same as the current text, do nothing.
    ///
    /// - `changed_text`: The text [`Rope`] that has changed.
    /// - `range`: The `selected_range` before change.
    /// - `new_text`: The inserted text.
    /// - `force`: Whether to force the update, if false, the update will be skipped if the text is the same.
    /// - `cx`: The application context.
    pub(crate) fn update(
        &mut self,
        changed_text: &Rope,
        range: &Range<usize>,
        new_text: &Rope,
        cx: &mut App,
    ) {
        let conceal = std::mem::take(&mut self.conceal);
        let fonts = std::mem::take(&mut self.fonts);
        let measure = Measure {
            text: WindowTextSystem::new(cx.text_system().clone()),
            base: self.font.clone(),
            font_size: self.font_size,
        };
        self._update(changed_text, range, new_text, &mut |line_str, line_start, wrap_width| {
            Self::wrap_presented_line(&measure, line_str, line_start, &conceal, &fonts, wrap_width)
        });
        self.conceal = conceal;
        self.fonts = fonts;
    }

    fn _update<F>(&mut self, changed_text: &Rope, range: &Range<usize>, new_text: &Rope, wrap_line: &mut F)
    where
        F: FnMut(&str, usize, Pixels) -> Vec<gpui::Boundary>,
    {
        // Remove the old changed lines.
        let buffer_line_count = self.lines_count();
        let start_row = self.text.offset_to_point(range.start).row;
        let start_row = start_row.min(buffer_line_count.saturating_sub(1));
        let end_row = self.text.offset_to_point(range.end).row;
        let end_row = end_row.min(buffer_line_count.saturating_sub(1));

        // To add the new lines.
        let new_start_row = changed_text.offset_to_point(range.start).row;
        let new_end_row = changed_text.offset_to_point(range.start + new_text.len()).row;

        let mut new_lines = Vec::with_capacity(new_end_row.saturating_sub(new_start_row) + 1);
        let wrap_width = self.wrap_width;

        // line not contains `\n`.
        for row in new_start_row..=new_end_row {
            let line = changed_text.slice_line(row);
            let line_start = changed_text.line_start_offset(row);
            let mut wrapped_lines = SmallVec::<[Range<usize>; 1]>::new();
            let mut indent_chars = 0;
            // Keep each hard-break token on the preceding row in source coordinates.
            // A trailing token also opens an empty final visual row. These boundaries are
            // independent of soft wrapping, which may be disabled altogether.
            let line_str: Cow<str> = line.into();
            let mut hard_ends = Vec::new();
            let mut previous_end = line_start;
            for range in &self.hard_breaks {
                if range.start >= previous_end
                    && range.start < range.end
                    && range.end <= line_start + line_str.len()
                    && line_str.is_char_boundary(range.start - line_start)
                    && line_str.is_char_boundary(range.end - line_start)
                {
                    hard_ends.push(range.end - line_start);
                    previous_end = range.end;
                }
            }
            let has_hard_breaks = !hard_ends.is_empty();
            let mut indent_segments = SmallVec::new();
            let mut segment_start = 0;
            for segment_end in hard_ends.into_iter().chain(std::iter::once(line.len())) {
                let first_visual_row = wrapped_lines.len();
                indent_chars = 0;
                let segment = &line_str[segment_start..segment_end];
                let mut prev_boundary_ix = segment_start;
                if let Some(wrap_width) = wrap_width {
                    let boundaries = wrap_line(segment, line_start + segment_start, wrap_width);
                    match self.wrapping_indent {
                        WrappingIndent::Same => {
                            for boundary in boundaries {
                                let ix = segment_start + boundary.ix;
                                if ix > prev_boundary_ix && ix < segment_end {
                                    wrapped_lines.push(prev_boundary_ix..ix);
                                    prev_boundary_ix = ix;
                                    indent_chars = boundary.next_indent;
                                }
                            }
                        }
                        WrappingIndent::None => {
                            // Rewrap the continuation without reserving the first row's indent.
                            if let Some(first_ix) =
                                boundaries.iter().map(|b| b.ix).find(|&ix| ix > 0 && ix < segment.len())
                            {
                                let first = segment_start + first_ix;
                                wrapped_lines.push(prev_boundary_ix..first);
                                prev_boundary_ix = first;
                                for boundary in
                                    wrap_line(&segment[first_ix..], line_start + first, wrap_width)
                                {
                                    let ix = first + boundary.ix;
                                    if ix > prev_boundary_ix && ix < segment_end {
                                        wrapped_lines.push(prev_boundary_ix..ix);
                                        prev_boundary_ix = ix;
                                    }
                                }
                            }
                        }
                    }
                }
                wrapped_lines.push(prev_boundary_ix..segment_end);
                if has_hard_breaks {
                    indent_segments.push((first_visual_row, indent_chars));
                }
                segment_start = segment_end;
            }

            new_lines.push(LineItem {
                len: line.len(),
                indent: indent_chars,
                indent_segments,
                wrapped_lines,
            });
        }

        if self.lines.is_empty() {
            self.lines = SumTree::from_iter(new_lines, &());
        } else {
            let mut cursor = self.lines.cursor::<BufferRows>(&());
            let mut new_tree = cursor.slice(&BufferRows(start_row), Bias::Right);
            // Skip the replaced rows
            cursor.seek_forward(&BufferRows(end_row + 1), Bias::Right);
            new_tree.extend(new_lines, &());
            // Untouched rows after the edit
            new_tree.append(cursor.suffix(), &());
            drop(cursor);
            self.lines = new_tree;
        }

        self.text = changed_text.clone();
    }

    /// Update the text wrapper and recalculate the wrapped lines.
    ///
    /// If the `text` is the same as the current text, do nothing.
    fn update_all(&mut self, text: &Rope, cx: &mut App) {
        self.update(text, &(0..text.len()), &text, cx);
    }

    /// Return display point (with soft wrap) from the given byte offset in the text.
    ///
    /// Panics if the `offset` is out of bounds.
    pub(crate) fn offset_to_display_point(&self, offset: usize) -> WrapDisplayPoint {
        self.offset_to_display_point_with_affinity(offset, false)
    }

    /// Like [`Self::offset_to_display_point`], but honours the caret's line-end affinity.
    ///
    /// A soft wrap boundary is one offset shared by two visual rows. Without affinity it always
    /// resolves to the start of the second row, which is wrong for a caret that is being drawn at
    /// the end of the first one -- vertical movement would then step from the row below the one
    /// the user can see.
    pub(crate) fn offset_to_display_point_with_affinity(
        &self,
        offset: usize,
        line_end_affinity: bool,
    ) -> WrapDisplayPoint {
        let row = self.text.offset_to_point(offset).row;
        let start = self.text.line_start_offset(row);

        // Seek to buffer row
        let mut cursor = self.lines.cursor::<Dimensions<BufferRows, WrapRows>>(&());
        cursor.seek(&BufferRows(row), Bias::Right);
        let wrapped_row = cursor.start().1.0;
        let Some(line) = cursor.item() else {
            return WrapDisplayPoint::new(wrapped_row, 0, 0);
        };

        let local_offset = offset.saturating_sub(start);
        // Unlike a soft wrap, a hard break has distinct before/after source positions.
        // Its end always belongs to the next row, even if the previous caret had affinity.
        let line_end_affinity =
            line_end_affinity && !self.hard_breaks.iter().any(|range| range.end == offset);
        for (ix, range) in line.wrapped_lines.iter().enumerate() {
            let matches = range.contains(&local_offset) || (line_end_affinity && local_offset == range.end);
            if matches {
                return WrapDisplayPoint::new(wrapped_row + ix, ix, local_offset.saturating_sub(range.start));
            }
        }

        // Otherwise return the eof of the line.
        let last_range = line.wrapped_lines.last().unwrap_or(&(0..0));
        let ix = line.lines_len().saturating_sub(1);
        return WrapDisplayPoint::new(wrapped_row + ix, ix, last_range.len());
    }

    /// Return byte offset in the text from the given display point (with soft wrap).
    ///
    /// Panics if the `point.row` is out of bounds.
    pub(crate) fn display_point_to_offset(&self, point: WrapDisplayPoint) -> usize {
        // Seek to wrap row `point.row`
        let mut cursor = self.lines.cursor::<Dimensions<WrapRows, BufferRows>>(&());
        cursor.seek(&WrapRows(point.row), Bias::Right);
        let Some(line) = cursor.item() else {
            return self.text.len();
        };
        let wrapped_row = cursor.start().0.0;
        let row = cursor.start().1.0;

        let line_start = self.text.line_start_offset(row);
        let local_row = point.row.saturating_sub(wrapped_row);
        if let Some(range) = line.wrapped_lines.get(local_row) {
            let end = self
                .hard_breaks
                .iter()
                .find(|hard_break| {
                    hard_break.end == line_start + range.end && hard_break.start >= line_start + range.start
                })
                .map(|hard_break| hard_break.start - line_start)
                .unwrap_or(range.end);
            line_start + range.start.saturating_add(point.column).min(end)
        } else {
            // If not found, return the end of the line.
            line_start + line.len()
        }
    }

    pub(crate) fn display_point_to_point(&self, point: WrapDisplayPoint) -> TreeSitterPoint {
        let offset = self.display_point_to_offset(point);
        self.text.offset_to_point(offset)
    }

    pub(crate) fn point_to_display_point(&self, point: TreeSitterPoint) -> WrapDisplayPoint {
        let offset = self.text.point_to_offset(point);
        self.offset_to_display_point(offset)
    }
}

/// A display point within the soft-wrapped text.
///
/// This represents a position in the text after soft-wrapping,
/// with an additional `local_row` field tracking the wrap line
/// within the original buffer line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WrapDisplayPoint {
    /// The 0-based soft wrapped row index in the text.
    pub row: usize,
    /// The 0-based row index in local line (include first line).
    ///
    /// This value only valid when return from [`TextWrapper::offset_to_display_point`], otherwise it will be ignored.
    pub local_row: usize,
    /// The 0-based column byte index in the display line (with soft wrap).
    pub column: usize,
}

impl WrapDisplayPoint {
    pub(crate) fn new(row: usize, local_row: usize, column: usize) -> Self {
        Self { row, local_row, column }
    }
}

/// The layout info of a line with soft wrapped lines.
pub(crate) struct LineLayout {
    /// Total bytes length of this line.
    len: usize,
    /// The soft wrapped lines of this line (Include the first line).
    pub(crate) wrapped_lines: SmallVec<[ShapedLine; 1]>,
    /// Extra left offset applied to continuation wrapped lines, used to reserve the first line's
    /// indentation when [`WrappingIndent::Same`] is used.
    pub(crate) wrap_indent: Pixels,
    /// Per-visual-row indents when hard breaks reset continuation indentation.
    wrap_indents: Vec<Pixels>,
    pub(crate) longest_width: Pixels,
    pub(crate) whitespace_indicators: Option<WhitespaceIndicators>,
    /// Whitespace indicators: (line_index, x_position, is_tab)
    pub(crate) whitespace_chars: Vec<(usize, Pixels, bool)>,
    /// Whether any run of this line carries a background color, so [`Self::paint_background`]
    /// can skip the glyph walk for the common case of a line without highlights.
    has_background: bool,
    /// Per wrapped segment, when markers were omitted from the shaped text.
    /// Empty means every segment is shaped as its buffer bytes.
    conceal: Vec<Option<ConcealMap>>,
}

/// Buffer bytes omitted from a shaped segment, and how to translate indexes.
pub(crate) struct ConcealMap {
    /// Buffer byte index in the segment → display byte index. Length is buffer_len + 1.
    to_display: Vec<usize>,
    /// Display byte index → buffer byte index. Length is display_len + 1.
    to_buffer: Vec<usize>,
    /// This segment ends in a concealed newline, not a shared soft-wrap boundary.
    hard_break: bool,
    objects: Vec<(Range<usize>, Size<Pixels>, u64)>,
}

impl ConcealMap {
    pub(crate) fn new(to_display: Vec<usize>, to_buffer: Vec<usize>) -> Self {
        Self { to_display, to_buffer, hard_break: false, objects: Vec::new() }
    }

    pub(crate) fn present(
        source: &str,
        start: usize,
        conceal: &[Range<usize>],
        objects: &[(Range<usize>, Size<Pixels>, u64)],
    ) -> (String, Self) {
        let mut display = String::new();
        let mut map = Self::new(vec![0; source.len() + 1], vec![0]);
        let mut i = 0;
        while i < source.len() {
            let abs = start + i;
            let object = objects.iter().find(|(r, _, _)| {
                r.start == abs
                    && r.end > abs
                    && r.end <= start + source.len()
                    && source.is_char_boundary(r.end - start)
            });
            let (n, ch) = match object {
                Some((range, _, _)) => (range.end - abs, '\u{fffc}'),
                None => {
                    let ch = source[i..].chars().next().unwrap();
                    (ch.len_utf8(), ch)
                }
            };
            let at = display.len();
            map.to_display[i..i + n].fill(at);
            if object.is_some() || !conceal.iter().any(|r| r.contains(&abs)) {
                display.push(ch);
                map.to_buffer.resize(display.len() + 1, i);
                map.to_buffer[at..display.len()].fill(i);
                map.to_buffer[display.len()] = i + n;
                if let Some((_, size, key)) = object {
                    map.objects.push((at..display.len(), *size, *key));
                }
            }
            i += n;
        }
        map.to_display[source.len()] = display.len();
        (display, map)
    }

    pub(crate) fn apply_objects(&self, line: &mut ShapedLine) {
        if self.objects.is_empty() {
            return;
        }
        let deltas: Vec<_> = self
            .objects
            .iter()
            .map(|(r, size, _)| (r.end, size.width - (line.x_for_index(r.end) - line.x_for_index(r.start))))
            .collect();
        let mut layout = gpui::LineLayout {
            len: line.len,
            font_size: line.font_size,
            width: line.width,
            ascent: line.ascent,
            descent: line.descent,
            runs: line.runs.clone(),
        };
        for run in &mut layout.runs {
            for glyph in &mut run.glyphs {
                glyph.position.x += deltas
                    .iter()
                    .filter(|(end, _)| *end <= glyph.index)
                    .map(|(_, delta)| *delta)
                    .sum::<Pixels>();
            }
        }
        layout.width += deltas.iter().map(|(_, delta)| *delta).sum::<Pixels>();
        *std::ops::DerefMut::deref_mut(line) = std::sync::Arc::new(layout);
    }

    pub(crate) fn set_empty_offset(&mut self, offset: usize) {
        self.to_buffer[0] = offset;
    }

    pub(crate) fn with_hard_break(mut self, hard_break: bool) -> Self {
        self.hard_break = hard_break;
        self
    }

    pub(crate) fn buffer_len(&self) -> usize {
        self.to_display.len().saturating_sub(1)
    }

    pub(crate) fn to_display(&self, buf: usize) -> usize {
        self.to_display.get(buf).copied().unwrap_or_else(|| self.to_display.last().copied().unwrap_or(0))
    }

    pub(crate) fn to_buffer(&self, disp: usize) -> usize {
        self.to_buffer.get(disp).copied().unwrap_or_else(|| self.to_buffer.last().copied().unwrap_or(0))
    }
}

impl LineLayout {
    pub(crate) fn justify(&mut self, width: Pixels) {
        let count = self.wrapped_lines.len();
        for i in 0..count.saturating_sub(1) {
            if self.conceal.get(i).and_then(|m| m.as_ref()).is_some_and(|m| m.hard_break) {
                continue;
            }
            let indent = self.line_indent(i);
            let line = &mut self.wrapped_lines[i];
            let available = width - indent;
            let extra = available - line.width;
            if extra <= px(0.) {
                continue;
            }
            let clusters: Vec<_> = line.text.grapheme_indices(true).collect();
            let mut gaps = Vec::new();
            for pair in clusters.windows(2) {
                let (at, left) = pair[0];
                let (next, right) = pair[1];
                let a = left.chars().next().unwrap();
                let b = right.chars().next().unwrap();
                if legal_break(a, b) && (!a.is_whitespace() || at > 0) {
                    gaps.push(next);
                }
            }
            if gaps.is_empty() {
                continue;
            }
            let step = extra / gaps.len() as f32;
            let mut runs = line.runs.clone();
            for run in &mut runs {
                for glyph in &mut run.glyphs {
                    glyph.position.x += step * gaps.partition_point(|gap| *gap <= glyph.index) as f32;
                }
            }
            let layout = gpui::LineLayout {
                len: line.len,
                font_size: line.font_size,
                width: available,
                ascent: line.ascent,
                descent: line.descent,
                runs,
            };
            *std::ops::DerefMut::deref_mut(line) = std::sync::Arc::new(layout);
        }
        self.longest_width = self.wrapped_lines.iter().map(|line| line.width).max().unwrap_or_default();
        if self.whitespace_indicators.is_some() {
            self.whitespace_chars.clear();
            let indicators = self.whitespace_indicators.clone();
            let current = std::mem::replace(self, Self::new());
            *self = current.with_whitespaces(indicators);
        }
    }

    pub(crate) fn new() -> Self {
        Self {
            len: 0,
            longest_width: px(0.),
            wrapped_lines: SmallVec::new(),
            wrap_indent: px(0.),
            wrap_indents: Vec::new(),
            whitespace_chars: Vec::new(),
            whitespace_indicators: None,
            has_background: false,
            conceal: Vec::new(),
        }
    }

    /// Record that some wrapped segments were shaped without their marker bytes.
    /// `self.len` stays the buffer length, so caret indexes remain buffer offsets.
    pub(crate) fn with_conceal(mut self, conceal: Vec<Option<ConcealMap>>) -> Self {
        if conceal.iter().any(|map| map.is_some()) {
            let mut len = 0;
            for (i, map) in conceal.iter().enumerate() {
                len += map
                    .as_ref()
                    .map(|map| map.buffer_len())
                    .unwrap_or_else(|| self.wrapped_lines.get(i).map(|line| line.len).unwrap_or(0));
            }
            self.len = len;
            self.conceal = conceal;
        }
        self
    }

    fn segment_buffer_len(&self, index: usize) -> usize {
        self.conceal
            .get(index)
            .and_then(|map| map.as_ref())
            .map(|map| map.buffer_len())
            .unwrap_or_else(|| self.wrapped_lines.get(index).map(|line| line.len).unwrap_or(0))
    }

    fn display_index(&self, segment: usize, buffer: usize) -> usize {
        self.conceal
            .get(segment)
            .and_then(|map| map.as_ref())
            .map(|map| map.to_display(buffer))
            .unwrap_or(buffer)
    }

    fn buffer_index(&self, segment: usize, display: usize) -> usize {
        self.conceal
            .get(segment)
            .and_then(|map| map.as_ref())
            .map(|map| map.to_buffer(display))
            .unwrap_or(display)
    }

    /// Record whether any run of this line carries a background color.
    pub(crate) fn with_background(mut self, has_background: bool) -> Self {
        self.has_background = has_background;
        self
    }

    /// Set the left offset reserved for continuation wrapped lines.
    pub(crate) fn wrap_indent(mut self, wrap_indent: Pixels) -> Self {
        self.wrap_indent = wrap_indent;
        self
    }

    pub(crate) fn with_wrap_indents(mut self, wrap_indents: Vec<Pixels>) -> Self {
        self.wrap_indents = wrap_indents;
        self
    }

    /// The pixel indent applied to a visual line. Hard breaks start fresh groups;
    /// only soft continuations inherit the first row's indentation.
    #[inline]
    pub(crate) fn line_indent(&self, line_index: usize) -> Pixels {
        self.wrap_indents
            .get(line_index)
            .copied()
            .unwrap_or_else(|| if line_index == 0 { px(0.) } else { self.wrap_indent })
    }

    fn row_offset(&self, row: usize, align: TextAlign, width: Pixels) -> Pixels {
        let indent = self.line_indent(row);
        indent + LastLayout::alignment_offset(align, width, indent + self.wrapped_lines[row].width)
    }

    pub(crate) fn row_height(&self, row: usize, base: Pixels) -> Pixels {
        self.conceal
            .get(row)
            .and_then(|map| map.as_ref())
            .map_or(base, |map| map.objects.iter().map(|(_, size, _)| size.height).fold(base, Pixels::max))
    }

    pub(crate) fn row_top(&self, row: usize, base: Pixels) -> Pixels {
        row * base
            + self
                .conceal
                .iter()
                .take(row)
                .flatten()
                .map(|map| map.objects.iter().map(|(_, size, _)| size.height).fold(base, Pixels::max) - base)
                .sum::<Pixels>()
    }

    pub(crate) fn row_at_y(&self, y: Pixels, base: Pixels) -> usize {
        let mut bottom = px(0.);
        for row in 0..self.wrapped_lines.len() {
            bottom += self.row_height(row, base);
            if y < bottom {
                return row;
            }
        }
        self.wrapped_lines.len().saturating_sub(1)
    }

    pub(crate) fn object_bounds(&self, layout: &LastLayout) -> Vec<(u64, Bounds<Pixels>)> {
        let mut out = Vec::new();
        for (row, map) in self.conceal.iter().enumerate() {
            if let Some(map) = map {
                for (range, size, key) in &map.objects {
                    let x = self.row_offset(row, layout.text_align, layout.content_width)
                        + self.wrapped_lines[row].x_for_index(range.start);
                    let y = self.row_top(row, layout.line_height)
                        + (self.row_height(row, layout.line_height) - size.height).half();
                    out.push((*key, Bounds::new(point(x, y), *size)));
                }
            }
        }
        out
    }

    pub(crate) fn range_bounds(&self, range: Range<usize>, layout: &LastLayout) -> Vec<Bounds<Pixels>> {
        self.range_bounds_with_affinity(range, layout, false)
    }

    pub(crate) fn range_bounds_with_affinity(&self, range: Range<usize>, layout: &LastLayout, affinity: bool) -> Vec<Bounds<Pixels>> {
        if range.start > self.len() || range.end < range.start {
            return Vec::new();
        }
        let Some(start) = self.position_for_index(range.start, layout, range.is_empty() && affinity) else {
            return Vec::new();
        };
        let Some(end) = self.position_for_index(range.end.min(self.len()), layout, !range.is_empty() || affinity) else {
            return Vec::new();
        };
        let first = self.row_at_y(start.y, layout.line_height);
        let last = self.row_at_y(end.y, layout.line_height);
        (first..=last)
            .map(|row| {
                let left = self.row_offset(row, layout.text_align, layout.content_width);
                let start_x = if row == first { start.x } else { left };
                let end_x = if row == last { end.x } else { left + self.wrapped_lines[row].width };
                Bounds::from_corners(
                    point(start_x.min(end_x), self.row_top(row, layout.line_height)),
                    point(start_x.max(end_x), self.row_top(row + 1, layout.line_height)),
                )
            })
            .collect()
    }

    pub(crate) fn lines(mut self, wrapped_lines: SmallVec<[ShapedLine; 1]>) -> Self {
        self.set_wrapped_lines(wrapped_lines);
        self
    }

    pub(crate) fn set_wrapped_lines(&mut self, wrapped_lines: SmallVec<[ShapedLine; 1]>) {
        self.len = wrapped_lines.iter().map(|l| l.len).sum();
        let width = wrapped_lines.iter().map(|l| l.width).max().unwrap_or_default();
        self.longest_width = width;
        self.wrapped_lines = wrapped_lines;
    }

    pub(crate) fn with_whitespaces(mut self, indicators: Option<WhitespaceIndicators>) -> Self {
        self.whitespace_indicators = indicators;
        let Some(indicators) = self.whitespace_indicators.as_ref() else {
            return self;
        };

        let space_indicator_offset = indicators.space.width.half();

        for (line_index, wrapped_line) in self.wrapped_lines.iter().enumerate() {
            for (relative_offset, c) in wrapped_line.text.char_indices() {
                if matches!(c, ' ' | '\t') {
                    let is_tab = c == '\t';
                    let start_x = wrapped_line.x_for_index(relative_offset);
                    let end_x = wrapped_line.x_for_index(relative_offset + c.len_utf8());
                    // Center the indicator in the actual character's space
                    let x_position =
                        if c == ' ' { (start_x + end_x).half() - space_indicator_offset } else { start_x };

                    self.whitespace_chars.push((line_index, x_position, is_tab));
                }
            }
        }
        self
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Get the position (x, y) for the given index in this line layout.
    ///
    /// - The `offset` is a local byte index in this line layout.
    /// - When `line_end_affinity` is true, an offset at a soft wrap boundary is placed at
    ///   the end of the current visual line rather than the start of the next one.
    /// - The return value is relative to the top-left corner of this line layout, start from (0, 0)
    pub(crate) fn position_for_index(
        &self,
        offset: usize,
        last_layout: &LastLayout,
        line_end_affinity: bool,
    ) -> Option<Point<Pixels>> {
        let mut acc_len = 0;
        let mut offset_y = px(0.);

        for (i, line) in self.wrapped_lines.iter().enumerate() {
            let is_last = i + 1 == self.wrapped_lines.len();
            let buf_len = self.segment_buffer_len(i);
            let hard_break =
                self.conceal.get(i).and_then(|map| map.as_ref()).is_some_and(|map| map.hard_break);

            let matches = if buf_len == 0 {
                // Empty visual lines still own their boundary offset.
                offset == acc_len
            } else if is_last || (line_end_affinity && !hard_break) {
                // Only a soft wrap shares its end offset with the following row.
                offset >= acc_len && offset <= acc_len + buf_len
            } else {
                // Exclusive: boundary offset belongs to the next visual line.
                offset >= acc_len && offset < acc_len + buf_len
            };

            if matches {
                let local = offset.saturating_sub(acc_len);
                let display = self.display_index(i, local);
                let x = line.x_for_index(display)
                    + self.row_offset(i, last_layout.text_align, last_layout.content_width);
                return Some(point(x, offset_y));
            }

            // Always advance by the buffer length. The last line gets +1 so the
            // cursor can be placed after the final character.
            acc_len += if is_last { buf_len + 1 } else { buf_len };
            offset_y += self.row_height(i, last_layout.line_height);
        }

        None
    }

    /// Get the closest index for the given x in this line layout.
    ///
    /// This ignores y, so it only makes sense for a layout that is known to occupy a single
    /// visual line. Wrapped layouts must use [`Self::closest_index_for_position`], which also
    /// reports the caret affinity that a wrap boundary needs.
    pub(crate) fn closest_index_for_x(&self, x: Pixels, last_layout: &LastLayout) -> usize {
        let mut acc_len = 0;
        for (i, line) in self.wrapped_lines.iter().enumerate() {
            let x = x - self.row_offset(i, last_layout.text_align, last_layout.content_width);
            if x <= line.width {
                let display = line.closest_index_for_x(x);
                return acc_len + self.buffer_index(i, display);
            }
            acc_len += self.segment_buffer_len(i);
        }

        acc_len
    }

    /// Resolve `pos` to the wrapped sub-line under it.
    ///
    /// Returns the sub-line index, the byte offset that sub-line starts at within this line
    /// layout, and `pos.x` translated into that sub-line's own coordinate space.
    fn wrapped_line_at(
        &self,
        pos: Point<Pixels>,
        last_layout: &LastLayout,
    ) -> Option<(usize, usize, Pixels)> {
        let mut offset = 0;
        let mut line_top = px(0.);
        for i in 0..self.wrapped_lines.len() {
            let line_bottom = line_top + self.row_height(i, last_layout.line_height);
            if pos.y >= line_top && pos.y < line_bottom {
                return Some((
                    i,
                    offset,
                    pos.x - self.row_offset(i, last_layout.text_align, last_layout.content_width),
                ));
            }

            offset += self.segment_buffer_len(i);
            line_top = line_bottom;
        }

        None
    }

    /// Get the index for the given position (x, y) in this line layout.
    ///
    /// The `pos` is relative to the top-left corner of this line layout, start from (0, 0).
    ///
    /// Returns a local byte index in this line layout (start from 0) together with the caret
    /// affinity to use for it: `true` when the index landed on the wrap boundary of a non-final
    /// sub-line. That boundary offset is shared by the end of one visual line and the start of
    /// the next, so the affinity is what tells [`Self::position_for_index`] which of the two the
    /// caret belongs to. Without it a click past the last glyph of a wrapped line would put a
    /// visible caret on the following line.
    pub(crate) fn closest_index_for_position(
        &self,
        pos: Point<Pixels>,
        last_layout: &LastLayout,
    ) -> Option<(usize, bool)> {
        let (i, offset, x) = self.wrapped_line_at(pos, last_layout)?;
        let line = &self.wrapped_lines[i];
        let display = line.closest_index_for_x(x);
        let buffer = self.buffer_index(i, display);
        // A concealed hard break maps the last glyph (or an empty row) to the
        // position before the token, not to the next row's source boundary.
        let hard_break = self.conceal.get(i).and_then(|map| map.as_ref()).is_some_and(|map| map.hard_break);
        let line_end_affinity = i + 1 < self.wrapped_lines.len() && display == line.len && !hard_break;

        Some((offset + buffer, line_end_affinity))
    }

    /// How many columns the given position sits past the end of the line under it.
    ///
    /// Past the end of a line there is no glyph to hit-test against, so a position out
    /// there resolves to the line end and loses how far right it really was. The extra
    /// distance is reported here in whole spaces, letting a columnar selection keep its
    /// width over a short row. Only the final visual line of a wrapped layout has that
    /// trailing space; a continuation line ends at a wrap boundary, where the next glyph
    /// merely lives on the following row.
    ///
    /// The `pos` is relative to the top-left corner of this line layout, start from (0, 0).
    pub(crate) fn columns_past_line_end(&self, pos: Point<Pixels>, last_layout: &LastLayout) -> usize {
        let Some((i, _, x)) = self.wrapped_line_at(pos, last_layout) else {
            return 0;
        };

        if i + 1 < self.wrapped_lines.len() || last_layout.space_width <= px(0.) {
            return 0;
        }

        let past_end = x - self.wrapped_lines[i].width;
        if past_end <= px(0.) {
            return 0;
        }

        (past_end / last_layout.space_width).round() as usize
    }

    pub(crate) fn index_for_position(&self, pos: Point<Pixels>, last_layout: &LastLayout) -> Option<usize> {
        let (i, offset, x) = self.wrapped_line_at(pos, last_layout)?;
        let display = self.wrapped_lines[i].index_for_x(x)?;

        Some(offset + self.buffer_index(i, display))
    }

    pub(crate) fn size(&self, line_height: Pixels) -> Size<Pixels> {
        let width = self
            .wrapped_lines
            .iter()
            .enumerate()
            .map(|(ix, line)| line.width + self.line_indent(ix))
            .max()
            .unwrap_or(self.longest_width);
        size(width, self.row_top(self.wrapped_lines.len(), line_height))
    }

    /// Paint only the glyph background quads of this line.
    ///
    /// gpui's [`ShapedLine::paint`] does not draw backgrounds, so every line painted with
    /// [`Self::paint`] needs this called first, with the same origin and align width.
    pub(crate) fn paint_background(
        &self,
        pos: Point<Pixels>,
        line_height: Pixels,
        text_align: TextAlign,
        align_width: Option<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Painting a background walks every glyph and pushes a scene layer, so skip the
        // whole pass for lines that have no background color to paint.
        if !self.has_background {
            return;
        }

        for (ix, line) in self.wrapped_lines.iter().enumerate() {
            _ = line.paint_background(
                pos + point(
                    self.row_offset(ix, text_align, align_width.unwrap_or(line.width)),
                    self.row_top(ix, line_height),
                ),
                self.row_height(ix, line_height),
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }
    }

    pub(crate) fn paint(
        &self,
        pos: Point<Pixels>,
        line_height: Pixels,
        text_align: TextAlign,
        align_width: Option<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        for (ix, line) in self.wrapped_lines.iter().enumerate() {
            _ = line.paint(
                pos + point(
                    self.row_offset(ix, text_align, align_width.unwrap_or(line.width)),
                    self.row_top(ix, line_height),
                ),
                self.row_height(ix, line_height),
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }

        // Paint whitespace indicators
        if let Some(indicators) = self.whitespace_indicators.as_ref() {
            for (line_index, x_position, is_tab) in &self.whitespace_chars {
                let invisible = if *is_tab { indicators.tab.clone() } else { indicators.space.clone() };

                let origin = point(
                    pos.x
                        + *x_position
                        + self.row_offset(
                            *line_index,
                            text_align,
                            align_width.unwrap_or(self.wrapped_lines[*line_index].width),
                        ),
                    pos.y + self.row_top(*line_index, line_height),
                );

                _ = invisible.paint(
                    origin,
                    self.row_height(*line_index, line_height),
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    use gpui::{Boundary, FontFeatures, FontStyle, FontWeight, px};

    fn glyph_layout(runs: &[&[(usize, f32)]], len: usize, width: f32) -> gpui::LineLayout {
        gpui::LineLayout {
            len,
            width: px(width),
            runs: runs
                .iter()
                .enumerate()
                .map(|(font, glyphs)| gpui::ShapedRun {
                    font_id: gpui::FontId(font),
                    glyphs: glyphs
                        .iter()
                        .map(|&(index, x)| gpui::ShapedGlyph {
                            id: gpui::GlyphId(0),
                            index,
                            position: point(px(x), px(0.)),
                            is_emoji: false,
                        })
                        .collect(),
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn batch_x_positions_match_gpui_for_missing_and_unordered_glyphs() {
        let layouts = [
            glyph_layout(&[], 0, 0.),
            glyph_layout(&[&[], &[]], 12, 47.),
            // No glyph at zero, ligature interiors, duplicate cluster indexes, and a missing tail.
            glyph_layout(&[&[(2, -3.), (5, 12.), (5, 99.)], &[], &[(9, 7.)]], 12, 47.),
            // Descending (RTL) indexes and overlapping indexes across different font runs.
            glyph_layout(&[&[(6, 30.), (4, 20.), (2, 10.)], &[(1, 90.), (8, 15.), (7, 80.)]], 12, 47.),
            // Even at/past len, GPUI still prefers a qualifying glyph over the line width.
            glyph_layout(&[&[(0, 4.), (12, 55.)], &[(15, -8.)]], 12, 47.),
        ];
        let indices = [12, 0, 1, 2, 3, 4, 5, 5, 6, 7, 8, 9, 10, 11, 12, 13, 15, 16, usize::MAX, 0];
        for layout in layouts {
            let expected: Vec<_> = indices.iter().map(|&index| layout.x_for_index(index)).collect();
            let actual = batch_x_for_indices(
                layout.runs.iter().flat_map(|run| run.glyphs.iter()),
                layout.width,
                indices,
            );
            assert_eq!(actual, expected, "layout: {layout:?}");
        }
    }

    #[test]
    fn batch_x_positions_match_gpui_for_every_small_glyph_order() {
        // Exhaustively cover repeats and arbitrary run/glyph order, not just sorted layouts.
        for glyph_count in 0..=6 {
            for mut encoding in 0..4usize.pow(glyph_count) {
                let glyphs: Vec<_> = (0..glyph_count)
                    .map(|ordinal| {
                        let index = encoding % 4;
                        encoding /= 4;
                        (index, 20. - ordinal as f32 * 3.)
                    })
                    .collect();
                let split = glyphs.len() / 2;
                let layout = glyph_layout(&[&glyphs[..split], &[], &glyphs[split..]], 4, 31.);
                let indices = [0, 1, 2, 3, 4, 5, 2, 0];
                let actual = batch_x_for_indices(
                    layout.runs.iter().flat_map(|run| run.glyphs.iter()),
                    layout.width,
                    indices,
                );
                let expected: Vec<_> = indices.iter().map(|&index| layout.x_for_index(index)).collect();
                assert_eq!(actual, expected, "layout: {layout:?}");
            }
        }
    }

    #[test]
    fn batch_x_positions_visit_each_glyph_at_most_once() {
        for count in [256, 1024, 4096] {
            for unordered in [false, true] {
                let glyphs: Vec<_> = (0..count)
                    .map(|ordinal| {
                        let index = if unordered { ordinal ^ 1 } else { ordinal };
                        (index, ordinal as f32)
                    })
                    .collect();
                let layout =
                    glyph_layout(&[&glyphs[..count / 2], &[], &glyphs[count / 2..]], count, count as f32);
                let visits = std::cell::Cell::new(0);
                let actual = batch_x_for_indices(
                    layout
                        .runs
                        .iter()
                        .flat_map(|run| run.glyphs.iter())
                        .inspect(|_| visits.set(visits.get() + 1)),
                    layout.width,
                    (0..=count).rev(),
                );
                let expected: Vec<_> = (0..=count).rev().map(|index| layout.x_for_index(index)).collect();
                assert_eq!(actual, expected);
                assert!(visits.get() <= count, "{count} glyphs required {} visits", visits.get());
            }
        }
    }

    #[gpui::test]
    fn presented_wrap_matches_gpui_with_conceal_and_mixed_fonts(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let measure = Measure {
                text: WindowTextSystem::new(cx.text_system().clone()),
                base: test_font(),
                font_size: px(14.),
            };
            let source = "**  of~~fi~~ce e\u{301}，中文 אבג 🙂 tail**";
            let display = "  office e\u{301}，中文 אבג 🙂 tail";
            let line_start = 37;
            let bold = FontOverride { weight: Some(FontWeight::BOLD), ..Default::default() };
            let italic = FontOverride {
                style: Some(FontStyle::Italic),
                family: Some("Courier New".into()),
                ..Default::default()
            };
            let split = display.find('中').unwrap();
            let fonts = vec![
                (line_start + 2..line_start + source.len() - 2, bold.clone()),
                (line_start + source.find('中').unwrap()..line_start + source.len() - 2, italic.clone()),
            ];
            let runs: Vec<_> = [
                (split, measure.font([&bold].into_iter())),
                (display.len() - split, measure.font([&bold, &italic].into_iter())),
            ]
            .into_iter()
            .map(|(len, font)| TextRun {
                len,
                font,
                color: gpui::black(),
                background_color: None,
                underline: None,
                strikethrough: None,
            })
            .collect();
            let layout = measure.text.layout_line(display, measure.font_size, &runs, None);
            let visible_source = source.grapheme_indices(true).filter(|&(_, ch)| ch != "*" && ch != "~");
            let chars: Vec<_> = display
                .grapheme_indices(true)
                .zip(visible_source)
                .map(|((at, cluster), (source_at, source_cluster))| {
                    assert_eq!(cluster, source_cluster);
                    (
                        source_at,
                        cluster.chars().next().unwrap(),
                        layout.x_for_index(at + cluster.len()) - layout.x_for_index(at),
                    )
                })
                .collect();
            let conceal: Vec<_> = source
                .match_indices("**")
                .chain(source.match_indices("~~"))
                .map(|(at, marker)| line_start + at..line_start + at + marker.len())
                .collect();
            for width in [px(1.), px(35.), px(75.), px(1000.)] {
                let expected = wrap_chars(&chars, measure.space(), width);
                let actual =
                    TextWrapper::wrap_presented_line(&measure, source, line_start, &conceal, &fonts, width);
                let boundaries = |items: Vec<Boundary>| {
                    items.into_iter().map(|b| (b.ix, b.next_indent)).collect::<Vec<_>>()
                };
                assert_eq!(boundaries(actual), boundaries(expected));
            }
            assert!(
                TextWrapper::wrap_presented_line(
                    &measure,
                    source,
                    line_start,
                    &[line_start..line_start + source.len()],
                    &fonts,
                    px(1.),
                )
                .is_empty()
            );
        });
    }

    #[gpui::test]
    fn g10_presented_wrap_keeps_graphemes_and_words_atomic(cx: &mut gpui::TestAppContext) {
        use unicode_segmentation::UnicodeSegmentation;
        cx.update(|cx| {
            let measure = Measure {
                text: WindowTextSystem::new(cx.text_system().clone()),
                base: test_font(),
                font_size: px(14.),
            };
            for atom in ["👍🏽", "👩‍💻", "🇨🇳", "e\u{301}clair", "unbreakableEnglishword", "Καλημέρα"]
            {
                let source = format!("**{atom}** tail");
                let conceal = vec![0..2, 2 + atom.len()..4 + atom.len()];
                let breaks = TextWrapper::wrap_presented_line(&measure, &source, 0, &conceal, &[], px(1.));
                assert_eq!(
                    breaks.iter().map(|b| b.ix).collect::<Vec<_>>(),
                    vec![source.find("tail").unwrap()],
                    "{source}"
                );
                assert!(breaks.iter().all(|b| source.grapheme_indices(true).any(|(at, _)| at == b.ix)));
            }
        });
    }

    #[test]
    fn g10_justify_uses_legal_gaps_and_leaves_hard_break_rows_left() {
        let row = |text: &str| {
            let glyphs: Vec<_> =
                text.grapheme_indices(true).enumerate().map(|(i, (at, _))| (at, i as f32 * 10.)).collect();
            let mut row = ShapedLine::default();
            row.text = text.to_owned().into();
            *std::ops::DerefMut::deref_mut(&mut row) =
                std::sync::Arc::new(glyph_layout(&[&glyphs], text.len(), 30.));
            row
        };
        let mut layout = LineLayout::new()
            .lines(smallvec::smallvec![row("甲ぁ乙"), row("甲 乙"), row("末")])
            .with_conceal(vec![
                None,
                Some(ConcealMap::new((0..=7).collect(), (0..=7).collect()).with_hard_break(true)),
                None,
            ]);
        layout.justify(px(100.));
        assert_eq!(layout.wrapped_lines[0].x_for_index(3), px(10.), "small kana stays attached");
        assert_eq!(layout.wrapped_lines[0].x_for_index(6), px(90.));
        assert_eq!(layout.wrapped_lines[1].width, px(30.), "hard-break final row");
        assert_eq!(layout.wrapped_lines[2].width, px(30.), "paragraph final row");
    }

    #[gpui::test]
    fn g10_inline_objects_wrap_atomically_and_reflow_on_resize(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let source = "甲 $abcdefgh$ 乙 $x$ 丙";
            let mut wrapper = TextWrapper::new(test_font(), px(16.), Some(px(90.)));
            wrapper.prepare_if_need(&Rope::from(source), cx);
            let objects = [(4..14, size(px(75.), px(42.))), (19..22, size(px(65.), px(18.)))];
            let fonts = objects
                .iter()
                .map(|(range, size)| {
                    (range.clone(), FontOverride { object: Some(*size), ..Default::default() })
                })
                .collect();
            wrapper.set_conceal(vec![], vec![], fonts, cx);
            let rows = &wrapper.line(0).unwrap().wrapped_lines;
            assert!(rows.len() >= 3, "measured objects must consume wrap width: {rows:?}");
            for (object, _) in &objects {
                assert!(rows.iter().any(|row| row.start <= object.start && row.end >= object.end));
                assert!(!rows.iter().any(|row| object.start < row.end && row.end < object.end));
            }
            let before = rows.len();
            wrapper.set_conceal(
                vec![],
                vec![],
                objects
                    .iter()
                    .map(|(range, _)| {
                        (
                            range.clone(),
                            FontOverride { object: Some(size(px(5.), px(16.))), ..Default::default() },
                        )
                    })
                    .collect(),
                cx,
            );
            assert!(wrapper.line(0).unwrap().wrapped_lines.len() < before);
        });
    }

    #[gpui::test]
    fn g10_object_rows_share_alignment_selection_and_actual_height(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let text = WindowTextSystem::new(cx.text_system().clone());
            for (align, justify) in
                [(TextAlign::Center, false), (TextAlign::Right, false), (TextAlign::Left, true)]
            {
                let (display, map) =
                    ConcealMap::present("前 $x$ 后", 0, &[], &[(4..7, size(px(80.), px(48.)), 42)]);
                let mut shaped = text.shape_line(
                    display.clone().into(),
                    px(16.),
                    &[TextRun {
                        len: display.len(),
                        font: test_font(),
                        color: gpui::black(),
                        ..Default::default()
                    }],
                    None,
                );
                map.apply_objects(&mut shaped);
                let final_row = text.shape_line(
                    "末".into(),
                    px(16.),
                    &[TextRun { len: 3, font: test_font(), color: gpui::black(), ..Default::default() }],
                    None,
                );
                let mut line = LineLayout::new()
                    .lines(smallvec::smallvec![shaped, final_row])
                    .with_conceal(vec![Some(map), None]);
                if justify {
                    line.justify(px(240.));
                }
                let mut layout = test_last_layout(px(20.));
                layout.content_width = px(240.);
                layout.text_align = align;
                assert_eq!(line.size(px(20.)).height, px(68.));
                let rect = line.object_bounds(&layout)[0].1;
                assert_eq!(rect.size, size(px(80.), px(48.)));
                assert_eq!(line.position_for_index(4, &layout, false).unwrap().x, rect.left());
                assert_eq!(line.position_for_index(11, &layout, false).unwrap().y, px(48.));
                assert_eq!(
                    line.closest_index_for_position(rect.origin + point(px(2.), px(22.)), &layout).unwrap().0,
                    4
                );
                assert_eq!(line.range_bounds(4..7, &layout)[0].size.height, px(48.));
            }
        });
    }

    #[test]
    fn g10_object_source_mapping_keeps_utf8_edges() {
        let (display, map) = ConcealMap::present("😀$abc$乙", 0, &[], &[(4..9, size(px(80.), px(44.)), 7)]);
        assert_eq!(display, "😀\u{fffc}乙");
        for at in 4..9 {
            assert_eq!(map.to_display(at), 4);
        }
        assert_eq!(map.to_display(9), 7);
        assert_eq!(map.to_buffer(4), 4);
        assert_eq!(map.to_buffer(7), 9);
        assert_eq!(map.to_buffer(display.len()), 12);
    }

    #[test]
    fn g10_overflow_waits_for_legal_cjk_punctuation_break() {
        for (source, expected) in
            [("「甲」乙", vec![9]), ("甲，乙。丙", vec![6, 12]), ("longword next", vec![9])]
        {
            let chars: Vec<_> = source.char_indices().map(|(at, c)| (at, c, px(10.))).collect();
            let breaks = wrap_chars(&chars, px(10.), px(1.));
            assert_eq!(breaks.iter().map(|b| b.ix).collect::<Vec<_>>(), expected, "{source}");
        }
    }

    #[test]
    fn test_update() {
        let font = gpui::Font {
            family: "Arial".into(),
            weight: FontWeight::default(),
            style: FontStyle::Normal,
            features: FontFeatures::default(),
            fallbacks: None,
        };

        let mut wrapper = TextWrapper::new(font, px(14.), None);
        let mut text =
            Rope::from("Hello, 世界!\r\nThis is second line.\nThis is third line.\n这里是第 4 行。");

        fn fake_wrap_line(_line: &str, _line_start: usize, _wrap_width: Pixels) -> Vec<Boundary> {
            vec![]
        }

        #[track_caller]
        fn assert_wrapper_lines(text: &Rope, wrapper: &TextWrapper, expected_lines: &[&[&str]]) {
            let mut actual_lines = vec![];
            let mut offset = 0;
            for line in wrapper.iter_lines() {
                actual_lines.push(
                    line.wrapped_lines
                        .iter()
                        .map(|range| text.slice(offset + range.start..offset + range.end))
                        .collect::<Vec<_>>(),
                );
                // +1 \n
                offset += line.len() + 1;
            }
            assert_eq!(actual_lines, expected_lines);
        }

        wrapper._update(&text, &(0..text.len()), &text, &mut fake_wrap_line);
        assert_eq!(wrapper.lines_count(), 4);
        assert_wrapper_lines(
            &text,
            &wrapper,
            &[&["Hello, 世界!\r"], &["This is second line."], &["This is third line."], &["这里是第 4 行。"]],
        );

        // Add a new text to end
        let range = text.len()..text.len();
        let new_text = "New text";
        text.replace(range.clone(), new_text);
        wrapper._update(&text, &range, &Rope::from(new_text), &mut fake_wrap_line);
        assert_eq!(
            text.to_string(),
            "Hello, 世界!\r\nThis is second line.\nThis is third line.\n这里是第 4 行。New text"
        );
        assert_eq!(wrapper.lines_count(), 4);
        assert_eq!(wrapper.lines_count(), 4);
        assert_wrapper_lines(
            &text,
            &wrapper,
            &[
                &["Hello, 世界!\r"],
                &["This is second line."],
                &["This is third line."],
                &["这里是第 4 行。New text"],
            ],
        );

        // Replace first line `Hello` to `AAA`
        let range = 0..5;
        let new_text = "AAA";
        text.replace(range.clone(), new_text);
        wrapper._update(&text, &range, &Rope::from(new_text), &mut fake_wrap_line);
        assert_eq!(
            text.to_string(),
            "AAA, 世界!\r\nThis is second line.\nThis is third line.\n这里是第 4 行。New text"
        );
        assert_eq!(wrapper.lines_count(), 4);
        assert_wrapper_lines(
            &text,
            &wrapper,
            &[
                &["AAA, 世界!\r"],
                &["This is second line."],
                &["This is third line."],
                &["这里是第 4 行。New text"],
            ],
        );

        // Remove the second line
        let start_offset = text.line_start_offset(1);
        let end_offset = text.line_end_offset(1);
        let range = start_offset..end_offset + 1;
        text.replace(range.clone(), "");
        wrapper._update(&text, &range, &Rope::from(""), &mut fake_wrap_line);
        assert_eq!(text.to_string(), "AAA, 世界!\r\nThis is third line.\n这里是第 4 行。New text");
        assert_eq!(wrapper.lines_count(), 3);
        assert_wrapper_lines(
            &text,
            &wrapper,
            &[&["AAA, 世界!\r"], &["This is third line."], &["这里是第 4 行。New text"]],
        );

        // Replace the first 2 lines to "This is a new line."
        let range = text.line_start_offset(0)..text.line_end_offset(1) + 1;
        let new_text = "This is a new line.\nThis is new line 2.\n";
        text.replace(range.clone(), new_text);
        wrapper._update(&text, &range, &Rope::from(new_text), &mut fake_wrap_line);
        assert_eq!(text.to_string(), "This is a new line.\nThis is new line 2.\n这里是第 4 行。New text");
        assert_eq!(wrapper.lines_count(), 3);
        assert_wrapper_lines(
            &text,
            &wrapper,
            &[&["This is a new line."], &["This is new line 2."], &["这里是第 4 行。New text"]],
        );

        // Add a new line at the end
        let range = text.len()..text.len();
        let new_text = "\nThis is a new line at the end.";
        text.replace(range.clone(), new_text);
        wrapper._update(&text, &range, &Rope::from(new_text), &mut fake_wrap_line);
        assert_eq!(
            text.to_string(),
            "This is a new line.\nThis is new line 2.\n这里是第 4 行。New text\nThis is a new line at the end."
        );
        assert_eq!(wrapper.lines_count(), 4);
        assert_wrapper_lines(
            &text,
            &wrapper,
            &[
                &["This is a new line."],
                &["This is new line 2."],
                &["这里是第 4 行。New text"],
                &["This is a new line at the end."],
            ],
        );

        // Add a new line at the beginning
        let range = 0..0;
        let new_text = "This is a new line at the beginning.\n";
        text.replace(range.clone(), new_text);
        wrapper._update(&text, &range, &Rope::from(new_text), &mut fake_wrap_line);
        assert_eq!(
            text.to_string(),
            "This is a new line at the beginning.\nThis is a new line.\nThis is new line 2.\n这里是第 4 行。New text\nThis is a new line at the end."
        );
        assert_eq!(wrapper.lines_count(), 5);
        assert_wrapper_lines(
            &text,
            &wrapper,
            &[
                &["This is a new line at the beginning."],
                &["This is a new line."],
                &["This is new line 2."],
                &["这里是第 4 行。New text"],
                &["This is a new line at the end."],
            ],
        );

        // Remove all to at least one line in `lines`.
        let range = 0..text.len();
        let new_text = "";
        text.replace(range.clone(), new_text);
        wrapper._update(&text, &range, &Rope::from(new_text), &mut fake_wrap_line);
        assert_eq!(text.to_string(), "");
        assert_eq!(wrapper.lines_count(), 1);
        assert_eq!(wrapper.line(0).unwrap().wrapped_lines.as_slice(), [0..0]);

        // Test update_all
        let range = 0..text.len();
        let new_text = "This is a full text.\nThis is a second line.";
        text.replace(range.clone(), new_text);
        wrapper._update(&text, &range, &text, &mut fake_wrap_line);
        assert_eq!(text.to_string(), "This is a full text.\nThis is a second line.");
        assert_eq!(wrapper.lines_count(), 2);
    }

    fn test_font() -> gpui::Font {
        gpui::Font {
            family: "Arial".into(),
            weight: FontWeight::default(),
            style: FontStyle::Normal,
            features: FontFeatures::default(),
            fallbacks: None,
        }
    }

    #[gpui::test]
    fn hard_breaks_keep_source_ranges_and_empty_rows(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            for width in [None, Some(px(1000.))] {
                for (source, breaks, expected) in [
                    ("甲<br><br>乙", vec![3..7, 7..11], vec![0..7, 7..11, 11..14]),
                    ("<br>甲<br>", vec![0..4, 7..11], vec![0..4, 4..11, 11..11]),
                    ("<br><br>", vec![0..4, 4..8], vec![0..4, 4..8, 8..8]),
                ] {
                    let text = Rope::from(source);
                    let mut wrapper = TextWrapper::new(test_font(), px(14.), width);
                    wrapper.conceal = breaks.clone();
                    wrapper.hard_breaks = breaks;
                    wrapper.update_all(&text, cx);
                    assert_eq!(wrapper.text().to_string(), source);
                    assert_eq!(wrapper.lines_count(), 1);
                    assert_eq!(wrapper.line(0).unwrap().wrapped_lines.as_slice(), expected);
                    for (row, range) in expected.iter().enumerate() {
                        for affinity in [false, true] {
                            assert_eq!(
                                wrapper.offset_to_display_point_with_affinity(range.start, affinity),
                                WrapDisplayPoint::new(row, row, 0),
                            );
                        }
                        assert_eq!(
                            wrapper.display_point_to_offset(WrapDisplayPoint::new(row, 0, 0)),
                            range.start
                        );
                    }
                }
            }
        });
    }

    #[gpui::test]
    fn hard_breaks_clamp_display_columns_before_hidden_tokens(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut wrapper = TextWrapper::new(test_font(), px(14.), None);
            let text = Rope::from("前\n甲<br><br>乙<br>\n后");
            let breaks = vec![7..11, 11..15, 18..22];
            wrapper.update_all(&text, cx);
            wrapper.set_conceal(breaks.clone(), breaks.clone(), vec![], cx);
            assert_eq!(wrapper.lines_count(), 3);
            assert_eq!(wrapper.buffer_line_to_wrap_row_range(1), 1..5);
            for (row, start, end) in [(1, 4, 7), (2, 11, 11), (3, 15, 18), (4, 22, 22)] {
                assert_eq!(wrapper.display_point_to_offset(WrapDisplayPoint::new(row, 0, 0)), start);
                for column in [3, 4, 5, 999] {
                    assert_eq!(wrapper.display_point_to_offset(WrapDisplayPoint::new(row, 0, column)), end);
                }
            }
            // Changing only hard breaks must invalidate the layout, even with wrapping off.
            wrapper.set_conceal(breaks.clone(), vec![], vec![], cx);
            assert_eq!(wrapper.len(), 3);
            wrapper.set_conceal(breaks.clone(), breaks, vec![], cx);
            assert_eq!(wrapper.len(), 6);
            assert_eq!(wrapper.text().to_string(), text.to_string());
        });
    }

    #[gpui::test]
    fn hard_breaks_mix_with_measured_soft_wrap_and_fonts(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let source = "  甲乙丙丁<br><br>  戊己庚辛<br>";
            let text = Rope::from(source);
            let breaks = vec![14..18, 18..22, 36..40];
            let fonts = vec![(2..14, FontOverride { weight: Some(FontWeight::BOLD), ..Default::default() })];
            for indent in [WrappingIndent::Same, WrappingIndent::None] {
                let mut wrapper = TextWrapper::new(test_font(), px(14.), Some(px(25.)));
                wrapper.wrapping_indent = indent;
                wrapper.conceal = breaks.clone();
                wrapper.hard_breaks = breaks.clone();
                wrapper.fonts = fonts.clone();
                wrapper.update_all(&text, cx);

                // Each hard-break segment must wrap exactly like an independent source line.
                let mut expected = Vec::new();
                for range in [0..18, 18..22, 22..40, 40..40] {
                    let mut part = TextWrapper::new(test_font(), px(14.), Some(px(25.)));
                    part.wrapping_indent = indent;
                    part.conceal = breaks
                        .iter()
                        .filter(|b| b.start >= range.start && b.end <= range.end)
                        .map(|b| b.start - range.start..b.end - range.start)
                        .collect();
                    part.fonts = fonts
                        .iter()
                        .filter(|(r, _)| r.start >= range.start && r.end <= range.end)
                        .map(|(r, f)| (r.start - range.start..r.end - range.start, f.clone()))
                        .collect();
                    part.update_all(&Rope::from(text.slice(range.clone()).to_string()), cx);
                    expected.extend(
                        part.line(0)
                            .unwrap()
                            .wrapped_lines
                            .iter()
                            .map(|r| r.start + range.start..r.end + range.start),
                    );
                }
                assert!(expected.len() > 4, "test must exercise soft wrapping");
                assert_eq!(wrapper.line(0).unwrap().wrapped_lines.as_slice(), expected);
                assert_eq!(wrapper.text().to_string(), source);
            }
        });
    }

    #[gpui::test]
    fn hard_breaks_many_empty_rows_survive_width_and_source_updates(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let mut text = Rope::from(format!("{}甲", "<br>".repeat(256)));
            let breaks: Vec<_> = (0..256).map(|i| i * 4..i * 4 + 4).collect();
            let mut wrapper = TextWrapper::new(test_font(), px(14.), Some(px(1.)));
            wrapper.set_conceal(breaks.clone(), breaks.clone(), vec![], cx);
            wrapper.update_all(&text, cx);
            for indent in [WrappingIndent::Same, WrappingIndent::None] {
                wrapper.set_wrapping_indent(indent, cx);
                for width in [None, Some(px(1.)), Some(px(500.))] {
                    wrapper.set_wrap_width(width, cx);
                    assert_eq!(wrapper.len(), 257);
                    assert_eq!(wrapper.line(0).unwrap().wrapped_lines.last(), Some(&(1024..1027)));
                }
            }
            // The normal source edit API still takes original byte ranges.
            text.replace(1024..1027, "乙丙");
            wrapper.update(&text, &(1024..1027), &Rope::from("乙丙"), cx);
            assert_eq!(wrapper.len(), 257);
            assert_eq!(wrapper.line(0).unwrap().wrapped_lines.last(), Some(&(1024..1030)));
            assert_eq!(wrapper.text().to_string(), format!("{}乙丙", "<br>".repeat(256)));
        });
    }

    #[test]
    fn hard_breaks_ignore_duplicate_soft_boundaries() {
        for indent in [WrappingIndent::Same, WrappingIndent::None] {
            let text = Rope::from("ab<br><br>");
            let mut wrapper = TextWrapper::new(test_font(), px(14.), Some(px(10.)));
            wrapper.wrapping_indent = indent;
            wrapper.hard_breaks = vec![2..6, 6..10];
            wrapper._update(&text, &(0..text.len()), &text, &mut |line, _, _| {
                let mut boundaries = vec![Boundary { ix: 0, next_indent: 0 }];
                if line.starts_with("ab") {
                    boundaries
                        .extend([Boundary { ix: 1, next_indent: 0 }, Boundary { ix: 1, next_indent: 0 }]);
                }
                boundaries.push(Boundary { ix: line.len(), next_indent: 0 });
                boundaries
            });
            assert_eq!(wrapper.line(0).unwrap().wrapped_lines.as_slice(), [0..1, 1..6, 6..10, 10..10]);
        }
    }

    /// The longest-row summary stays exact when the previously-longest line is shrunk.
    #[test]
    fn test_longest_row_after_shrink() {
        let mut wrapper = TextWrapper::new(test_font(), px(14.), None);
        let mut text = Rope::from("aa\nthis is the longest line\nbb");
        wrapper._update(&text, &(0..text.len()), &text, &mut |_, _, _| vec![]);
        assert_eq!(wrapper.longest_row(), 1);

        // Shrink line 1 so line 2-equivalent isn't longest.
        // Make line 0 the longest now.
        let start = text.line_start_offset(0);
        let end = text.line_end_offset(0);
        let range = start..end;
        let new_text = "a very very long first line now";
        text.replace(range.clone(), new_text);
        wrapper._update(&text, &range, &Rope::from(new_text), &mut |_, _, _| vec![]);
        assert_eq!(wrapper.longest_row(), 0);
    }

    /// Editing the last line and deleting everything must keep the tree consistent.
    #[test]
    fn test_edit_last_line_and_full_delete() {
        let mut wrapper = TextWrapper::new(test_font(), px(14.), None);
        let mut text = Rope::from("one\ntwo\nthree");
        wrapper._update(&text, &(0..text.len()), &text, &mut |_, _, _| vec![]);
        assert_eq!(wrapper.lines_count(), 3);

        // Replace the last line only.
        let start = text.line_start_offset(2);
        let range = start..text.len();
        let new_text = "THREE EDITED";
        text.replace(range.clone(), new_text);
        wrapper._update(&text, &range, &Rope::from(new_text), &mut |_, _, _| vec![]);
        assert_eq!(wrapper.lines_count(), 3);
        assert_eq!(wrapper.line(2).unwrap().len(), "THREE EDITED".len());

        // Delete everything.
        let range = 0..text.len();
        text.replace(range.clone(), "");
        wrapper._update(&text, &range, &Rope::from(""), &mut |_, _, _| vec![]);
        assert_eq!(wrapper.lines_count(), 1);
        assert_eq!(wrapper.len(), 1);
        assert_eq!(wrapper.line(0).unwrap().wrapped_lines.as_slice(), [0..0]);
    }

    #[test]
    fn test_wrap_row_buffer_line_boundaries() {
        let mut wrapper = TextWrapper::new(test_font(), px(14.), None);
        wrapper.text = Rope::from("aa\nbbbb\nc");
        wrapper.lines = SumTree::from_iter(
            vec![
                LineItem {
                    len: 2,
                    indent: 0,
                    indent_segments: SmallVec::new(),
                    wrapped_lines: smallvec::smallvec![0..2],
                },
                LineItem {
                    len: 4,
                    indent: 0,
                    indent_segments: SmallVec::new(),
                    wrapped_lines: smallvec::smallvec![0..2, 2..4],
                },
                LineItem {
                    len: 1,
                    indent: 0,
                    indent_segments: SmallVec::new(),
                    wrapped_lines: smallvec::smallvec![0..1],
                },
            ],
            &(),
        );

        assert_eq!(wrapper.lines_count(), 3);
        assert_eq!(wrapper.len(), 4);

        assert_eq!(wrapper.buffer_line_to_first_wrap_row(0), 0);
        assert_eq!(wrapper.buffer_line_to_first_wrap_row(1), 1);
        assert_eq!(wrapper.buffer_line_to_first_wrap_row(2), 3);
        assert_eq!(wrapper.buffer_line_to_first_wrap_row(3), 4);

        assert_eq!(wrapper.buffer_line_to_wrap_row_range(0), 0..1);
        assert_eq!(wrapper.buffer_line_to_wrap_row_range(1), 1..3);
        assert_eq!(wrapper.buffer_line_to_wrap_row_range(2), 3..4);
        assert_eq!(wrapper.buffer_line_to_wrap_row_range(3), 4..4);

        assert_eq!(wrapper.wrap_row_to_buffer_line(0), 0);
        assert_eq!(wrapper.wrap_row_to_buffer_line(1), 1);
        assert_eq!(wrapper.wrap_row_to_buffer_line(2), 1);
        assert_eq!(wrapper.wrap_row_to_buffer_line(3), 2);
        assert_eq!(wrapper.wrap_row_to_buffer_line(4), 2);
    }

    #[test]
    fn test_wrap_row_queries_after_incremental_splice() {
        let mut wrapper = TextWrapper::new(test_font(), px(14.), Some(px(10.)));
        let mut text = Rope::from("aa\nbbbb\nc");
        let mut fake_wrap_line = |line: &str, _line_start: usize, _wrap_width: Pixels| {
            if line.len() > 2 { vec![Boundary { ix: 2, next_indent: 0 }] } else { vec![] }
        };

        wrapper._update(&text, &(0..text.len()), &text, &mut fake_wrap_line);
        assert_eq!(wrapper.buffer_line_to_wrap_row_range(0), 0..1);
        assert_eq!(wrapper.buffer_line_to_wrap_row_range(1), 1..3);
        assert_eq!(wrapper.buffer_line_to_wrap_row_range(2), 3..4);

        let range = text.line_start_offset(1)..text.line_end_offset(1);
        let new_text = "dd\neeee";
        text.replace(range.clone(), new_text);
        wrapper._update(&text, &range, &Rope::from(new_text), &mut fake_wrap_line);

        assert_eq!(wrapper.lines_count(), 4);
        assert_eq!(wrapper.len(), 5);
        assert_eq!(wrapper.buffer_line_to_wrap_row_range(0), 0..1);
        assert_eq!(wrapper.buffer_line_to_wrap_row_range(1), 1..2);
        assert_eq!(wrapper.buffer_line_to_wrap_row_range(2), 2..4);
        assert_eq!(wrapper.buffer_line_to_wrap_row_range(3), 4..5);
        assert_eq!(wrapper.wrap_row_to_buffer_line(0), 0);
        assert_eq!(wrapper.wrap_row_to_buffer_line(1), 1);
        assert_eq!(wrapper.wrap_row_to_buffer_line(2), 2);
        assert_eq!(wrapper.wrap_row_to_buffer_line(3), 2);
        assert_eq!(wrapper.wrap_row_to_buffer_line(4), 3);
    }

    #[test]
    fn test_line_layout() {
        let mut line_layout = LineLayout::new();

        let line1 = ShapedLine::default().with_len(100);
        let line2 = ShapedLine::default().with_len(50);
        let wrapped_lines = smallvec::smallvec![line1, line2];
        line_layout.set_wrapped_lines(wrapped_lines);
        assert_eq!(line_layout.len(), 150);
        assert_eq!(line_layout.wrapped_lines.len(), 2);
    }

    #[test]
    fn justified_glyph_positions_drive_hit_testing_and_keep_final_row() {
        fn row(text: &str) -> ShapedLine {
            use unicode_segmentation::UnicodeSegmentation;
            let glyphs: Vec<_> =
                text.grapheme_indices(true).enumerate().map(|(i, (at, _))| (at, i as f32 * 10.)).collect();
            let mut line = ShapedLine::default();
            line.text = text.to_owned().into();
            *std::ops::DerefMut::deref_mut(&mut line) =
                std::sync::Arc::new(glyph_layout(&[&glyphs], text.len(), glyphs.len() as f32 * 10.));
            line
        }
        let first = "中文 e\u{301} 👩‍💻 tail";
        let mut layout = LineLayout::new();
        layout.set_wrapped_lines(smallvec::smallvec![row(first), row("last")]);
        let last = layout.wrapped_lines[1].clone();
        layout.justify(px(300.));
        assert_eq!(layout.wrapped_lines[0].width, px(300.));
        assert_eq!(layout.wrapped_lines[1].width, last.width);
        let mut context = test_last_layout(px(20.));
        context.content_width = px(300.);
        use unicode_segmentation::UnicodeSegmentation;
        for (at, _) in first.grapheme_indices(true) {
            let position = layout.position_for_index(at, &context, false).unwrap();
            let hit = layout.closest_index_for_position(point(position.x, px(5.)), &context).unwrap().0;
            assert_eq!(hit, at, "glyph at {at}");
        }
    }

    /// A layout context whose only load-bearing field is the line height.
    fn test_last_layout(line_height: Pixels) -> LastLayout {
        LastLayout {
            visible_range: 0..1,
            visible_buffer_lines: vec![0],
            visible_line_byte_offsets: vec![0],
            visible_top: px(0.),
            visible_range_offset: 0..0,
            lines: Rc::new(vec![]),
            line_height,
            wrap_width: None,
            wrapping_indent: WrappingIndent::default(),
            line_number_width: px(0.),
            space_width: px(0.),
            cursor_bounds: None,
            text_align: TextAlign::Left,
            content_width: px(0.),
        }
    }

    #[test]
    fn test_position_for_index_prefers_first_leading_empty_visual_line() {
        let mut line_layout = LineLayout::new();
        line_layout.set_wrapped_lines(smallvec::smallvec![
            ShapedLine::default(),
            ShapedLine::default(),
            ShapedLine::default().with_len(3),
        ]);

        assert_eq!(
            line_layout.position_for_index(0, &test_last_layout(px(20.)), false),
            Some(point(px(0.), px(0.)))
        );
    }

    #[test]
    fn clicking_past_a_wrapped_row_keeps_the_caret_on_that_row() {
        // One buffer line wrapped into two visual rows, splitting at byte 10.
        let mut line_layout = LineLayout::new();
        line_layout.set_wrapped_lines(smallvec::smallvec![
            ShapedLine::default().with_len(10),
            ShapedLine::default().with_len(5),
        ]);
        let last_layout = test_last_layout(px(20.));

        // Clicking past the last glyph of the first row resolves to the wrap boundary, which is
        // also the first offset of the second row -- hence the affinity.
        let (ix, line_end_affinity) =
            line_layout.closest_index_for_position(point(px(999.), px(5.)), &last_layout).unwrap();
        assert_eq!(ix, 10);
        assert!(line_end_affinity);

        // Carrying that affinity is what keeps the caret on the row that was clicked; dropping it
        // is the bug -- the caret shows up one row below the pointer.
        assert_eq!(
            line_layout.position_for_index(ix, &last_layout, line_end_affinity).map(|pos| pos.y),
            Some(px(0.))
        );
        assert_eq!(line_layout.position_for_index(ix, &last_layout, false).map(|pos| pos.y), Some(px(20.)));

        // The final row owns the end of the line outright, so there is nothing to disambiguate.
        let (ix, line_end_affinity) =
            line_layout.closest_index_for_position(point(px(999.), px(25.)), &last_layout).unwrap();
        assert_eq!(ix, 15);
        assert!(!line_end_affinity);
    }

    #[test]
    fn a_wrap_boundary_offset_resolves_to_the_row_the_caret_is_drawn_on() {
        let mut wrapper = TextWrapper::new(test_font(), px(14.), None);
        wrapper.text = Rope::from("first line\nthis one wraps");
        wrapper.lines = SumTree::from_iter(
            vec![
                LineItem {
                    len: Rope::from("first line").len(),
                    indent: 0,
                    indent_segments: SmallVec::new(),
                    wrapped_lines: smallvec::smallvec![0..10],
                },
                LineItem {
                    len: Rope::from("this one wraps").len(),
                    indent: 0,
                    indent_segments: SmallVec::new(),
                    wrapped_lines: smallvec::smallvec![0..9, 9..14],
                },
            ],
            &(),
        );

        // Offset 20 is the wrap boundary of the second buffer line: 11 (line start) + 9.
        // It is the last offset of wrap row 1 and the first of wrap row 2 at the same time, so
        // only the affinity can say which row a vertical move should step away from.
        assert_eq!(wrapper.offset_to_display_point_with_affinity(20, true), WrapDisplayPoint::new(1, 0, 9));
        assert_eq!(wrapper.offset_to_display_point_with_affinity(20, false), WrapDisplayPoint::new(2, 1, 0));
        assert_eq!(
            wrapper.offset_to_display_point(20),
            wrapper.offset_to_display_point_with_affinity(20, false)
        );

        // An offset that is not on a boundary is unaffected either way.
        for line_end_affinity in [false, true] {
            assert_eq!(
                wrapper.offset_to_display_point_with_affinity(23, line_end_affinity),
                WrapDisplayPoint::new(2, 1, 3)
            );
        }
    }

    #[test]
    fn test_offset_to_display_point() {
        let font = gpui::Font {
            family: "Arial".into(),
            weight: FontWeight::default(),
            style: FontStyle::Normal,
            features: FontFeatures::default(),
            fallbacks: None,
        };

        let mut wrapper = TextWrapper::new(font, px(14.), None);
        wrapper.text =
            Rope::from("Hello, 世界!\r\nThis is second line.\nThis is third line.\n这里是第 4 行。");
        wrapper.lines = SumTree::from_iter(
            vec![
                // range: 0..15
                LineItem {
                    len: Rope::from("Hello, 世界!\r").len(),
                    indent: 0,
                    indent_segments: SmallVec::new(),
                    wrapped_lines: smallvec::smallvec![0..15],
                },
                // range: 16..36
                LineItem {
                    len: Rope::from("This is second line.\n").len(),
                    indent: 0,
                    indent_segments: SmallVec::new(),
                    wrapped_lines: smallvec::smallvec![0..10, 10..20],
                },
                // range: 37..56
                LineItem {
                    len: Rope::from("This is third line.\n").len(),
                    indent: 0,
                    indent_segments: SmallVec::new(),
                    wrapped_lines: smallvec::smallvec![0..9, 9..15, 15..20],
                },
                // range: 57..79
                LineItem {
                    len: Rope::from("这里是第 4 行。").len(),
                    indent: 0,
                    indent_segments: SmallVec::new(),
                    wrapped_lines: smallvec::smallvec![0..22],
                },
            ],
            &(),
        );

        assert_eq!(wrapper.offset_to_display_point(12), WrapDisplayPoint::new(0, 0, 12));
        assert_eq!(wrapper.offset_to_display_point(15), WrapDisplayPoint::new(0, 0, 15));

        assert_eq!(wrapper.offset_to_display_point(16), WrapDisplayPoint::new(1, 0, 0));
        assert_eq!(wrapper.offset_to_display_point(21), WrapDisplayPoint::new(1, 0, 5));
        assert_eq!(wrapper.offset_to_display_point(27), WrapDisplayPoint::new(2, 1, 1));
        assert_eq!(wrapper.offset_to_display_point(37), WrapDisplayPoint::new(3, 0, 0));
        assert_eq!(wrapper.offset_to_display_point(54), WrapDisplayPoint::new(5, 2, 2));
        assert_eq!(wrapper.offset_to_display_point(59), WrapDisplayPoint::new(6, 0, 2));

        assert_eq!(wrapper.display_point_to_offset(WrapDisplayPoint::new(6, 0, 2)), 59);
        assert_eq!(wrapper.display_point_to_offset(WrapDisplayPoint::new(5, 2, 2)), 54);
        assert_eq!(wrapper.display_point_to_offset(WrapDisplayPoint::new(3, 0, 0)), 37);
        assert_eq!(wrapper.display_point_to_offset(WrapDisplayPoint::new(2, 1, 1)), 27);
        assert_eq!(wrapper.display_point_to_offset(WrapDisplayPoint::new(1, 0, 5)), 21);
        assert_eq!(wrapper.display_point_to_offset(WrapDisplayPoint::new(1, 0, 0)), 16);
        assert_eq!(wrapper.display_point_to_offset(WrapDisplayPoint::new(0, 0, 15)), 15);
    }

    #[test]
    fn test_wrapping_indent_same_keeps_indent_reserved() {
        let mut wrapper = TextWrapper::new(test_font(), px(14.0), Some(px(10.)));
        wrapper.wrapping_indent = WrappingIndent::Same;
        let text = Rope::from("  abcdefghijklmnopqrstuv");
        let mut fake_wrap_line = |line: &str, _line_start: usize, _wrap_width: Pixels| {
            if line.starts_with(' ') {
                vec![Boundary { ix: 5, next_indent: 2 }]
            } else {
                let mut boundaries = vec![];
                let mut i = 8;
                while i < line.len() {
                    boundaries.push(Boundary { ix: i, next_indent: 0 });
                    i += 8;
                }
                boundaries
            }
        };

        wrapper._update(&text, &(0..text.len()), &text, &mut fake_wrap_line);

        let line = wrapper.line(0).unwrap();
        assert_eq!(line.indent, 2);
        assert_eq!(line.wrapped_lines.as_slice(), [0..5, 5..24]);
    }

    #[test]
    fn test_wrapping_indent_none_continuation_lines_wrapped_at_full_width() {
        let mut wrapper = TextWrapper::new(test_font(), px(14.0), Some(px(10.)));
        wrapper.wrapping_indent = WrappingIndent::None;
        let text = Rope::from("  abcdefghijklmnopqrstuv");
        let mut fake_wrap_line = |line: &str, _line_start: usize, _wrap_width: Pixels| {
            if line.starts_with(' ') {
                vec![Boundary { ix: 5, next_indent: 2 }]
            } else {
                let mut boundaries = vec![];
                let mut i = 8;
                while i < line.len() {
                    boundaries.push(Boundary { ix: i, next_indent: 0 });
                    i += 8;
                }
                boundaries
            }
        };

        wrapper._update(&text, &(0..text.len()), &text, &mut fake_wrap_line);

        let line = wrapper.line(0).unwrap();
        assert_eq!(line.indent, 0);
        assert_eq!(line.wrapped_lines.as_slice(), [0..5, 5..13, 13..21, 21..24]);
    }

    #[test]
    fn g10_row_alignment_reserves_continuation_indent_once() {
        let mut row = ShapedLine::default();
        *std::ops::DerefMut::deref_mut(&mut row) = std::sync::Arc::new(glyph_layout(&[&[(0, 0.)]], 1, 30.));
        let line = LineLayout::new().lines(smallvec::smallvec![row.clone(), row]).wrap_indent(px(20.));
        for (align, expected) in [(TextAlign::Center, px(45.)), (TextAlign::Right, px(70.))] {
            let mut context = test_last_layout(px(20.));
            context.content_width = px(100.);
            context.text_align = align;
            let position = line.position_for_index(1, &context, false).unwrap();
            assert_eq!(position, point(expected, px(20.)));
            assert_eq!(line.closest_index_for_position(position, &context), Some((1, false)));
        }
    }

    #[test]
    fn test_wrap_indent_offsets_continuation_lines() {
        let mut line_layout = LineLayout::new();
        line_layout.set_wrapped_lines(smallvec::smallvec![
            ShapedLine::default().with_len(5),
            ShapedLine::default().with_len(10),
        ]);

        line_layout = line_layout.wrap_indent(px(20.0));

        let last_layout = LastLayout {
            visible_range: 0..1,
            visible_buffer_lines: vec![0],
            visible_line_byte_offsets: vec![0],
            visible_top: px(0.),
            visible_range_offset: 0..0,
            lines: Rc::new(vec![]),
            line_height: px(20.0),
            wrap_width: Some(px(10.)),
            wrapping_indent: WrappingIndent::Same,
            line_number_width: px(0.),
            space_width: px(0.),
            cursor_bounds: None,
            text_align: TextAlign::Left,
            content_width: px(0.),
        };

        assert_eq!(line_layout.position_for_index(0, &last_layout, false), Some(point(px(0.), px(0.))),);

        assert_eq!(line_layout.position_for_index(6, &last_layout, false), Some(point(px(20.), px(20.))),)
    }
}
