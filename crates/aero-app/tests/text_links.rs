use std::sync::{Arc, Mutex};

// Exercise the exact production style merger as well as the rendered GPUI tests below.
extern crate gpui_kit as gpui;
#[path = "../../../forks/gpui-base/src/text/inline_style.rs"]
mod inline_style;

#[test]
fn link_override_wins_on_every_style_segment_without_changing_marks() {
    use gpui_kit::{FontStyle, FontWeight, HighlightStyle, TextStyle, UnderlineStyle};
    use inline_style::{InlineHighlight, combine_highlights, link_highlights, text_runs};
    let original = gpui_kit::rgb(0x123abc).into();
    let custom = gpui_kit::rgb(LINK).into();
    let linked = |mut style: HighlightStyle| {
        style.color = Some(original);
        style.underline = Some(UnderlineStyle { thickness: px(1.), ..Default::default() });
        InlineHighlight::from(style)
    };
    let plain = linked(HighlightStyle::default());
    let bold = linked(HighlightStyle { font_weight: Some(FontWeight::BOLD), ..Default::default() });
    let italic = linked(HighlightStyle { font_style: Some(FontStyle::Italic), ..Default::default() });
    let mut code = linked(HighlightStyle::default());
    code.font_family = Some("Mono".into());
    code.font_size_scale = Some(0.875);
    let marks = vec![(0..4, plain), (4..8, bold), (8..12, italic), (12..16, code)];
    for color in [Some(custom), None] {
        for thickness in [0., 1.] {
            let overrides = vec![(
                0..16,
                InlineHighlight::from(HighlightStyle {
                    color,
                    underline: Some(UnderlineStyle { thickness: px(thickness), color, wavy: false }),
                    ..Default::default()
                }),
            )];
            let result = link_highlights(marks.clone(), overrides);
            let runs = text_runs(16, &TextStyle::default(), &result);
            assert_eq!(runs.len(), 4);
            for run in &runs {
                assert_eq!(run.color, color.unwrap_or(original), "link color must win after each split");
                assert_eq!(
                    run.underline.map(|u| u.thickness).unwrap_or(px(0.)),
                    px(thickness),
                    "None/Dashed must not regain a solid underline"
                );
            }
            assert_eq!(runs[1].font.weight, FontWeight::BOLD);
            assert_eq!(runs[2].font.style, FontStyle::Italic);
            assert_eq!(runs[3].font.family.as_ref(), "Mono");
            assert_eq!(result[3].1.font_size_scale, Some(0.875));
        }
    }
    // Ordinary overlapping marks still follow their original activation order.
    let a = InlineHighlight::from(HighlightStyle { color: Some(original), ..Default::default() });
    let b = InlineHighlight::from(HighlightStyle { color: Some(custom), ..Default::default() });
    let merged = combine_highlights(vec![(4..8, a)], vec![(0..12, b)]);
    assert_eq!(merged[1].1.style.color, Some(original));
}

use gpui_kit::base::text::{LinkStyle, LinkUnderline, TextView, TextViewStyle};
use gpui_kit::{
    Bounds, Context, IntoElement, ParentElement as _, Pixels, Render, SharedString, Styled as _, StyledText,
    TestAppContext, TextAlign, TextLayout, VisualTestContext, Window, div, point, px,
};

const LINK: u32 = 0xab3456;
type Events = Arc<Mutex<Vec<(SharedString, bool, Bounds<Pixels>)>>>;

type PreviewTimer = Arc<Mutex<(Option<gpui_kit::Task<()>>, bool)>>;

struct Links {
    source: String,
    events: Events,
    reference: Option<String>,
    layout: TextLayout,
    align: TextAlign,
    width: f32,
    offset: f32,
    clip: f32,
    html: bool,
    style: LinkStyle,
    selectable: bool,
    clicks: Arc<Mutex<Vec<(SharedString, gpui_kit::ClickEvent)>>>,
    occluder_moves: Option<Arc<Mutex<usize>>>,
    preview: Option<PreviewTimer>,
}

