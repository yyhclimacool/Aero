use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Instant;

use gpui_kit::StatefulInteractiveElement as _;

use super::*;

const MAX_MARKS: usize = 80;
const PREVIEW_CHARS: usize = 280;
/// 刻度放大：偏快、几乎不回弹，大约 200 ms 落定。减弱动态效果时直接跳到终点。
const MARK_SPRING: SpringConfig = SpringConfig::new(420., 34., 1.);
/// 久未重绘再悬停时，不要把空闲的那几秒一次积进弹簧，否则刻度会跳到终点。
const MAX_LENS_STEP: f32 = 0.05;

/// 指针下的刻度放到最大，左右两根依次变小，再远的不动。
fn mark_lens(distance: Option<usize>) -> f32 {
    match distance {
        Some(0) => 1.,
        Some(1) => 0.45,
        Some(2) => 0.16,
        _ => 0.,
    }
}

fn magnified_width(rest: f32) -> f32 {
    (rest + 14.).min(30.)
}

#[derive(Default)]
pub(super) struct Navigation {
    key: Option<(u64, FileStamp)>,
    hovered: Option<BlockId>,
    task: Option<Task<()>>,
    preview: Option<NavigationPreview>,
    motion: RefCell<HashMap<u64, MarkMotion>>,
    /// 固定后刻度展开成标题列表；换文档时保留，不跟缩略图的悬停一起清掉。
    pinned: bool,
}

struct MarkMotion {
    position: f32,
    velocity: f32,
    at: Instant,
}

struct NavigationPreview {
    title: String,
    body: String,
    anchor: Bounds<Pixels>,
}

fn marker_indices(units: &[Unit], limit: usize) -> Vec<usize> {
    let candidates: Vec<_> = units
        .iter()
        .enumerate()
        .filter_map(|(ix, unit)| {
            (!unit.range.is_empty()
                && !matches!(
                    unit.kind,
                    BlockKind::FrontMatter | BlockKind::Marker | BlockKind::ThematicBreak
                ))
            .then_some(ix)
        })
        .collect();
    let limit = limit.max(2);
    if candidates.len() <= limit {
        return candidates;
    }
    (0..limit).map(|i| candidates[i * (candidates.len() - 1) / (limit - 1)]).collect()
}

fn step_lens(
    motion: &mut HashMap<u64, MarkMotion>,
    id: u64,
    target: f32,
    now: Instant,
    snap: bool,
) -> (f32, bool) {
    let state = motion.entry(id).or_insert(MarkMotion { position: target, velocity: 0., at: now });
    if snap {
        *state = MarkMotion { position: target, velocity: 0., at: now };
        return (target, false);
    }
    let elapsed = now.saturating_duration_since(state.at).as_secs_f32().min(MAX_LENS_STEP);
    if elapsed > 0. {
        let next = MARK_SPRING.step(
            SpringState { position: state.position, velocity: state.velocity },
            target,
            elapsed,
        );
        state.position = next.position;
        state.velocity = next.velocity;
    }
    state.at = now;
    let settled = MARK_SPRING.is_settled(
        SpringState { position: state.position, velocity: state.velocity },
        target,
        0.02,
    );
    if settled {
        state.position = target;
        state.velocity = 0.;
    }
    (state.position, !settled)
}

fn navigation_mark_bar(
    id: u64,
    rest_width: f32,
    t: f32,
    highlighted: bool,
    step: f32,
    muted: Hsla,
    foreground: Hsla,
) -> impl IntoElement {
    let grown = magnified_width(rest_width);
    let rest_height = if highlighted { 3. } else { 2. };
    let rest_color = if highlighted { foreground } else { muted };
    let t = t.clamp(0., 1.2);
    let width = f32::interpolate(rest_width, grown, t);
    let height = f32::interpolate(rest_height, rest_height + 3., t);
    div()
        .absolute()
        .right(px(0.))
        .top(px((step - height) / 2.))
        .w(px(width))
        .h(px(height))
        .rounded_full()
        .bg(Hsla::interpolate(rest_color, foreground, t.min(1.)))
        .debug_selector(move || format!("navigation-bar-{id}"))
}

