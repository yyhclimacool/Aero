use std::{cell::RefCell, ops::Range, rc::Rc};

use gpui_kit::base::input::TextareaState;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::prelude::*;
use gpui_kit::*;

pub(super) fn color_hex(cx: &App) -> &'static str {
    if cx.theme().mode.is_dark() { "#66551c" } else { "#fef08a" }
}

pub(super) fn color(cx: &App) -> Hsla {
    if cx.theme().mode.is_dark() { rgb(0x66551c).into() } else { rgb(0xfef08a).into() }
}

#[derive(Default)]
struct Cache {
    source: String,
    ranges: Vec<Range<usize>>,
    #[cfg(test)]
    painted: Vec<Bounds<Pixels>>,
}

#[derive(Clone, Default)]
pub(super) struct SourceHighlight(Rc<RefCell<Cache>>);

impl SourceHighlight {
    #[cfg(test)]
    pub fn painted(&self) -> Vec<Bounds<Pixels>> {
        self.0.borrow().painted.clone()
    }
}

/// Textarea 没有行内装饰 API。背景在其文字之前绘制，位置读取当前帧排版，保持原生选区和 IME。
pub(super) fn background(input: Entity<TextareaState>, cache: SourceHighlight) -> AnyElement {
    canvas(
        |_, _, _| (),
        move |_, _, window, cx| {
            let input = input.read(cx);
            let source = input.value().to_string();
            let mut cache = cache.0.borrow_mut();
            if source != cache.source {
                cache.ranges = aero_core::highlight::ranges(&source);
                cache.source = source;
            }
            let rectangles = rectangles(input, &cache.source, &cache.ranges);
            #[cfg(test)]
            {
                cache.painted = rectangles.clone();
            }
            let Some(bounds) = input.text_bounds() else { return };
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                for bounds in rectangles {
                    window.paint_quad(fill(bounds, color(cx)));
                }
            });
        },
    )
    .absolute()
    .size_full()
    .into_any_element()
}

fn rectangles(input: &TextareaState, source: &str, ranges: &[Range<usize>]) -> Vec<Bounds<Pixels>> {
    let Some(text_bounds) = input.text_bounds() else { return Vec::new() };
    let mut out: Vec<Bounds<Pixels>> = Vec::new();
    for range in ranges {
        let Some(text) = source.get(range.clone()) else { continue };
        for (offset, ch) in text.char_indices() {
            if matches!(ch, '\n' | '\r') {
                continue;
            }
            let at = range.start + offset;
            let end = at + ch.len_utf8();
            let Some(start) = input.range_to_bounds(&(at..at)) else { continue };
            let Some(next) = input.range_to_bounds(&(end..end)) else { continue };
            let right = if start.top() == next.top() { next.left() } else { text_bounds.right() };
            if right <= start.left() {
                continue;
            }
            let bounds = Bounds::from_corners(start.origin, point(right, start.bottom()));
            if let Some(last) = out.last_mut()
                && last.top() == bounds.top()
                && last.right() == bounds.left()
            {
                last.size.width += bounds.size.width;
            } else {
                out.push(bounds);
            }
        }
    }
    out
}
