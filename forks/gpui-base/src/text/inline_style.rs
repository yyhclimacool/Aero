#[cfg(test)]
use std::cell::Cell;
use std::ops::Range;

use gpui::{HighlightStyle, SharedString, StyleRefinement, TextRun, TextStyle};

/// The style applied to one range of inline text.
///
/// A [`HighlightStyle`] carries no font family, so the family an inline code
/// span is set in rides beside it; `None` keeps the family of the enclosing
/// text style.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct InlineHighlight {
    pub(crate) style: HighlightStyle,
    pub(crate) font_family: Option<SharedString>,
    pub(crate) font_size_scale: Option<f32>,
    /// Typography and box refinements for an inline code span.
    pub(crate) code_style: Option<StyleRefinement>,
}

impl InlineHighlight {
    /// Layers `other` over `self`, the way [`HighlightStyle::highlight`] does.
    fn highlight(mut self, other: &InlineHighlight) -> Self {
        self.style = self.style.highlight(other.style);
        if other.font_family.is_some() {
            self.font_family = other.font_family.clone();
        }
        if other.font_size_scale.is_some() {
            self.font_size_scale = other.font_size_scale;
        }
        if other.code_style.is_some() {
            self.code_style = other.code_style.clone();
        }
        self
    }
}

impl From<HighlightStyle> for InlineHighlight {
    fn from(style: HighlightStyle) -> Self {
        Self {
            style,
            font_family: None,
            font_size_scale: None,
            code_style: None,
        }
    }
}

#[cfg(test)]
thread_local! {
    pub(crate) static COMBINED_HIGHLIGHT_INPUTS: Cell<usize> = const { Cell::new(0) };
}

/// Merges two highlight lists over one text into non-overlapping ranges,
/// cutting at every endpoint of every input range. Same sweep as
/// [`gpui::combine_highlights`], for [`InlineHighlight`] payloads.
pub(crate) fn combine_highlights(
    a: impl IntoIterator<Item = (Range<usize>, InlineHighlight)>,
    b: impl IntoIterator<Item = (Range<usize>, InlineHighlight)>,
) -> Vec<(Range<usize>, InlineHighlight)> {
    let mut endpoints = Vec::new();
    let mut highlights = Vec::new();
    for (range, highlight) in a.into_iter().chain(b) {
        #[cfg(test)]
        COMBINED_HIGHLIGHT_INPUTS.with(|inputs| inputs.set(inputs.get() + 1));
        if !range.is_empty() {
            let id = highlights.len();
            endpoints.push((range.start, id, true));
            endpoints.push((range.end, id, false));
            highlights.push(highlight);
        }
    }
    endpoints.sort_unstable_by_key(|(position, _, _)| *position);

    let mut combined = Vec::new();
    let mut active: Vec<usize> = Vec::new();
    let mut ix = 0;
    for (position, id, is_start) in endpoints {
        if position > ix && !active.is_empty() {
            let style = active.iter().fold(InlineHighlight::default(), |acc, id| {
                acc.highlight(&highlights[*id])
            });
            combined.push((ix..position, style));
        }
        ix = position;
        if is_start {
            active.push(id);
        } else {
            active.retain(|active_id| *active_id != id);
        }
    }
    combined
}

/// Builds the [`TextRun`]s for `text_len` bytes of inline text: each
/// highlight refines `default_style` over its range, and a highlight that
/// names a font family shapes its run in that family.
pub(crate) fn text_runs(
    text_len: usize,
    default_style: &TextStyle,
    highlights: &[(Range<usize>, InlineHighlight)],
) -> Vec<TextRun> {
    let mut runs = Vec::with_capacity(highlights.len() * 2 + 1);
    let mut ix = 0;
    for (range, highlight) in highlights {
        if ix < range.start {
            runs.push(default_style.clone().to_run(range.start - ix));
        }
        let mut style = default_style.clone().highlight(highlight.style);
        if let Some(code) = &highlight.code_style {
            // Font size is handled by the shaped fragment, not by a TextRun.
            style.font_features = code
                .text
                .font_features
                .clone()
                .unwrap_or(style.font_features);
            style.font_fallbacks = code.text.font_fallbacks.clone().or(style.font_fallbacks);
        }
        let mut run = style.to_run(range.len());
        if let Some(family) = &highlight.font_family {
            run.font.family = family.clone();
        }
        runs.push(run);
        ix = range.end;
    }
    if ix < text_len {
        runs.push(default_style.to_run(text_len - ix));
    }
    runs
}

/// Resolve ordinary marks first, then apply the link layer to every split.
/// Empty boundary highlights do not participate in color blending, so a
/// translucent override is applied exactly once, not once per merge pass.
pub(crate) fn link_highlights(
    highlights: Vec<(Range<usize>, InlineHighlight)>,
    overrides: Vec<(Range<usize>, InlineHighlight)>,
) -> Vec<(Range<usize>, InlineHighlight)> {
    let base = combine_highlights(highlights, []);
    let boundaries = overrides
        .iter()
        .map(|(range, _)| (range.clone(), InlineHighlight::default()));
    let mut combined = combine_highlights(base, boundaries);
    for (range, highlight) in &mut combined {
        for (link_range, overlay) in &overrides {
            if link_range.contains(&range.start) {
                *highlight = highlight.clone().highlight(overlay);
            }
        }
    }
    combined
}