fn preview_text(doc: &Document, unit: &Unit, chars: usize) -> String {
    let raw: String = doc.slice(unit.range.clone()).chars().take(1024).collect();
    let plain = aero_core::index::plain_text(&raw, &unit.kind);
    let flat = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut text: String = flat.chars().take(chars).collect();
    if flat.chars().count() > chars || raw.len() < unit.range.len() {
        text.push('…');
    }
    text
}

impl Editor {
    pub(super) fn sync_navigation(&mut self) {
        let key = (self.doc.version(), self.stamp);
        if self.navigation.key != Some(key) {
            let pinned = self.navigation.pinned;
            self.navigation = Navigation { key: Some(key), pinned, ..Default::default() };
        }
    }

    pub(super) fn navigation_pinned(&self) -> bool {
        self.navigation.pinned
    }

    pub(crate) fn show_navigation_outline(&mut self, cx: &mut Context<Self>) {
        if self.navigation.pinned {
            return;
        }
        self.set_navigation_pinned(true, cx);
    }

    fn set_navigation_pinned(&mut self, pinned: bool, cx: &mut Context<Self>) {
        self.navigation.pinned = pinned;
        self.navigation.task = None;
        self.navigation.hovered = None;
        self.navigation.preview = None;
        cx.notify();
    }

    pub(super) fn clear_navigation(&mut self, cx: &mut Context<Self>) {
        self.navigation.task = None;
        let changed = self.navigation.hovered.take().is_some() | self.navigation.preview.take().is_some();
        if changed {
            cx.notify();
        }
    }

    fn hover_navigation(
        &mut self,
        id: BlockId,
        hovered: bool,
        anchor: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self.navigation.pinned {
            return;
        }
        if !hovered {
            if self.navigation.hovered == Some(id) {
                self.clear_navigation(cx);
            }
            return;
        }
        if self.navigation.hovered == Some(id) {
            return;
        }
        self.clear_navigation(cx);
        self.navigation.hovered = Some(id);
        self.navigation.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(150)).await;
            let _ = this.update(cx, |this, cx| {
                if this.navigation.hovered != Some(id) {
                    return;
                }
                let Some(ix) = this.unit_index(id) else {
                    return;
                };
                let unit = &this.units[ix];
                let heading =
                    this.units[..=ix].iter().rev().find(|u| matches!(u.kind, BlockKind::Heading { .. }));
                let title = heading
                    .map(|u| preview_text(&this.doc, u, 70))
                    .unwrap_or_else(|| format!("第 {} 段", ix + 1));
                let body_unit = if matches!(unit.kind, BlockKind::Heading { .. }) {
                    this.units
                        .get(ix + 1)
                        .filter(|next| !matches!(next.kind, BlockKind::Heading { .. }))
                        .unwrap_or(unit)
                } else {
                    unit
                };
                let body = preview_text(&this.doc, body_unit, PREVIEW_CHARS);
                this.navigation.preview = Some(NavigationPreview { title, body, anchor });
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn render_navigation(&self, window: &Window, cx: &Context<Self>) -> Option<AnyElement> {
        let available = (window.viewport_size().height * 0.7).max(px(20.));
        let limit = ((available.as_f32() / 8.) as usize).clamp(2, MAX_MARKS);
        let indices = marker_indices(&self.units, limit);
        if indices.is_empty() {
            return None;
        }
        let step = (available / indices.len() as f32).min(px(14.));
        let muted = cx.theme().muted_foreground.opacity(0.4);
        let foreground = cx.theme().foreground;
        let hovered_at = self
            .navigation
            .hovered
            .and_then(|id| indices.iter().position(|&unit_ix| self.units[unit_ix].id == id));
        let highlight = self.highlight;
        let specs: Vec<(BlockId, f32, f32, bool)> = indices
            .iter()
            .enumerate()
            .map(|(mark_ix, &ix)| {
                let unit = &self.units[ix];
                let width = match unit.kind {
                    BlockKind::Heading { level: 1 } => 22.,
                    BlockKind::Heading { level: 2 } => 18.,
                    BlockKind::Heading { .. } => 14.,
                    _ => 8.,
                };
                (
                    unit.id,
                    width,
                    mark_lens(hovered_at.map(|at| at.abs_diff(mark_ix))),
                    highlight == Some(unit.id),
                )
            })
            .collect();
        let now = cx.background_executor().now();
        let snap = cx.reduce_motion();
        let mut motion = self.navigation.motion.borrow_mut();
        let mut moving = false;
        let mut live = Vec::with_capacity(specs.len());
        let step_px = step.as_f32();
        let marks: Vec<_> = specs
            .into_iter()
            .map(|(id, width, amount, highlighted)| {
                let (t, still) = step_lens(&mut motion, id.0, amount, now, snap);
                moving |= still;
                live.push(id.0);
                let anchor = Rc::new(Cell::new(Bounds::default()));
                let bounds = anchor.clone();
                div()
                    .on_children_prepainted(move |children, _, _| {
                        if let Some(bounds) = children.first() {
                            anchor.set(*bounds);
                        }
                    })
                    .id(("navigation-mark", id.0))
                    .debug_selector(move || format!("navigation-mark-{}", id.0))
                    .relative()
                    .w(px(24.))
                    .h(step)
                    .flex_none()
                    .cursor_pointer()
                    .child(navigation_mark_bar(id.0, width, t, highlighted, step_px, muted, foreground))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_hover(cx.listener(move |this, hovered, _, cx| {
                        this.hover_navigation(id, *hovered, bounds.get(), cx)
                    }))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.jump_from_navigation(id, window, cx)),
                    )
                    .into_any_element()
            })
            .collect();
        motion.retain(|id, _| live.contains(id));
        drop(motion);
        if moving {
            window.request_animation_frame();
        }
        Some(
            // 铺满编辑区但不接鼠标：只有刻度和「固定」自己接收点击。
            div()
                .absolute()
                .inset_0()
                .child(
                    div()
                        .id("document-navigator")
                        .debug_selector(|| "document-navigator".into())
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .right(px(12.))
                        .w(px(24.))
                        .flex()
                        .items_center()
                        .child(v_flex().w_full().children(marks)),
                )
                .child(self.navigation_pin(false, cx))
                .into_any_element(),
        )
    }

