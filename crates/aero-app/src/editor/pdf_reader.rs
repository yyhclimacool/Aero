use std::ops::Range;
use std::sync::Arc;

use aero_core::{BlockId, VaultPath};
use gpui_kit::base::Disableable as _;
use gpui_kit::base::input::{Enter, Escape, InputEvent, InputState};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::actions::{
    BlockDown, BlockUp, BlurEditor, DeleteSelectedBlocks, OpenPdf, PdfNextPage, PdfPreviousPage, SplitBlock,
};
use crate::render::{PdfInfo, PdfInfoState, RenderService, State};

const DEFAULT_HEIGHT: f32 = 480.;
const MIN_HEIGHT: f32 = 160.;
const MAX_HEIGHT: f32 = 1200.;
const PAGE_GAP: f32 = 12.;

#[derive(Debug, Default)]
struct PageLayout {
    tops: Vec<f32>,
    heights: Vec<f32>,
    total: f32,
}

impl PageLayout {
    fn new(pages: impl IntoIterator<Item = (f32, f32)>, width: f32) -> Self {
        let mut layout = Self::default();
        for (w, h) in pages {
            // Malformed dimensions must never poison GPUI layout with NaN/inf.
            let ratio = h / w;
            let height = if ratio.is_finite() && ratio > 0. { (width * ratio).max(1.) } else { width };
            if !layout.tops.is_empty() {
                layout.total += PAGE_GAP;
            }
            layout.tops.push(layout.total);
            layout.heights.push(height);
            layout.total += height;
        }
        layout
    }

    fn scroll_height(&self, viewport: f32) -> f32 {
        // A short last page still needs to be reachable at the viewport top.
        self.total.max(self.tops.last().copied().unwrap_or(0.) + viewport)
    }

    fn clamp_offset(&self, offset: f32, viewport: f32) -> f32 {
        if offset.is_nan() { 0. } else { offset.clamp(0., (self.scroll_height(viewport) - viewport).max(0.)) }
    }

    /// One-based; the top edge is authoritative, with gaps belonging to the preceding page.
    fn page_at(&self, offset: f32) -> u32 {
        self.tops.partition_point(|top| *top <= offset).max(1) as u32
    }

    fn visible_range(&self, offset: f32, viewport: f32) -> Range<usize> {
        if self.tops.is_empty() {
            return 0..0;
        }
        let first = self.page_at(offset) as usize - 1;
        let end = self.tops.partition_point(|top| *top < offset + viewport).max(first + 1);
        first.saturating_sub(1)..end.saturating_add(1).min(self.tops.len())
    }
}

fn viewport_height(height: f32) -> f32 {
    if height.is_nan() { DEFAULT_HEIGHT } else { height.clamp(MIN_HEIGHT, MAX_HEIGHT) }
}

fn parse_page(text: &str, total: u32) -> Result<u32, &'static str> {
    let text = text.trim();
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err("请输入有效的整数页码");
    }
    text.parse::<u32>().ok().filter(|page| *page >= 1 && *page <= total).ok_or("页码超出范围")
}

#[derive(Clone, Copy)]
struct ResizeDrag {
    top: bool,
    start_y: f32,
    start_height: f32,
}

impl ResizeDrag {
    fn height_at(self, y: f32) -> f32 {
        let delta = y - self.start_y;
        viewport_height(self.start_height + if self.top { -delta } else { delta })
    }
}

/// A root PDF block owns its navigation, input and viewport independently of Editor rendering.
/// `prop_page` is the last *external* page, never the page discovered by wheel scrolling.
/// No IO or PDF parsing happens in this entity.
pub(super) struct PdfReader {
    editor: WeakEntity<super::Editor>,
    block: BlockId,
    path: VaultPath,
    source: String,
    prop_page: u32,
    width: u32,
    actual_width: f32,
    page: u32,
    pending_page: Option<u32>,
    info: Option<Arc<PdfInfo>>,
    info_error: Option<SharedString>,
    layout: PageLayout,
    offset: f32,
    height: f32,
    drag: Option<ResizeDrag>,
    focus: FocusHandle,
    input: Option<Entity<InputState>>,
    editing: bool,
    reset_input: bool,
    input_error: Option<&'static str>,
    service: Entity<RenderService>,
    _subscriptions: Vec<Subscription>,
}

