//! 数学公式块（`ui_design.md` 5.6、方案 6.5）：`$$…$$` 渲染成位图，失败显示源码 + 错误。
//!
//! 走 9.9 的渲染服务，所以占位、两级缓存、失败回退与取消都和图片共用一套。
//! 公式在 RaTeX 排版阶段就按主题前景色着色，主题一换缓存键跟着变、自动重排。

use gpui_kit::base::text::{TextView, TextViewStyle};
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
            State::Ready { image, pixels } => {
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
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
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

    /// 段落里的 `$…$`。没有行内公式时返回 None，调用方继续用普通 Markdown 预览。
    pub(super) fn render_with_inline_math(
        &self,
        text: &str,
        style: TextViewStyle,
        view_id: SharedString,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let formulas = aero_core::highlight::inline_formulas(text);
        if formulas.is_empty() {
            return None;
        }
        let Some(service) = self.images.clone() else { return None };
        let em = super::BODY_FONT_SIZE;
        let foreground = cx.theme().foreground;
        let scale = self.scale;
        let mut parts: Vec<AnyElement> = Vec::new();
        let mut at = 0usize;
        for (ix, formula) in formulas.iter().enumerate() {
            if formula.range.start > at {
                parts.push(inline_text(
                    format!("{view_id}-t-{ix}").into(),
                    &text[at..formula.range.start],
                    style.clone(),
                ));
            }
            let state = service
                .update(cx, |service, cx| service.math(&formula.latex, false, em, scale, foreground, cx));
            parts.push(inline_math_part(ix, &formula.latex, state, scale, em, cx));
            at = formula.range.end;
        }
        if at < text.len() {
            parts.push(inline_text(format!("{view_id}-t-end").into(), &text[at..], style));
        }
        Some(div().w_full().flex().flex_wrap().items_center().gap_1().children(parts).into_any_element())
    }
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

fn inline_text(id: SharedString, text: &str, style: TextViewStyle) -> AnyElement {
    div()
        .flex_none()
        .child(
            TextView::markdown(id, aero_core::links::to_markdown(text))
                .style(style.with_paragraph_gap(rems(0.)))
                .selectable(false),
        )
        .into_any_element()
}

fn inline_math_part(ix: usize, latex: &str, state: State, scale: f32, em: f32, cx: &App) -> AnyElement {
    let selector = move || format!("math-inline-{ix}");
    match state {
        State::Ready { image, pixels } => {
            let (width, height) = (pixels.width as f32 / scale, pixels.height as f32 / scale);
            div()
                .flex_none()
                .debug_selector(selector)
                .child(img(ImageSource::Render(image)).w(px(width)).h(px(height)))
                .into_any_element()
        }
        State::Pending { pixels } => div()
            .flex_none()
            .debug_selector(selector)
            .w(px(pixels.map_or(em * latex.len().max(1) as f32 * 0.6, |p| p.width as f32 / scale)))
            .h(px(pixels.map_or(em, |p| p.height as f32 / scale)))
            .rounded(px(4.))
            .bg(cx.theme().muted)
            .into_any_element(),
        State::Failed(_) => div()
            .flex_none()
            .debug_selector(|| "math-inline-error".to_owned())
            .font_family(cx.theme().mono_font_family.clone())
            .text_size(px(13.))
            .text_color(cx.theme().danger)
            .child(format!("${latex}$"))
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
        // 只有定界符 / 空公式：交给源码显示，不去麻烦排版引擎
        assert_eq!(formula("$$$$"), "");
        assert_eq!(formula("$$   $$"), "");
    }
}
