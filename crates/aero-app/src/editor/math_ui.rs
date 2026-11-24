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
        base: usize,
        view: InlineMathView,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let formulas = aero_core::highlight::inline_formulas(text);
        if formulas.is_empty() {
            return None;
        }
        let service = self.images.as_ref()?;
        let block = view.block;
        let view_id = &view.id;
        let em = super::BODY_FONT_SIZE;
        let foreground = cx.theme().foreground;
        let scale = self.scale;
        let mut parts: Vec<AnyElement> = Vec::new();
        let mut at = 0usize;
        for (ix, formula) in formulas.iter().enumerate() {
            if formula.range.start > at {
                parts.push(self.inline_text_piece(
                    &text[at..formula.range.start],
                    base + at..base + formula.range.start,
                    format!("{view_id}-t-{ix}").into(),
                    &view,
                    cx,
                ));
            }
            let state = service
                .update(cx, |service, cx| service.math(&formula.latex, false, em, scale, foreground, cx));
            let range = base + formula.range.start..base + formula.range.end;
            let hit_range = range.clone();
            let record_range = range.clone();
            let editor = cx.weak_entity();
            parts.push(
                div()
                    .on_children_prepainted(move |bounds, _, cx| {
                        if let Some(bounds) = bounds.first().copied() {
                            let _ = editor.update(cx, |this, _| {
                                this.inline_math_hits.push((block, record_range.clone(), bounds));
                            });
                        }
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!("math-inline-hit-{ix}")))
                            .flex_none()
                            .cursor_text()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                                    cx.stop_propagation();
                                    this.open_math_popup(hit_range.clone(), Some(block), window, cx);
                                }),
                            )
                            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                                if event.is_right_click() {
                                    return;
                                }
                                cx.stop_propagation();
                                this.open_math_popup(range.clone(), Some(block), window, cx);
                            }))
                            .child(inline_math_part(ix, &formula.latex, state, scale, em, cx)),
                    )
                    .into_any_element(),
            );
            at = formula.range.end;
        }
        if at < text.len() {
            parts.push(self.inline_text_piece(
                &text[at..],
                base + at..base + text.len(),
                format!("{view_id}-t-end").into(),
                &view,
                cx,
            ));
        }
        Some(div().w_full().flex().flex_wrap().items_start().gap_1().children(parts).into_any_element())
    }

    /// 公式两侧的文字。正在编辑的那一段用文本框，其余保持预览，避免把 `$…$` 源码画出来。
    fn inline_text_piece(
        &self,
        text: &str,
        abs: std::ops::Range<usize>,
        view_id: SharedString,
        view: &InlineMathView,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if view.editing.as_ref() == Some(&abs)
            && let Some(focused) = &self.focused
        {
            // 文本框默认是宽高 100%。父级没有确定宽度时会变成 0。
            // 高度交给文本框自己按行数长，不要用字数去估，估矮了会把行尾的字切到下一行。
            return div()
                .w_full()
                .text_size(px(super::BODY_FONT_SIZE))
                .line_height(relative(super::BODY_LINE_HEIGHT))
                .debug_selector(|| "math-text".into())
                .child(gpui_kit::base::input::Textarea::new(&focused.textarea))
                .into_any_element();
        }
        let range = abs;
        let block = view.block;
        let wide = text.chars().count() as f32 * super::BODY_FONT_SIZE > super::COLUMN_WIDTH * 0.85;
        div()
            .id(view_id.clone())
            .when(wide, |el| el.w_full())
            .when(!wide, |el| el.flex_none())
            .debug_selector(|| "math-text".into())
            .cursor_text()
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.is_right_click() {
                    return;
                }
                cx.stop_propagation();
                let len = range.end.saturating_sub(range.start);
                this.focus_range(range.clone(), Some(block), false, len, window, cx);
            }))
            .child(inline_text(view_id, text, view.style.clone()))
            .into_any_element()
    }
}

pub(super) struct InlineMathView {
    pub(super) block: aero_core::BlockId,
    pub(super) style: TextViewStyle,
    pub(super) id: SharedString,
    /// 正在编辑的文字片段在整篇笔记中的 UTF-8 字节范围。
    pub(super) editing: Option<std::ops::Range<usize>>,
}

/// 光标所在的、公式以外的文字区间（相对 `text`）。光标在公式里时，取它前面最近的一段文字。
pub(super) fn text_span_for_cursor(text: &str, cursor: usize) -> Option<std::ops::Range<usize>> {
    let formulas = aero_core::highlight::inline_formulas(text);
    if formulas.is_empty() {
        return Some(0..text.len());
    }
    let mut spans = Vec::new();
    let mut at = 0usize;
    for formula in &formulas {
        spans.push(at..formula.range.start);
        at = formula.range.end;
    }
    spans.push(at..text.len());
    if let Some(span) =
        spans.iter().find(|span| span.start <= cursor && cursor < span.end && !span.is_empty())
    {
        return Some(span.clone());
    }
    spans.into_iter().rev().find(|span| !span.is_empty() && span.end <= cursor)
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

fn inline_text(id: SharedString, text: &str, style: TextViewStyle) -> AnyElement {
    let wide = text.chars().count() as f32 * super::BODY_FONT_SIZE > super::COLUMN_WIDTH * 0.85;
    div()
        .when(wide, |el| el.w_full())
        .when(!wide, |el| el.flex_none())
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
        State::Ready { image, pixels, .. } => {
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
