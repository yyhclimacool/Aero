use gpui_kit::base::input::TextareaState;
use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use std::ops::Range;

use crate::render::State;

pub(super) struct InlineMath {
    pub range: Range<usize>,
    pub size: Size<Pixels>,
    latex: String,
    state: State,
}

impl super::Editor {
    pub(super) fn render_math_block(&self, source: &str, cx: &mut Context<Self>) -> AnyElement {
        let latex = formula(source);
        let Some(service) = self.images.clone() else { return math_source(source, cx) };
        if latex.is_empty() {
            return math_source(source, cx);
        }
        let em = super::BODY_FONT_SIZE;
        let (foreground, muted, danger) = (cx.theme().foreground, cx.theme().muted, cx.theme().danger);
        let scale = self.scale;
        let state = service.update(cx, |service, cx| service.math(&latex, true, em, scale, foreground, cx));
        match state {
            State::Ready { image, pixels, .. } => {
                let (width, height) = (pixels.width as f32 / scale, pixels.height as f32 / scale);
                div()
                    .id(SharedString::from(format!(
                        "math-scroll-{}",
                        aero_core::hash_bytes(latex.as_bytes())
                    )))
                    .debug_selector(|| "math-scroll".to_owned())
                    .w_full()
                    .overflow_x_scroll()
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .debug_selector(|| "math-block".to_owned())
                            .child(img(ImageSource::Render(image)).w(px(width)).h(px(height))),
                    )
                    .into_any_element()
            }
            State::Pending { pixels } => div()
                .debug_selector(|| "math-placeholder".to_owned())
                .w(px(pixels.map_or(120., |p| p.width as f32 / scale)))
                .h(px(pixels.map_or(em * 1.6, |p| p.height as f32 / scale)))
                .rounded(px(6.))
                .bg(muted)
                .into_any_element(),
            State::Failed(error) => v_flex()
                .w_full()
                .gap_1()
                .child(math_source(source, cx))
                .child(
                    div()
                        .debug_selector(|| "math-error".to_owned())
                        .text_xs()
                        .text_color(danger)
                        .child(error),
                )
                .into_any_element(),
        }
    }

    pub(super) fn sync_inline_math(
        &mut self,
        key: super::presenter::Key,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let p = &self.presenters[&key];
        let text = p.textarea.read(cx).value();
        let formulas = if p.extra.code { Vec::new() } else { aero_core::highlight::inline_formulas(&text) };
        let mut entries = Vec::new();
        if let Some(service) = self.images.clone() {
            let em = super::BODY_FONT_SIZE;
            let scale = self.scale;
            let foreground = cx.theme().foreground;
            for formula in formulas {
                let state = service
                    .update(cx, |service, cx| service.math(&formula.latex, false, em, scale, foreground, cx));
                let measured = match &state {
                    State::Ready { pixels, .. } | State::Pending { pixels: Some(pixels) } => {
                        size(px(pixels.width as f32 / scale), px(pixels.height as f32 / scale))
                    }
                    State::Pending { pixels: None } => {
                        size(px(em * formula.latex.len().max(1) as f32 * 0.6), px(em))
                    }
                    State::Failed(_) => {
                        let source = format!("${}$", formula.latex);
                        let mut font = window.text_style().font();
                        font.family = cx.theme().mono_font_family.clone();
                        let run =
                            TextRun { len: source.len(), font, color: foreground, ..Default::default() };
                        let line = window.text_system().shape_line(source.into(), px(13.), &[run], None);
                        size(line.width, px(super::BODY_FONT_SIZE * super::BODY_LINE_HEIGHT))
                    }
                };
                entries.push(InlineMath {
                    range: formula.range,
                    size: measured,
                    latex: formula.latex,
                    state,
                });
            }
        }
        let p = self.presenters.get_mut(&key).unwrap();
        if p.inline_math.iter().map(|m| (&m.range, m.size)).ne(entries.iter().map(|m| (&m.range, m.size))) {
            p.styled = None;
            p.natural_width = None;
        }
        p.inline_math = entries;
    }

    pub(super) fn inline_math_overlay(
        &self,
        key: super::presenter::Key,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = &self.presenters[&key];
        let textarea: Entity<TextareaState> = p.textarea.clone();
        let base = p.range.start;
        let editor = cx.weak_entity();
        let children: Vec<_> = p
            .inline_math
            .iter()
            .enumerate()
            .map(|(ix, formula)| {
                let range = base + formula.range.start..base + formula.range.end;
                let hit_range = range.clone();
                let child = div()
                    .id(SharedString::from(format!("math-inline-hit-{ix}")))
                    .w(formula.size.width)
                    .h(formula.size.height)
                    .cursor_text()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            window.prevent_default();
                            this.open_math_popup(hit_range.clone(), Some(key.id), window, cx);
                        }),
                    )
                    .child(inline_math_part(ix, formula, cx))
                    .into_any_element();
                (ix as u64, range, child)
            })
            .collect();
        canvas(
            move |_, window, cx| {
                let bounds = textarea.read(cx).inline_object_bounds().to_vec();
                let mut painted = Vec::new();
                for (id, range, mut child) in children {
                    if let Some((_, rect)) = bounds.iter().find(|(key, _)| *key == id) {
                        child.layout_as_root(
                            size(
                                AvailableSpace::Definite(rect.size.width),
                                AvailableSpace::Definite(rect.size.height),
                            ),
                            window,
                            cx,
                        );
                        child.prepaint_at(rect.origin, window, cx);
                        let _ =
                            editor.update(cx, |this, _| this.inline_math_hits.push((key.id, range, *rect)));
                        painted.push(child);
                    }
                }
                painted
            },
            |_, children, window, cx| {
                for mut child in children {
                    child.paint(window, cx);
                }
            },
        )
        .absolute()
        .size_full()
        .into_any_element()
    }
}