impl PdfReader {
    pub(super) fn new(
        editor: WeakEntity<super::Editor>,
        block: BlockId,
        path: VaultPath,
        source: String,
        page: u32,
        width: u32,
        service: Entity<RenderService>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&service, |_, _, cx| cx.notify());
        Self {
            editor,
            block,
            path,
            source,
            prop_page: page,
            width: width.clamp(24, 4096),
            actual_width: width.clamp(24, 680) as f32,
            page: page.max(1),
            pending_page: Some(page.max(1)),
            info: None,
            info_error: None,
            layout: PageLayout::default(),
            offset: 0.,
            height: DEFAULT_HEIGHT,
            drag: None,
            focus: cx.focus_handle(),
            input: None,
            editing: false,
            reset_input: false,
            input_error: None,
            service,
            _subscriptions: vec![subscription],
        }
    }

    pub(super) fn sync(
        &mut self,
        path: VaultPath,
        source: String,
        page: u32,
        width: u32,
        cx: &mut Context<Self>,
    ) {
        let path_changed = self.path != path;
        let navigation_changed = path_changed || self.source != source || self.prop_page != page;
        let width = width.clamp(24, 4096);
        let width_changed = self.width != width;
        if !navigation_changed && !width_changed {
            return;
        }
        self.path = path;
        self.source = source;
        self.prop_page = page;
        self.width = width;
        if path_changed {
            self.info = None;
            self.layout = PageLayout::default();
            self.info_error = None;
            self.offset = 0.;
        }
        if navigation_changed {
            self.page = page.max(1);
            self.pending_page = Some(self.page);
            self.input_error = None;
            self.reset_input = true;
        }
        cx.notify();
    }

    pub(super) fn current_page(&self) -> u32 {
        self.page
    }

    pub(super) fn page_count(&self) -> Option<u32> {
        self.info.as_ref().map(|info| info.pages.len() as u32)
    }

    /// Local navigation only. A metadata-pending jump is applied when dimensions arrive.
    pub(super) fn go_to_page(&mut self, page: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.page = page.max(1);
        self.pending_page = Some(self.page);
        self.apply_pending_page();
        self.input_error = None;
        self.sync_input(true, window, cx);
        cx.notify();
    }

    fn ensure_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.is_some() {
            return;
        }
        let value = self.page.to_string();
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("页码");
            state.set_value(value, window, cx);
            state
        });
        self._subscriptions.push(cx.subscribe_in(&input, window, Self::on_input));
        self.input = Some(input);
    }

    fn sync_input(&mut self, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = &self.input else { return };
        // Focus notifications may not run before this lazy child is rendered again.
        // The window's actual focus is authoritative, not the last delivered event.
        if !force && input.read(cx).focus_handle(cx).is_focused(window) {
            return;
        }
        let value = self.page.to_string();
        if input.read(cx).value().as_ref() != value {
            input.update(cx, |state, cx| state.set_value(value, window, cx));
        }
    }

    fn on_input(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Focus => {
                self.editing = true;
                let _ = self.editor.update(cx, |editor, cx| {
                    editor.exit_edit(cx);
                    editor.selection = None;
                    cx.notify();
                });
            }
            InputEvent::Blur => {
                self.editing = false;
                self.input_error = None;
                self.sync_input(true, window, cx);
            }
            InputEvent::Change => self.input_error = None,
            InputEvent::PressEnter { .. } => {
                let Some(total) = self.page_count() else {
                    self.input_error = Some("正在读取页数");
                    cx.notify();
                    return;
                };
                match parse_page(&input.read(cx).value(), total) {
                    Ok(page) => self.confirm_page(page, window, cx),
                    Err(error) => self.input_error = Some(error),
                }
            }
        }
        cx.notify();
    }

    fn confirm_page(&mut self, page: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(total) = self.page_count() else { return };
        if page == 0 || page > total {
            return;
        }
        // Reconfirming the visible page must not add undo, including after a wheel gesture.
        if page == self.page {
            self.go_to_page(page, window, cx);
            return;
        }
        let block = self.block;
        let committed = self
            .editor
            .update(cx, |editor, cx| editor.commit_pdf_page(block, page, total, window, cx))
            .unwrap_or(false);
        if committed {
            self.go_to_page(page, window, cx);
        }
    }

    fn apply_pending_page(&mut self) {
        let Some(total) = self.page_count().filter(|total| *total > 0) else { return };
        let Some(page) = self.pending_page.take() else { return };
        self.page = page.clamp(1, total);
        self.offset = self.layout.clamp_offset(self.layout.tops[self.page as usize - 1], self.height);
    }

    fn refresh_info(&mut self, cx: &mut Context<Self>) {
        match self.service.update(cx, |service, cx| service.pdf_info(self.path.clone(), cx)) {
            PdfInfoState::Ready(info) if !info.pages.is_empty() => {
                let changed = self.info.as_ref().is_none_or(|old| old.fingerprint != info.fingerprint);
                self.info_error = None;
                if changed {
                    self.layout =
                        PageLayout::new(info.pages.iter().map(|s| (s.width, s.height)), self.actual_width);
                    self.info = Some(info);
                    self.pending_page = Some(self.pending_page.unwrap_or(self.page));
                    self.input_error = None;
                }
                self.apply_pending_page();
            }
            state => {
                // Invalidation removes stale dimensions, errors and page images together.
                // Rendering below never asks for a bitmap while metadata is unavailable.
                if self.info.take().is_some() {
                    self.pending_page = Some(self.pending_page.unwrap_or(self.page));
                    self.layout = PageLayout::default();
                }
                self.info_error = match state {
                    PdfInfoState::Failed(error) => Some(error),
                    PdfInfoState::Ready(_) => Some("PDF 没有可显示的页面".into()),
                    PdfInfoState::Pending => None,
                };
            }
        }
    }

    fn measure_width(&mut self, width: f32, window: &mut Window, cx: &mut Context<Self>) {
        if !width.is_finite() || width < 1. || (width - self.actual_width).abs() < 0.5 {
            return;
        }
        let index = self.page.saturating_sub(1) as usize;
        let fraction = self
            .layout
            .tops
            .get(index)
            .map_or(0., |top| (self.offset - top).max(0.) / (self.layout.heights[index] + PAGE_GAP));
        self.actual_width = width;
        if let Some(info) = &self.info {
            self.layout = PageLayout::new(info.pages.iter().map(|s| (s.width, s.height)), width);
            self.offset = self.layout.clamp_offset(
                self.layout.tops[index] + fraction * (self.layout.heights[index] + PAGE_GAP),
                self.height,
            );
            self.apply_pending_page();
        }
        self.sync_input(false, window, cx);
        cx.notify();
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Deliberately handle scrolling ourselves: native edge chaining must not move the note.
        cx.stop_propagation();
        if self.info.is_none() {
            return;
        }
        let delta = event.delta.pixel_delta(px(24.));
        self.offset = self.layout.clamp_offset(self.offset - f32::from(delta.y), self.height);
        self.page = self.layout.page_at(self.offset);
        self.pending_page = None;
        self.sync_input(false, window, cx);
        cx.notify();
    }

    fn escape(&mut self, _: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(drag) = self.drag.take() {
            self.height = drag.start_height;
            self.offset = self.layout.clamp_offset(self.offset, self.height);
        } else if self.editing {
            self.editing = false;
            self.input_error = None;
            self.sync_input(true, window, cx);
            self.focus.focus(window, cx);
        } else {
            cx.propagate();
            return;
        }
        cx.notify();
    }

    fn resize_handle(&self, top: bool, cx: &mut Context<Self>) -> AnyElement {
        let id = if top { "pdf-resize-top" } else { "pdf-resize-bottom" };
        div()
            .id(id)
            .debug_selector(move || id.into())
            .w_full()
            .h(px(6.))
            .flex_none()
            .cursor(CursorStyle::ResizeRow)
            .hover(|style| style.bg(cx.theme().border))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.prevent_default();
                    this.focus.focus(window, cx);
                    this.drag =
                        Some(ResizeDrag { top, start_y: event.position.y.into(), start_height: this.height });
                    cx.notify();
                }),
            )
            .on_click(|_, _, cx| cx.stop_propagation())
            .into_any_element()
    }

    fn drag_resize(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let Some(drag) = self.drag else { return };
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
        } else {
            self.height = drag.height_at(event.position.y.into());
            self.offset = self.layout.clamp_offset(self.offset, self.height);
            self.page = self.layout.page_at(self.offset);
        }
        cx.notify();
    }

    fn finish_resize(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.drag = None;
        cx.notify();
    }

    fn resize_overlay(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.drag?;
        Some(
            deferred(
                anchored().position(point(px(0.), px(0.))).child(
                    div()
                        .id("pdf-resize-overlay")
                        .debug_selector(|| "pdf-resize-overlay".into())
                        .w(window.viewport_size().width)
                        .h(window.viewport_size().height)
                        .occlude()
                        .cursor(CursorStyle::ResizeRow)
                        .on_mouse_move(cx.listener(Self::drag_resize))
                        .on_mouse_up(MouseButton::Left, cx.listener(Self::finish_resize))
                        .on_mouse_up_out(MouseButton::Left, cx.listener(Self::finish_resize))
                        .on_scroll_wheel(|_, _, cx| cx.stop_propagation()),
                ),
            )
            .with_priority(20)
            .into_any_element(),
        )
    }

    fn error(&self, error: SharedString, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .w_full()
            .gap_1()
            .p_2()
            .child(div().debug_selector(|| "pdf-source".into()).text_xs().child(self.source.clone()))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .debug_selector(|| "pdf-error".into())
                            .text_xs()
                            .text_color(cx.theme().danger)
                            .child(error),
                    )
                    .child(div().debug_selector(|| "pdf-retry".into()).child(
                        Button::new("pdf-retry").ghost().xsmall().label("重试").on_click(cx.listener(
                            |this, _, _, cx| {
                                cx.stop_propagation();
                                this.info_error = None;
                                this.service.update(cx, |service, cx| service.retry(cx));
                                cx.notify();
                            },
                        )),
                    )),
            )
            .into_any_element()
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let page = self.page;
        let total = self.page_count();
        let mut bar = h_flex()
            .id("pdf-toolbar")
            .debug_selector(|| "pdf-toolbar".into())
            .w_full()
            .flex_none()
            .gap_2()
            .px_2()
            .py_1()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation());
        for (id, label, next, enabled) in [
            ("pdf-prev", "上一页", page.saturating_sub(1), total.is_some() && page > 1),
            ("pdf-next", "下一页", page.saturating_add(1), total.is_some_and(|total| page < total)),
        ] {
            bar = bar.child(div().debug_selector(move || id.into()).child(
                Button::new(id).ghost().xsmall().label(label).disabled(!enabled).on_click(cx.listener(
                    move |this, _, window, cx| {
                        cx.stop_propagation();
                        if enabled {
                            this.confirm_page(next, window, cx);
                        }
                    },
                )),
            ));
        }
        if let Some(input) = &self.input {
            bar = bar.child(
                div()
                    .id("pdf-page-input")
                    .debug_selector(|| "pdf-page-input".into())
                    .key_context("PdfPageInput")
                    .on_action(|_: &DeleteSelectedBlocks, _, cx| cx.stop_propagation())
                    .w(px(64.))
                    .flex_none()
                    // Input actions get first refusal; PDF shortcuts must never escape this wrapper.
                    .on_action(|_: &PdfPreviousPage, _, cx| cx.stop_propagation())
                    .on_action(|_: &PdfNextPage, _, cx| cx.stop_propagation())
                    .on_action(|_: &OpenPdf, _, cx| cx.stop_propagation())
                    .on_action(|_: &BlockUp, _, cx| cx.stop_propagation())
                    .on_action(|_: &BlockDown, _, cx| cx.stop_propagation())
                    // Aero binds Enter to SplitBlock in every Input context, before the
                    // input engine's Enter. Consume it here, never in the parent Editor.
                    .on_action(cx.listener(|this, _: &SplitBlock, window, cx| {
                        cx.stop_propagation();
                        if let Some(input) = this.input.clone() {
                            this.on_input(
                                &input,
                                &InputEvent::PressEnter { secondary: false, shift: false },
                                window,
                                cx,
                            );
                        }
                    }))
                    .on_action(|_: &Enter, _, cx| cx.stop_propagation())
                    .child(Input::new(input).xsmall()),
            );
        }
        bar.child(
            div()
                .debug_selector(move || format!("pdf-page-{page}-of-{}", total.unwrap_or(0)))
                .text_xs()
                .child(format!("/ {}", total.map_or("?".into(), |n| n.to_string()))),
        )
        .when_some(self.input_error, |bar, error| {
            bar.child(
                div()
                    .debug_selector(|| "pdf-page-input-error".into())
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(error),
            )
        })
        .into_any_element()
    }

    fn page_element(&self, index: usize, scale: f32, cx: &mut Context<Self>) -> AnyElement {
        let page = index as u32 + 1;
        let current = page == self.page;
        let state = self.service.update(cx, |service, cx| {
            service.pdf(self.path.clone(), page, self.actual_width.ceil().max(1.) as u32, scale, cx)
        });
        let content = match state {
            State::Ready { image, .. } => img(ImageSource::Render(image))
                .size_full()
                .when(current, |image| image.debug_selector(|| "pdf-image".into()))
                .into_any_element(),
            State::Pending { .. } => div()
                .size_full()
                .bg(cx.theme().muted)
                .debug_selector(|| "pdf-placeholder".into())
                .into_any_element(),
            State::Failed(error) => self.error(error, cx),
        };
        let block = self.block;
        div()
            .id(("pdf-page", page))
            .debug_selector(move || format!("pdf-page-{page}"))
            .w_full()
            .h(px(self.layout.heights[index]))
            .flex_none()
            .child(
                div()
                    .id("pdf-page-hit")
                    .size_full()
                    .when(current, |page| page.debug_selector(|| "pdf-block".into()))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        if event.is_right_click() {
                            return;
                        }
                        cx.stop_propagation();
                        let double = matches!(event, ClickEvent::Mouse(click) if click.down.click_count >= 2);
                        let path = this.path.clone();
                        let _ = this.editor.update(cx, |editor, cx| {
                            editor.select_block_by_id(block, window, cx);
                            if double {
                                editor.open_pdf_reader_source(&path, window, cx);
                            }
                        });
                    }))
                    .child(content),
            )
            .into_any_element()
    }

    fn pages(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        if let Some(error) = &self.info_error {
            return self.error(error.clone(), cx);
        }
        if self.info.is_none() {
            return div()
                .p_2()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .debug_selector(|| "pdf-placeholder".into())
                .child("正在读取 PDF…")
                .into_any_element();
        }
        let range = self.layout.visible_range(self.offset, self.height);
        let start = self.layout.tops.get(range.start).copied().unwrap_or(0.);
        let mut content = v_flex()
            .absolute()
            .top(px(-self.offset))
            .left_0()
            .w_full()
            .h(px(self.layout.scroll_height(self.height)))
            .child(div().h(px(start)).flex_none());
        for index in range.clone() {
            content = content.child(self.page_element(index, window.scale_factor(), cx));
            if index + 1 < self.layout.tops.len() {
                content = content.child(div().h(px(PAGE_GAP)).flex_none());
            }
        }
        let end = self.layout.tops.get(range.end).copied().unwrap_or(self.layout.total);
        content
            .child(div().h(px((self.layout.scroll_height(self.height) - end).max(0.))).flex_none())
            .into_any_element()
    }
}