impl Links {
    fn new(source: &str, events: Events) -> Self {
        Self {
            source: source.into(),
            events,
            reference: None,
            layout: TextLayout::default(),
            align: TextAlign::Left,
            width: 120.,
            offset: 0.,
            clip: 500.,
            html: false,
            style: LinkStyle { color: Some(gpui_kit::rgb(LINK).into()), underline: LinkUnderline::Dashed },
            selectable: false,
            clicks: Arc::default(),
            occluder_moves: None,
            preview: None,
        }
    }
}

impl Render for Links {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let events = self.events.clone();
        let clicks = self.clicks.clone();
        let preview = self.preview.clone();
        let style = self.style;
        let text = if self.html {
            TextView::html("links", self.source.clone())
        } else {
            TextView::markdown("links", self.source.clone())
        };
        let mut root =
            div().w(px(self.width)).text_size(px(16.)).line_height(px(24.)).text_align(self.align).child(
                div().h(px(self.clip)).overflow_hidden().child(
                    div().mt(px(self.offset)).child(
                        text.style(TextViewStyle::default().with_link(gpui_kit::rgb(0x123abc).into()))
                            .selectable(self.selectable)
                            .on_link_click(move |url, event, _, _| {
                                clicks.lock().unwrap().push((url.clone(), event.clone()));
                            })
                            .link_style(move |_, _| style)
                            .on_link_hover(move |url, hovered, bounds, _, cx| {
                                events.lock().unwrap().push((url.clone(), hovered, bounds));
                                if let Some(preview) = &preview {
                                    let mut state = preview.lock().unwrap();
                                    state.0 = None;
                                    state.1 = false;
                                    if hovered {
                                        let preview = preview.clone();
                                        let timer = cx
                                            .background_executor()
                                            .timer(std::time::Duration::from_millis(500));
                                        state.0 = Some(cx.spawn(async move |_| {
                                            timer.await;
                                            preview.lock().unwrap().1 = true;
                                        }));
                                    }
                                }
                            }),
                    ),
                ),
            );
        if let Some(reference) = &self.reference {
            let text = StyledText::new(reference.clone());
            self.layout = text.layout().clone();
            root = root.child(text);
        }
        if let Some(moves) = &self.occluder_moves {
            use gpui_kit::InteractiveElement as _;
            let moves = moves.clone();
            root = root.relative().child(
                div()
                    .absolute()
                    .left(px(20.))
                    .top(px(0.))
                    .w(px(80.))
                    .h(px(24.))
                    .bg(gpui_kit::rgb(0xffffff))
                    .occlude()
                    .on_mouse_move(move |_, _, cx| {
                        *moves.lock().unwrap() += 1;
                        cx.stop_propagation();
                    }),
            );
        }
        root
    }
}

fn dashes(cx: &mut VisualTestContext, color: u32) -> Vec<Bounds<Pixels>> {
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        let scale = window.scale_factor();
        window
            .painted_quads()
            .into_iter()
            .filter(|q| q.background.as_solid() == Some(gpui_kit::rgb(color).into()))
            .map(|q| q.bounds.map(|p| px(p.0 / scale)))
            .collect()
    })
}

fn move_to(cx: &mut VisualTestContext, position: gpui_kit::Point<Pixels>) {
    cx.update(|window, cx| window.simulate_mouse_move(position, cx));
    cx.run_until_parked();
}

#[gpui_kit::test]
fn links_decorate_wrapped_text_and_notify_leave(cx: &mut TestAppContext) {
    check_links(cx, "[中文😀 long linked text that wraps over many lines](aero://note/test)");
}

#[gpui_kit::test]
fn links_decorate_inline_flow_and_notify_leave(cx: &mut TestAppContext) {
    check_links(cx, "[linked `code` and more text that wraps](aero://note/test)");
}

