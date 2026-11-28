//! 数学公式块（`ui_design.md` 5.6、方案 6.5）：`$$…$$` 渲染成位图，失败显示源码 + 错误。
//!
//! 走 9.9 的渲染服务，所以占位、两级缓存、失败回退与取消都和图片共用一套。
//! 公式在 RaTeX 排版阶段就按主题前景色着色，主题一换缓存键跟着变、自动重排。

use gpui_kit::base::input::{InputInline, Markup};
use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::render::State;

impl super::Editor {
    /// `$$…$$` 块。`source` 是块的源码（含定界符）。
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
                // 比正文列宽的公式横向滚动，不缩小、也不把滚轮交给文档
                div()
                    .id(SharedString::from(format!(
                        "math-scroll-{}",
                        aero_core::hash_bytes(latex.as_bytes())
                    )))
                    .debug_selector(|| "math-scroll".to_owned())
                    .w_full()
                    .overflow_x_scroll()
                    .on_scroll(|_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .debug_selector(|| "math-block".to_owned())
                            .child(img(ImageSource::Render(image)).w(px(width)).h(px(height))),
                    )
                    .into_any_element()
            }
            // 首帧按字号给一条占位，渲染完成后替换
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

    /// 公式占用正文中的一个字形位置。过高的公式等比缩到当前行高。
    pub(super) fn inline_math(
        &self,
        text: &str,
        em: f32,
        foreground: Hsla,
        markup: &mut Markup,
        cx: &mut Context<Self>,
    ) -> (Vec<InputInline>, Vec<std::ops::Range<usize>>, bool) {
        let Some(service) = &self.images else { return (Vec::new(), Vec::new(), false) };
        let placeholder = cx.theme().muted;
        let scale = self.scale;
        let mut images = Vec::new();
        let mut ranges = Vec::new();
        let mut pending = false;
        for formula in aero_core::highlight::inline_formulas(text) {
            let state = service
                .update(cx, |service, cx| service.math(&formula.latex, false, em, scale, foreground, cx));
            ranges.push(formula.range.clone());
            let (pixels, image) = match state {
                State::Ready { image, pixels, .. } => (Some(pixels), Some(image)),
                State::Pending { pixels } => {
                    pending = true;
                    (pixels, None)
                }
                State::Failed(_) => {
                    markup.styles.push((
                        formula.range.clone(),
                        HighlightStyle { color: Some(cx.theme().danger), ..Default::default() },
                    ));
                    markup.fonts.push((formula.range, cx.theme().mono_font_family.clone()));
                    continue;
                }
            };
            let (width, height) = pixels
                .map_or((em * formula.latex.chars().count().max(1) as f32 * 0.6, em), |p| {
                    (p.width as f32 / scale, p.height as f32 / scale)
                });
            let ratio = (em / height).min(1.);
            images.push(InputInline::image(
                formula.range,
                size(px(width * ratio), px(height * ratio)),
                image,
                placeholder,
            ));
        }
        (images, ranges, pending)
    }
}

/// 光标严格落在 `$…$` 内部（不含两端定界符的外侧）时，返回这段源码区间。
pub(super) fn inline_formula_containing(text: &str, cursor: usize) -> Option<std::ops::Range<usize>> {
    aero_core::highlight::inline_formulas(text)
        .into_iter()
        .find(|formula| formula.range.start < cursor && cursor < formula.range.end)
        .map(|formula| formula.range)
}

/// 去掉 `$$` 定界符，留下公式本身。
fn formula(source: &str) -> String {
    let text = source.trim();
    let text = text.strip_prefix("$$").unwrap_or(text);
    let text = text.strip_suffix("$$").unwrap_or(text);
    text.trim().to_owned()
}

/// 渲染不了就显示源码：内容永不丢失，也不写回文件。
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

#[cfg(test)]
mod tests {
    use super::formula;

    #[test]
    fn formula_strips_delimiters_and_whitespace() {
        assert_eq!(formula("$$\n\\int_0^1 x\\,dx\n$$"), "\\int_0^1 x\\,dx");
        assert_eq!(formula("$$a^2$$"), "a^2");
        assert_eq!(formula("  $$ x $$  "), "x");
        // 只有定界符 / 空公式：交给源码显示，不去麻烦排版引擎
        assert_eq!(formula("$$$$"), "");
        assert_eq!(formula("$$   $$"), "");
    }
}
