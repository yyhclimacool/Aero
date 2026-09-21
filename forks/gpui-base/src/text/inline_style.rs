use std::ops::Range;

use gpui::{HighlightStyle, SharedString, TextRun, TextStyle};

/// A highlight with the font information not represented by GPUI's HighlightStyle.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct InlineHighlight {
    pub(crate) style: HighlightStyle,
    pub(crate) font_family: Option<SharedString>,
    pub(crate) font_size_scale: Option<f32>,
}

impl InlineHighlight {
    fn highlight(mut self, other: &InlineHighlight) -> Self {
        self.style = self.style.highlight(other.style);
        if other.font_family.is_some() {
            self.font_family = other.font_family.clone();
        }
        if other.font_size_scale.is_some() {
            self.font_size_scale = other.font_size_scale;
        }
        self
    }
}

impl From<HighlightStyle> for InlineHighlight {
    fn from(style: HighlightStyle) -> Self {
        Self { style, font_family: None, font_size_scale: None }
    }
}

/// Keep the existing activation-order semantics of overlapping text marks.
pub(crate) fn combine_highlights(
    a: impl IntoIterator<Item = (Range<usize>, InlineHighlight)>,
    b: impl IntoIterator<Item = (Range<usize>, InlineHighlight)>,
) -> Vec<(Range<usize>, InlineHighlight)> {
    let mut endpoints = Vec::new();
    let mut highlights = Vec::new();
    for (range, highlight) in a.into_iter().chain(b) {
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
            let style =
                active.iter().fold(InlineHighlight::default(), |acc, id| acc.highlight(&highlights[*id]));
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

/// Resolve ordinary marks first, then apply the link layer to every split.
/// Empty boundary highlights do not participate in color blending, so a
/// translucent override is applied exactly once, not once per merge pass.
pub(crate) fn link_highlights(
    highlights: Vec<(Range<usize>, InlineHighlight)>,
    overrides: Vec<(Range<usize>, InlineHighlight)>,
) -> Vec<(Range<usize>, InlineHighlight)> {
    let base = combine_highlights(highlights, []);
    let boundaries = overrides.iter().map(|(range, _)| (range.clone(), InlineHighlight::default()));
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
        let mut run = default_style.clone().highlight(highlight.style).to_run(range.len());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_layer_does_not_depend_on_activation_order() {
        let original = gpui::rgb(0x123abc).into();
        let custom = gpui::rgb(0xab3456).into();
        let base = InlineHighlight::from(HighlightStyle {
            color: Some(original),
            underline: Some(gpui::UnderlineStyle { thickness: gpui::px(1.), ..Default::default() }),
            ..Default::default()
        });
        let overlay = InlineHighlight::from(HighlightStyle {
            color: Some(custom),
            underline: Some(gpui::UnderlineStyle { thickness: gpui::px(0.), ..Default::default() }),
            ..Default::default()
        });
        let highlights = link_highlights(vec![(0..4, base.clone()), (4..8, base)], vec![(0..8, overlay)]);
        let runs = text_runs(8, &TextStyle::default(), &highlights);
        assert!(runs.iter().all(|r| r.color == custom));
        assert!(runs.iter().all(|r| r.underline.unwrap().thickness == gpui::px(0.)));
    }
}