    fn jump_from_navigation(&mut self, id: BlockId, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_navigation(cx);
        self.exit_edit(cx);
        self.selection = None;
        self.focus_handle.focus(window, cx);
        if self.unit_index(id).is_some() {
            self.reveal_block(id, cx);
        }
    }

    fn navigation_pin(&self, pinned: bool, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, background, muted, foreground, hover) =
            (theme.border, theme.background, theme.muted_foreground, theme.foreground, theme.list_hover);
        div()
            .id("navigation-pin")
            .debug_selector(|| "navigation-pin".into())
            .when(!pinned, |el| el.absolute().top(px(8.)).right(px(8.)))
            .px_1()
            .py(px(2.))
            .rounded_md()
            .border_1()
            .border_color(border)
            .bg(background)
            .text_xs()
            .text_color(muted)
            .cursor_pointer()
            .hover(move |style| style.bg(hover).text_color(foreground))
            .child(if pinned { "缩略图" } else { "固定" })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.set_navigation_pinned(!pinned, cx);
            }))
            .into_any_element()
    }

    pub(super) fn pinned_outline(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (foreground, muted, hover, active_bg) =
            (theme.foreground, theme.muted_foreground, theme.list_hover, theme.list_active);
        let headings = self.headings();
        let empty = headings.is_empty();
        let highlight = self.highlight;
        let rows = headings.into_iter().map(move |(level, text, id)| {
            let active = highlight == Some(id);
            let label = if text.is_empty() { "（空标题）".to_owned() } else { text };
            let color = if active || level < 3 { foreground } else { muted };
            div()
                .id(("navigation-heading", id.0))
                .debug_selector(move || format!("navigation-heading-{}", id.0))
                .w_full()
                .pl(px(8. + (level.saturating_sub(1)) as f32 * 12.))
                .pr_2()
                .py_1()
                .rounded_md()
                .text_sm()
                .truncate()
                .cursor_pointer()
                .text_color(color)
                .when(level == 1, |el| el.font_weight(FontWeight::MEDIUM))
                .when(active, |el| el.bg(active_bg))
                .hover(move |style| style.bg(hover))
                .child(label)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.jump_from_navigation(id, window, cx);
                }))
                .into_any_element()
        });
        v_flex()
            .id("navigation-outline")
            .debug_selector(|| "navigation-outline".into())
            .flex_none()
            .h_full()
            .w(px(220.))
            .bg(theme.sidebar)
            .border_l_1()
            .border_color(theme.sidebar_border)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                h_flex()
                    .flex_none()
                    .px_2()
                    .h(px(36.))
                    .items_center()
                    .justify_between()
                    .child(div().text_xs().text_color(theme.muted_foreground).child("大纲"))
                    .child(self.navigation_pin(true, cx)),
            )
            .child(
                v_flex()
                    .id("navigation-outline-rows")
                    .flex_1()
                    .min_h(px(0.))
                    .px_1()
                    .pb_2()
                    .overflow_y_scroll()
                    .children(rows)
                    .when(empty, |el| {
                        el.child(div().px_2().py_2().text_xs().text_color(muted).child("这篇还没有标题。"))
                    }),
            )
            .into_any_element()
    }

    pub(super) fn render_navigation_preview(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let preview = self.navigation.preview.as_ref()?;
        let viewport = window.viewport_size();
        let width = px(320.).min((viewport.width - px(32.)).max(px(100.)));
        let top =
            (preview.anchor.center().y - px(60.)).clamp(px(8.), (viewport.height - px(208.)).max(px(8.)));
        let right = (preview.anchor.right() - px(32.)).max(width + px(8.));
        Some(
            deferred(
                anchored()
                    .anchor(Anchor::TopRight)
                    .position(point(right, top))
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        v_flex()
                            .id("navigation-preview")
                            .debug_selector(|| "navigation-preview".into())
                            .occlude()
                            .w(width)
                            .max_h(px(200.))
                            .p_3()
                            .gap_2()
                            .rounded(px(8.))
                            .bg(cx.theme().popover)
                            .text_color(cx.theme().popover_foreground)
                            .border_1()
                            .border_color(cx.theme().border)
                            .shadow_lg()
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.clear_navigation(cx)))
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(preview.title.clone()),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .line_height(relative(1.5))
                                    .max_h(px(130.))
                                    .overflow_hidden()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(preview.body.clone()),
                            ),
                    ),
            )
            .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{magnified_width, mark_lens, marker_indices, preview_text};
    use crate::editor::units::units;
    use aero_core::Document;

    #[test]
    fn dense_documents_keep_bounded_markers_and_both_ends() {
        let doc = Document::from_text(&"paragraph\n\n".repeat(1000));
        let units = units(&doc);
        let marks = marker_indices(&units, 80);
        assert_eq!(marks.len(), 80);
        assert_eq!(marks[0], 0);
        assert_eq!(*marks.last().unwrap(), units.len() - 1);
        assert!(marks.windows(2).all(|p| p[0] < p[1]));
    }

    #[test]
    fn previews_are_bounded_visible_text_not_markdown_syntax() {
        let doc =
            Document::from_text(&format!("**中文** [文字](https://example.com) {}", "内容".repeat(5000)));
        let units = units(&doc);
        let text = preview_text(&doc, &units[0], 280);
        assert!(text.starts_with("中文 文字 "));
        assert!(text.ends_with('…'));
        assert!(text.chars().count() <= 281);
        assert!(!text.contains("**") && !text.contains("https://"));
    }

    #[test]
    fn lens_peaks_under_the_pointer_and_widths_stay_bounded() {
        assert_eq!(mark_lens(Some(0)), 1.);
        assert!(mark_lens(Some(1)) > mark_lens(Some(2)) && mark_lens(Some(2)) > 0.);
        assert_eq!(mark_lens(Some(3)), 0.);
        assert_eq!(mark_lens(None), 0.);
        assert!(magnified_width(8.) > 8. && magnified_width(8.) < magnified_width(22.));
        assert!(magnified_width(22.) <= 30.);
    }

    #[test]
    fn metadata_and_separators_do_not_create_navigation_marks() {
        let doc = Document::from_text("---\ntitle: Name\n---\n\n---\n\nbody\n");
        let units = units(&doc);
        let marks = marker_indices(&units, 80);
        assert_eq!(marks.len(), 1);
        assert_eq!(doc.slice(units[marks[0]].range.clone()).to_string(), "body");
    }
}