fn check_links(cx: &mut TestAppContext, source: &str) {
    cx.update(gpui_kit::base::init);
    let events: Events = Arc::default();
    let (_, cx) = cx.add_window_view(|_, _| Links::new(source, events.clone()));
    cx.run_until_parked();
    let quads = dashes(cx, LINK);
    assert!(quads.len() > 4, "wrapped links must paint dashed underlines");
    let position = point(quads[0].left() + px(1.), quads[0].top() - px(4.));
    move_to(cx, position);
    assert!(events.lock().unwrap().iter().any(|(url, enter, bounds)| {
        url.as_ref() == "aero://note/test" && *enter && bounds.contains(&position)
    }));
    move_to(cx, point(px(500.), px(500.)));
    assert!(events.lock().unwrap().iter().any(|(_, enter, _)| !enter), "leaving the text must notify");
}

#[gpui_kit::test]
fn hover_leaves_under_a_control_that_stops_mouse_moves(cx: &mut TestAppContext) {
    cx.update(gpui_kit::base::init);
    let events: Events = Arc::default();
    let moves = Arc::new(Mutex::new(0));
    let (_, cx) = cx.add_window_view(|_, _| {
        let mut root = Links::new("[long link](target)", events.clone());
        root.occluder_moves = Some(moves.clone());
        root
    });
    move_to(cx, point(px(5.), px(5.)));
    events.lock().unwrap().clear();
    move_to(cx, point(px(30.), px(5.)));
    assert!(*moves.lock().unwrap() > 0, "control must receive the event");
    assert_eq!(events.lock().unwrap().iter().map(|e| e.1).collect::<Vec<_>>(), [false]);
}

#[gpui_kit::test]
fn code_splits_keep_one_row_anchor_and_preview_deadline(cx: &mut TestAppContext) {
    cx.update(gpui_kit::base::init);
    let events: Events = Arc::default();
    let preview = Arc::new(Mutex::new((None, false)));
    let (_, cx) = cx.add_window_view(|_, _| {
        let mut root = Links::new("[before `code` after](same)[next](same)", events.clone());
        root.width = 500.;
        root.preview = Some(preview.clone());
        root
    });
    let quads = dashes(cx, LINK);
    move_to(cx, point(px(500.), px(500.)));
    events.lock().unwrap().clear();
    // Body advances 9.6px; code advances 8.4px plus its horizontal padding.
    let positions = [point(px(5.), px(8.)), point(px(80.), px(8.)), point(px(140.), px(8.))];
    move_to(cx, positions[0]);
    cx.executor().advance_clock(std::time::Duration::from_millis(250));
    move_to(cx, positions[1]);
    cx.executor().advance_clock(std::time::Duration::from_millis(249));
    move_to(cx, positions[2]);
    assert!(!preview.lock().unwrap().1);
    cx.executor().advance_clock(std::time::Duration::from_millis(1));
    cx.run_until_parked();
    assert!(preview.lock().unwrap().1, "crossing code must not reset the 500ms preview timer");
    let entered = events.lock().unwrap().clone();
    assert_eq!(entered.len(), 1, "one link occurrence on one row: {entered:?}");
    assert!(positions.iter().all(|p| entered[0].2.contains(p)));
    assert!(
        (entered[0].2.right() - px(166.4)).abs() < px(1.),
        "anchor includes all three fragments, not next: {entered:?}, {quads:?}"
    );
    move_to(cx, point(px(170.), px(8.)));
    assert_eq!(events.lock().unwrap().iter().map(|e| e.1).collect::<Vec<_>>(), [true, false, true]);
}