impl Render for PdfReader {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_input(window, cx);
        let focused =
            self.input.as_ref().is_some_and(|input| input.read(cx).focus_handle(cx).is_focused(window));
        if self.editing && !focused {
            self.input_error = None;
        }
        self.editing = focused;
        self.refresh_info(cx);
        let force_input = std::mem::take(&mut self.reset_input);
        self.sync_input(force_input, window, cx);
        let reader = cx.weak_entity();
        let viewport = div()
            .w_full()
            .on_children_prepainted(move |bounds, window, cx| {
                if let Some(bounds) = bounds.first() {
                    let width = f32::from(bounds.size.width);
                    let _ = reader.update(cx, |this, cx| this.measure_width(width, window, cx));
                }
            })
            .child(
                div()
                    .id("pdf-viewport")
                    .debug_selector(|| "pdf-viewport".into())
                    .relative()
                    .w_full()
                    .h(px(self.height))
                    .flex_none()
                    .overflow_hidden()
                    .on_scroll_wheel(cx.listener(Self::scroll))
                    .child(self.pages(window, cx)),
            );
        v_flex()
            .id(("pdf-reader", self.block.0))
            .debug_selector(|| "pdf-reader".into())
            .track_focus(&self.focus)
            .w_full()
            .max_w(px(self.width as f32))
            .min_w_0()
            .on_action(cx.listener(Self::escape))
            .on_action(cx.listener(|this, _: &BlurEditor, window, cx| this.escape(&Escape, window, cx)))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(cx.listener(Self::scroll))
            .child(self.resize_handle(true, cx))
            .child(self.toolbar(cx))
            .child(viewport)
            .child(self.resize_handle(false, cx))
            .child({
                let reader = cx.weak_entity();
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _| {
                        let moving = reader.clone();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                            if phase.capture() {
                                let _ = moving.update(cx, |this, cx| {
                                    if this.drag.is_some() {
                                        this.drag_resize(event, window, cx);
                                    }
                                });
                            }
                        });
                        let finishing = reader.clone();
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                            if phase.capture() && event.button == MouseButton::Left {
                                let _ = finishing.update(cx, |this, cx| {
                                    if this.drag.is_some() {
                                        this.finish_resize(event, window, cx);
                                    }
                                });
                            }
                        });
                    },
                )
                .absolute()
                .size_full()
            })
            .children(self.resize_overlay(window, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::super::Editor;
    use super::{
        DEFAULT_HEIGHT, MAX_HEIGHT, MIN_HEIGHT, PageLayout, PdfReader, ResizeDrag, parse_page,
        viewport_height,
    };
    use crate::render::RenderService;
    use aero_core::{Vault, VaultPath};
    use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext};
    use std::sync::Arc;

    fn reader_fixture(
        cx: &mut TestAppContext,
    ) -> (tempfile::TempDir, tempfile::TempDir, Entity<PdfReader>, Entity<RenderService>) {
        cx.update(|cx| gpui_kit::init(cx));
        let dir = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let source = "![[paper.pdf#page=1|400]]\n";
        std::fs::write(dir.path().join("a.md"), source).unwrap();
        std::fs::write(dir.path().join("paper.pdf"), crate::render::two_page_pdf()).unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let note = VaultPath::new("a.md").unwrap();
        let (doc, stamp) = Editor::load(vault.clone(), note.clone()).unwrap();
        let block = doc.blocks().roots[0].id;
        let service = cx.new(|_| RenderService::new(vault.clone(), cache.path().to_path_buf()));
        let (editor, _) = cx.add_window_view(|_, cx| Editor::new(vault, note, doc, stamp, cx));
        let reader = cx.new(|cx| {
            PdfReader::new(
                editor.downgrade(),
                block,
                VaultPath::new("paper.pdf").unwrap(),
                source.into(),
                1,
                400,
                service.clone(),
                cx,
            )
        });
        (dir, cache, reader, service)
    }

    #[gpui_kit::test]
    fn pending_navigation_and_measured_width_preserve_local_page_across_prop_sync(cx: &mut TestAppContext) {
        let (_dir, _cache, reader, _service) = reader_fixture(cx);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| {
            reader.update(cx, |reader, cx| {
                reader.refresh_info(cx);
                assert_eq!(reader.page_count(), None);
                reader.go_to_page(2, window, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            reader.update(cx, |reader, cx| {
                reader.refresh_info(cx);
                assert_eq!(reader.page_count(), Some(2));
                assert_eq!(reader.current_page(), 2);
                assert_eq!(reader.offset, reader.layout.tops[1]);
                reader.sync(reader.path.clone(), reader.source.clone(), 1, 400, cx);
                assert_eq!(
                    reader.current_page(),
                    2,
                    "identical external props cannot reset local navigation"
                );
                reader.measure_width(200., window, cx);
                assert_eq!(reader.current_page(), 2);
                assert_eq!(reader.offset, reader.layout.tops[1]);
                let source = reader.source.clone();
                reader.sync(reader.path.clone(), source.replace("page=1", "page=2"), 2, 200, cx);
                reader.sync(reader.path.clone(), source, 1, 200, cx);
                reader.refresh_info(cx);
                assert_eq!(reader.current_page(), 1, "external undo must apply its page");
                assert_eq!(reader.offset, 0.);
            })
        });
    }

    #[gpui_kit::test]
    fn replacement_metadata_drops_stale_state_and_clamps_the_local_page(cx: &mut TestAppContext) {
        let (dir, _cache, reader, service) = reader_fixture(cx);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|_, cx| reader.update(cx, |reader, cx| reader.refresh_info(cx)));
        cx.run_until_parked();
        let fingerprint = cx.update(|window, cx| {
            reader.update(cx, |reader, cx| {
                reader.refresh_info(cx);
                reader.go_to_page(2, window, cx);
                reader.info.as_ref().unwrap().fingerprint
            })
        });
        let replacement = String::from_utf8(crate::render::two_page_pdf())
            .unwrap()
            .replace("/Kids [3 0 R 4 0 R] /Count 2", "/Kids [3 0 R      ] /Count 1");
        std::fs::write(dir.path().join("paper.pdf"), replacement).unwrap();
        cx.update(|_, cx| {
            service.update(cx, |service, cx| service.invalidate(&[VaultPath::new("paper.pdf").unwrap()], cx));
            reader.update(cx, |reader, cx| {
                reader.refresh_info(cx);
                assert_eq!(reader.page_count(), None);
                assert!(reader.layout.tops.is_empty(), "old pages must not survive invalidation");
            });
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            reader.update(cx, |reader, cx| {
                reader.refresh_info(cx);
                assert_eq!(reader.page_count(), Some(1));
                assert_eq!(reader.current_page(), 1);
                assert_ne!(reader.info.as_ref().unwrap().fingerprint, fingerprint);
                assert_eq!(reader.height, DEFAULT_HEIGHT);
                assert!(reader.info_error.is_none());
            })
        });
    }

    #[test]
    fn resize_edges_have_opposite_directions_and_share_limits() {
        let top = ResizeDrag { top: true, start_y: 200., start_height: DEFAULT_HEIGHT };
        let bottom = ResizeDrag { top: false, ..top };
        assert_eq!(top.height_at(100.), 580.);
        assert_eq!(bottom.height_at(300.), 580.);
        assert_eq!(top.height_at(5000.), MIN_HEIGHT);
        assert_eq!(bottom.height_at(5000.), MAX_HEIGHT);
        assert_eq!(top.height_at(-5000.), MAX_HEIGHT);
        assert_eq!(bottom.height_at(-5000.), MIN_HEIGHT);
    }

    #[test]
    fn mixed_page_sizes_use_actual_width_and_twelve_point_gaps() {
        let layout = PageLayout::new([(100., 200.), (200., 100.), (100., 100.)], 300.);
        assert_eq!(layout.heights, [600., 150., 300.]);
        assert_eq!(layout.tops, [0., 612., 774.]);
        assert_eq!(layout.total, 1074.);
    }

    #[test]
    fn only_visible_pages_and_one_neighbor_are_mounted() {
        let layout = PageLayout::new(std::iter::repeat_n((100., 100.), 1000), 200.);
        assert_eq!(layout.visible_range(0., 480.), 0..4);
        assert_eq!(layout.visible_range(2120., 480.), 9..14);
        assert_eq!(layout.visible_range(211_788., 480.), 998..1000);
    }

    #[test]
    fn current_page_uses_viewport_top_and_gap_belongs_to_previous_page() {
        let layout = PageLayout::new([(100., 100.); 3], 200.);
        assert_eq!(layout.page_at(199.), 1);
        assert_eq!(layout.page_at(211.), 1);
        assert_eq!(layout.page_at(212.), 2);
        assert_eq!(layout.page_at(f32::MAX), 3);
    }

    #[test]
    fn short_last_page_can_be_aligned_to_viewport_top() {
        let layout = PageLayout::new([(100., 100.); 3], 200.);
        assert_eq!(layout.scroll_height(480.), 904.);
        assert_eq!(layout.clamp_offset(-100., 480.), 0.);
        assert_eq!(layout.clamp_offset(f32::MAX, 480.), 424.);
        assert_eq!(layout.clamp_offset(f32::NAN, 480.), 0.);
        let short = PageLayout::new([(100., 20.)], 200.);
        assert_eq!(short.scroll_height(480.), 480.);
        assert_eq!(short.clamp_offset(100., 480.), 0.);
        assert_eq!(short.visible_range(0., 480.), 0..1);
    }

    #[test]
    fn viewport_height_is_independent_of_document_and_clamped() {
        assert_eq!(viewport_height(20.), MIN_HEIGHT);
        assert_eq!(viewport_height(650.), 650.);
        assert_eq!(viewport_height(2000.), MAX_HEIGHT);
        assert_eq!(viewport_height(f32::NAN), DEFAULT_HEIGHT);
        assert_eq!(PageLayout::default().visible_range(0., 480.), 0..0);
    }

    #[test]
    fn page_input_accepts_only_whole_pages_in_metadata_range() {
        for text in ["", " ", "0", "-1", "1.0", "NaN", "inf", "+1", "3", "4294967296"] {
            assert!(parse_page(text, 2).is_err(), "unexpected valid page: {text:?}");
        }
        assert_eq!(parse_page("1", 2), Ok(1));
        assert_eq!(parse_page(" 02 ", 2), Ok(2));
        assert!(parse_page("1", 0).is_err());
    }
}