pub(super) fn inline_formula_containing(text: &str, cursor: usize) -> Option<Range<usize>> {
    aero_core::highlight::inline_formulas(text)
        .into_iter()
        .find(|formula| formula.range.start < cursor && cursor < formula.range.end)
        .map(|formula| formula.range)
}

fn formula(source: &str) -> String {
    let text = source.trim();
    let text = text.strip_prefix("$$").unwrap_or(text);
    let text = text.strip_suffix("$$").unwrap_or(text);
    text.trim().to_owned()
}

fn math_source(source: &str, cx: &App) -> AnyElement {
    div()
        .w_full()
        .debug_selector(|| "math-source".to_owned())
        .font_family(cx.theme().mono_font_family.clone())
        .text_size(px(13.))
        .text_color(cx.theme().muted_foreground)
        .child(source.trim().to_owned())
        .into_any_element()
}

fn inline_math_part(ix: usize, formula: &InlineMath, cx: &App) -> AnyElement {
    let el = div()
        .debug_selector(move || format!("math-inline-{ix}"))
        .w(formula.size.width)
        .h(formula.size.height);
    match &formula.state {
        State::Ready { image, .. } => el
            .child(img(ImageSource::Render(image.clone())).w(formula.size.width).h(formula.size.height))
            .into_any_element(),
        State::Pending { .. } => el.rounded(px(4.)).bg(cx.theme().muted).into_any_element(),
        State::Failed(_) => el
            .child(
                div()
                    .debug_selector(|| "math-inline-error".to_owned())
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(px(13.))
                    .text_color(cx.theme().danger)
                    .child(format!("${}$", formula.latex)),
            )
            .into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::formula;

    #[test]
    fn formula_strips_delimiters_and_whitespace() {
        assert_eq!(formula("$$\n\\int_0^1 x\\,dx\n$$"), "\\int_0^1 x\\,dx");
        assert_eq!(formula("$$a^2$$"), "a^2");
        assert_eq!(formula("  $$ x $$  "), "x");
        assert_eq!(formula("$$$$"), "");
        assert_eq!(formula("$$   $$"), "");
    }
}