#[gpui_kit::test]
fn linked_inline_image_shares_its_surrounding_link_anchor(cx: &mut TestAppContext) {
    cx.update(gpui_kit::base::init);
    let events: Events = Arc::default();
    let (_, cx) = cx.add_window_view(|_, _| {
        let mut root = Links::new(
            "[before ![image](https://example.test/image.svg) after](same)[next](same)",
            events.clone(),
        );
        root.width = 500.;
        root
    });
    dashes(cx, LINK);
    move_to(cx, point(px(500.), px(500.)));
    events.lock().unwrap().clear();
    for x in [5., 75., 100.] {
        move_to(cx, point(px(x), px(12.)));
    }
    let entered = events.lock().unwrap().clone();
    assert_eq!(entered.len(), 1, "image and surrounding text share an occurrence: {entered:?}");
    assert!(
        (entered[0].2.right() - px(142.8)).abs() < px(1.),
        "anchor must include the image, but exclude the adjacent link: {entered:?}"
    );
    move_to(cx, point(px(150.), px(12.)));
    assert_eq!(events.lock().unwrap().iter().map(|e| e.1).collect::<Vec<_>>(), [true, false, true]);
}

#[gpui_kit::test]
fn aligned_wrapped_short_row_first_glyph_clicks(cx: &mut TestAppContext) {
    cx.update(gpui_kit::base::init);
    let clicks = Arc::new(Mutex::new(Vec::new()));
    let (view, cx) = cx.add_window_view(|_, _| {
        let mut root = Links::new("[longfirstrow x](target)", Arc::default());
        root.width = 125.;
        root.clicks = clicks.clone();
        root
    });
    for align in [TextAlign::Center, TextAlign::Right] {
        view.update(cx, |view, cx| {
            view.align = align;
            cx.notify();
        });
        cx.run_until_parked();
        let quads = dashes(cx, LINK);
        let y = quads.iter().map(|q| q.top()).max().unwrap();
        assert!(y > px(24.), "must wrap");
        let x = quads.iter().filter(|q| q.top() == y).map(|q| q.left()).min().unwrap();
        clicks.lock().unwrap().clear();
        cx.simulate_click(point(x + px(0.5), y - px(4.)), Default::default());
        assert_eq!(clicks.lock().unwrap().len(), 1, "first glyph must click with {align:?}");
        cx.simulate_click(point(x - px(2.), y - px(4.)), Default::default());
        assert_eq!(clicks.lock().unwrap().len(), 1, "aligned leading whitespace is not a link");
    }
}

#[gpui_kit::test]
fn hover_leaves_on_window_exit(cx: &mut TestAppContext) {
    use gpui_kit::{InputEvent as _, MouseExitEvent};
    cx.update(gpui_kit::base::init);
    let events: Events = Arc::default();
    let (_, cx) = cx.add_window_view(|_, _| Links::new("[link](aero://note/test)", events.clone()));
    move_to(cx, point(px(5.), px(5.)));
    assert!(events.lock().unwrap().iter().any(|e| e.1));
    events.lock().unwrap().clear();
    cx.update(|window, cx| {
        window.dispatch_event(
            MouseExitEvent {
                position: point(px(5.), px(5.)),
                pressed_button: None,
                modifiers: Default::default(),
            }
            .to_platform_input(),
            cx,
        );
    });
    cx.run_until_parked();
    assert_eq!(events.lock().unwrap().iter().map(|e| e.1).collect::<Vec<_>>(), [false]);
}

#[gpui_kit::test]
fn hover_leaves_when_layout_or_content_changes_without_mouse_move(cx: &mut TestAppContext) {
    cx.update(gpui_kit::base::init);
    let events: Events = Arc::default();
    let (view, cx) = cx.add_window_view(|_, _| Links::new("[link](aero://note/test)", events.clone()));
    move_to(cx, point(px(5.), px(5.)));
    events.lock().unwrap().clear();
    view.update(cx, |view, cx| {
        view.offset = 50.;
        cx.notify();
    });
    cx.run_until_parked();
    dashes(cx, LINK);
    cx.run_until_parked();
    assert_eq!(events.lock().unwrap().iter().map(|e| e.1).collect::<Vec<_>>(), [false]);
    move_to(cx, point(px(5.), px(55.)));
    events.lock().unwrap().clear();
    view.update(cx, |view, cx| {
        view.source.clear();
        cx.notify();
    });
    cx.run_until_parked();
    dashes(cx, LINK);
    cx.run_until_parked();
    assert_eq!(
        events.lock().unwrap().iter().map(|e| e.1).collect::<Vec<_>>(),
        [false],
        "removing contents of a mounted TextView must leave"
    );
}

#[gpui_kit::test]
fn hover_leaves_on_press_and_scroll(cx: &mut TestAppContext) {
    use gpui_kit::{InputEvent as _, Modifiers, MouseButton, ScrollDelta, ScrollEvent, ScrollSource};
    cx.update(gpui_kit::base::init);
    let events: Events = Arc::default();
    let (_, cx) = cx.add_window_view(|_, _| Links::new("[link](aero://note/test)", events.clone()));
    let position = point(px(5.), px(5.));
    move_to(cx, position);
    events.lock().unwrap().clear();
    cx.simulate_mouse_down(position, MouseButton::Left, Modifiers::default());
    assert_eq!(events.lock().unwrap().iter().map(|e| e.1).collect::<Vec<_>>(), [false]);
    cx.simulate_mouse_up(position, MouseButton::Left, Modifiers::default());
    move_to(cx, point(px(500.), px(500.)));
    move_to(cx, position);
    events.lock().unwrap().clear();
    cx.update(|window, cx| {
        window.dispatch_event(
            ScrollEvent {
                position,
                delta: ScrollDelta::Pixels(point(px(0.), px(-24.))),
                modifiers: Default::default(),
                touch_phase: Default::default(),
                source: ScrollSource::Pointer,
            }
            .to_platform_input(),
            cx,
        );
    });
    cx.run_until_parked();
    assert_eq!(events.lock().unwrap().iter().map(|e| e.1).collect::<Vec<_>>(), [false]);
}

#[gpui_kit::test]
fn linked_image_uses_hover_hook(cx: &mut TestAppContext) {
    check_linked_image(cx, "");
}

#[gpui_kit::test]
fn linked_image_in_inline_flow_uses_hover_hook(cx: &mut TestAppContext) {
    check_linked_image(cx, " after");
}

fn check_linked_image(cx: &mut TestAppContext, suffix: &str) {
    cx.update(|cx| {
        gpui_kit::base::init(cx);
        cx.set_http_client(gpui_kit::http_client::FakeHttpClient::create(|_| async {
            Ok(gpui_kit::http_client::Response::builder().status(200).body(
                gpui_kit::http_client::AsyncBody::from(r#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32"><rect width="32" height="32" fill="red"/></svg>"#)
            ).unwrap())
        }));
    });
    let events: Events = Arc::default();
    let clicks = Arc::new(Mutex::new(Vec::new()));
    let (_, cx) = cx.add_window_view(|_, _| {
        let mut root = Links::new(
            &format!("[<img src=\"https://example.test/image.svg\" width=\"32\" height=\"32\">](aero://note/image){suffix}"),
            events.clone(),
        );
        root.selectable = true;
        root.clip = 16.;
        root.clicks = clicks.clone();
        root
    });
    cx.run_until_parked();
    dashes(cx, LINK);
    move_to(cx, point(px(500.), px(500.)));
    events.lock().unwrap().clear();
    move_to(cx, point(px(8.), px(8.)));
    let entered = events.lock().unwrap().clone();
    assert_eq!(entered.len(), 1, "image link must enter: {entered:?}");
    assert_eq!(entered[0].0.as_ref(), "aero://note/image");
    assert!(entered[0].1);
    assert_eq!(entered[0].2.size, gpui_kit::size(px(32.), px(16.)));
    move_to(cx, point(px(8.), px(24.)));
    assert_eq!(events.lock().unwrap().iter().map(|e| e.1).collect::<Vec<_>>(), [true, false]);
    use gpui_kit::{Modifiers, MouseButton};
    for button in [MouseButton::Left, MouseButton::Middle, MouseButton::Right] {
        cx.simulate_mouse_down(point(px(8.), px(8.)), button, Modifiers::default());
        cx.simulate_mouse_up(point(px(8.), px(8.)), button, Modifiers::default());
    }
    let clicks = clicks.lock().unwrap();
    assert_eq!(clicks.len(), 3);
    assert!(clicks.iter().all(|(url, _)| url.as_ref() == "aero://note/image"));
    assert!(clicks[1].1.is_middle_click());
    assert!(clicks[2].1.is_right_click());
}

#[gpui_kit::test]
fn clipped_and_repeated_links_have_distinct_hover_regions(cx: &mut TestAppContext) {
    cx.update(gpui_kit::base::init);
    let events: Events = Arc::default();
    let (_, cx) = cx.add_window_view(|_, _| {
        let mut root =
            Links::new("[one](same)[two](same) [three](other)\n\n[hidden](hidden)", events.clone());
        root.width = 300.;
        root.clip = 12.;
        root
    });
    move_to(cx, point(px(500.), px(500.)));
    events.lock().unwrap().clear();
    for x in [5., 35., 75.] {
        move_to(cx, point(px(x), px(5.)));
    }
    let transitions: Vec<_> = events.lock().unwrap().iter().map(|e| (e.0.to_string(), e.1)).collect();
    assert_eq!(
        transitions,
        [
            ("same".into(), true),
            ("same".into(), false),
            ("same".into(), true),
            ("same".into(), false),
            ("other".into(), true)
        ]
    );
    assert!(events.lock().unwrap().iter().all(|e| e.2.bottom() == px(12.)));
    move_to(cx, point(px(5.), px(18.)));
    events.lock().unwrap().clear();
    move_to(cx, point(px(5.), px(50.)));
    assert!(events.lock().unwrap().is_empty(), "clipped links cannot be entered");
    assert!(dashes(cx, LINK).is_empty(), "the underline lies below the clip");
}

#[test]
fn rendered_nested_links_switch_none_solid_dashed() {
    use gpui_kit::{AppContext as _, HeadlessAppContext, NoopTextSystem, size};
    let mut cx = HeadlessAppContext::with_platform(
        Arc::new(NoopTextSystem),
        Arc::new(()),
        gpui_kit::platform::current_headless_renderer,
    );
    cx.update(gpui_kit::base::init);
    let root = cx.new(|_| {
        let mut root = Links::new("[plain **bold *italic*** `code` end](target)", Arc::default());
        root.width = 500.;
        root.style.color = Some(gpui_kit::rgb(0xff0000).into());
        root
    });
    let window = cx.open_window(size(px(500.), px(64.)), |_, _| root.clone()).unwrap();
    let mut samples = Vec::new();
    for underline in [LinkUnderline::None, LinkUnderline::Solid, LinkUnderline::Dashed, LinkUnderline::None] {
        root.update(&mut cx, |root, cx| {
            root.style.underline = underline;
            cx.notify();
        });
        cx.run_until_parked();
        let scale = cx
            .update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.scale_factor()
            })
            .unwrap();
        let image = cx.capture_screenshot(window.into()).expect("real headless rendering");
        // NoopTextSystem paints no glyphs: red pixels are the actual decorations,
        // including GPUI's solid underline primitives (not painted_quads).
        let spans = [(0., 40.), (48., 80.), (88., 136.), (154., 182.), (192., 216.)];
        let counts: Vec<_> = spans
            .iter()
            .map(|(left, right)| {
                image
                    .enumerate_pixels()
                    .filter(|(x, _, p)| {
                        (*x as f32) >= left * scale
                            && (*x as f32) < right * scale
                            && p[0] > 180
                            && p[1] < 80
                            && p[2] < 80
                            && p[3] > 128
                    })
                    .count()
            })
            .collect();
        samples.push(counts);
    }
    assert!(samples[0].iter().all(|n| *n == 0), "None: {samples:?}");
    assert!(samples[1].iter().all(|n| *n > 0), "Solid must paint every nested mark: {samples:?}");
    assert!(
        samples[2].iter().zip(&samples[1]).all(|(dashed, solid)| *dashed > 0 && dashed < solid),
        "Dashed must have real gaps on every nested mark: {samples:?}"
    );
    assert_eq!(samples[3], samples[0], "switching back to None clears decorations");
}

#[gpui_kit::test]
fn unspecified_color_keeps_original_link_color(cx: &mut TestAppContext) {
    cx.update(gpui_kit::base::init);
    let (_, cx) = cx.add_window_view(|_, _| {
        let mut root =
            Links::new("[plain **strong** *emphasis* `code` end](aero://note/test)", Arc::default());
        root.style.color = None;
        root
    });
    cx.run_until_parked();
    assert!(!dashes(cx, 0x123abc).is_empty(), "dashes must inherit the link color, not the body color");
}

#[gpui_kit::test]
fn every_visual_line_has_exact_link_geometry(cx: &mut TestAppContext) {
    cx.update(gpui_kit::base::init);
    let events: Events = Arc::default();
    let text = "中文😀 office affinity 中文😀 office affinity\n首字😀 ffi 中文尾行";
    let (view, cx) = cx.add_window_view(|_, _| {
        let mut root =
            Links::new(&format!("[{}](aero://note/test)", text.replace('\n', "  \n")), events.clone());
        root.reference = Some(text.into());
        root
    });
    for align in [TextAlign::Left, TextAlign::Center, TextAlign::Right] {
        view.update(cx, |view, cx| {
            view.align = align;
            cx.notify();
        });
        cx.run_until_parked();
        let quads = dashes(cx, LINK);
        // Scene quads snap to device pixels; hover rectangles remain logical pixels.
        let snap_tolerance = cx.update(|window, _| px(0.51 / window.scale_factor()));
        let layout = view.read_with(cx, |view, _| view.layout.clone());
        let mut y = px(0.);
        let mut expected = Vec::new();
        for line in layout.line_layouts() {
            let unwrapped = &line.unwrapped_layout;
            let mut start = px(0.);
            for end in line
                .wrap_boundaries
                .iter()
                .map(|b| unwrapped.runs[b.run_ix].glyphs[b.glyph_ix].position.x)
                .chain([unwrapped.width])
            {
                let width = end - start;
                let x = match align {
                    TextAlign::Left => px(0.),
                    TextAlign::Center => (layout.bounds().size.width - width) / 2.,
                    TextAlign::Right => layout.bounds().size.width - width,
                };
                expected.push(Bounds::from_corners(point(x, y), point(x + width, y + layout.line_height())));
                y += layout.line_height();
                start = end;
            }
        }
        assert!(expected.len() >= 4);
        for row in &expected {
            let row_quads: Vec<_> =
                quads.iter().filter(|q| q.top() >= row.top() && q.top() < row.bottom()).collect();
            assert!(!row_quads.is_empty(), "no dashes on {row:?}, {align:?}");
            assert!(
                (row_quads[0].left() - row.left()).abs() < snap_tolerance,
                "first glyph omitted: {row:?}, {:?}, {align:?}",
                row_quads[0]
            );
            assert!(
                row_quads
                    .iter()
                    .all(|q| q.left() >= row.left() - snap_tolerance
                        && q.right() <= row.right() + snap_tolerance),
                "dashes overflow {row:?}: {row_quads:?}"
            );
            move_to(cx, point(px(500.), px(500.)));
            events.lock().unwrap().clear();
            let first_glyph = point(row.left() + px(0.5), row.top() + px(4.));
            move_to(cx, first_glyph);
            let captured = events.lock().unwrap().clone();
            assert_eq!(captured.len(), 1, "first glyph hover: {row:?}: {captured:?}");
            assert!(captured[0].1);
            let actual = captured[0].2;
            assert!((actual.left() - row.left()).abs() < px(0.1));
            assert!((actual.right() - row.right()).abs() < px(0.1), "hover overflow: {actual:?}, {row:?}");
            assert_eq!(actual.top(), row.top());
            assert_eq!(actual.bottom(), row.bottom());
        }
        assert!(
            quads.iter().all(|q| expected.iter().any(|r| q.top() >= r.top() && q.bottom() <= r.bottom()))
        );
    }
}
