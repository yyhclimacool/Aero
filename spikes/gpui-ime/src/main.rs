//! M0 spike #1：验证 GPUI Kit 能否作为 Aero 的 UI 底座。
//!
//! 只回答三个问题（见 docs/project_plan.md 第 18 节 M0）：
//! 1. 锁定版本的 gpui-kit 在本机能否编译、开窗、渲染中文；
//! 2. VirtualList 承载 10k 行是否流畅；
//! 3. Textarea 的 macOS 中文输入法体验——候选框位置、组合中文本、回退、
//!    以及 `InputEvent::Change` 在组合期间是否触发（这决定编辑器重解析的时机）。
//!
//! 手工验收清单见同目录 README.md。结论写入 docs/decisions/，随后删除本目录。

use std::rc::Rc;
use std::time::Instant;

use gpui_kit::assets::Assets;
use gpui_kit::component::{
    ActiveTheme as _, Root, VirtualListScrollHandle, h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    scroll::ScrollableElement as _,
    v_flex, v_virtual_list,
};
use gpui_kit::*;

actions!(spike_gpui_ime, [Quit]);

const ROWS: usize = 10_000;
const ROW_HEIGHT: f32 = 28.;
const MAX_LOG: usize = 14;

const SAMPLE: &str = "# 安装 ripgrep\n\n\
安装 ripgrep 的方法有两种，推荐 `brew install ripgrep`。\n\
中英文混排：Rust 的 rope 结构对 CJK 字符按 UTF-8 计数，光标应落在字符边界上。\n\n\
- 参考 [[Rust CLI]] 与 #工具\n\
- 全角标点：，。！？「」\n\
- Emoji 与变体：👍🏽 🇨🇳 ﬁ\n";

struct Spike {
    textarea: Entity<TextareaState>,
    title: Entity<InputState>,
    row_sizes: Rc<Vec<Size<Pixels>>>,
    /// 滚动偏移保存在 handle 里；`v_virtual_list` 每帧会新建一个临时 handle，
    /// 不把持久 handle 传给 `track_scroll` 的话，偏移每帧被重置为 0，列表看起来无法滚动。
    rows_scroll: VirtualListScrollHandle,
    rows_visible: std::ops::Range<usize>,
    log: Vec<String>,
    started: Instant,
    _subscriptions: Vec<Subscription>,
}

impl Spike {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let textarea = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("在这里输入中文……")
                .default_value(SAMPLE)
        });
        let title = cx.new(|cx| InputState::new(window, cx).placeholder("单行输入：笔记标题"));

        let subscriptions = vec![
            cx.subscribe_in(&textarea, window, |this, _, ev: &InputEvent, _, cx| {
                this.push_log("textarea", ev);
                cx.notify();
            }),
            cx.subscribe_in(&title, window, |this, _, ev: &InputEvent, _, cx| {
                this.push_log("title", ev);
                cx.notify();
            }),
        ];

        Self {
            textarea,
            title,
            row_sizes: Rc::new(vec![size(px(240.), px(ROW_HEIGHT)); ROWS]),
            rows_scroll: VirtualListScrollHandle::new(),
            rows_visible: 0..0,
            log: Vec::new(),
            started: Instant::now(),
            _subscriptions: subscriptions,
        }
    }

    fn push_log(&mut self, source: &str, ev: &InputEvent) {
        let t = self.started.elapsed().as_millis();
        #[allow(unreachable_patterns)]
        let what = match ev {
            InputEvent::Change => "Change".to_string(),
            InputEvent::PressEnter { secondary, shift } => {
                format!("PressEnter secondary={secondary} shift={shift}")
            }
            InputEvent::Focus => "Focus".to_string(),
            InputEvent::Blur => "Blur".to_string(),
            _ => "Other".to_string(),
        };
        self.log.push(format!("{t:>7} ms  {source:<9} {what}"));
        if self.log.len() > MAX_LOG {
            let overflow = self.log.len() - MAX_LOG;
            self.log.drain(..overflow);
        }
    }
}

impl Render for Spike {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (status, preview) = {
            let state = self.textarea.read(cx);
            let value = state.value();
            let cursor = state.cursor();
            let sel = state.selected_range();
            let status = format!(
                "字符 {} · 字节 {} · 光标(byte) {} · 选区 {}..{}",
                value.chars().count(),
                value.len(),
                cursor,
                sel.start,
                sel.end
            );
            // 光标前 24 个字符，用来核对 IME 提交后的实际内容
            let head: String = value
                .get(..cursor.min(value.len()))
                .unwrap_or("")
                .chars()
                .rev()
                .take(24)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            (status, format!("光标前: …{head}"))
        };

        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;

        let sidebar = v_flex()
            .w(px(240.))
            .h_full()
            .border_r_1()
            .border_color(border)
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_sm()
                    .text_color(muted)
                    .child(format!(
                        "VirtualList · {ROWS} 行 · 可见 {}..{}",
                        self.rows_visible.start, self.rows_visible.end
                    )),
            )
            .child(
                v_flex()
                    .id("rows-container")
                    .relative()
                    .flex_1()
                    .min_h(px(0.))
                    .child(
                        v_virtual_list(
                            cx.entity().clone(),
                            "rows",
                            self.row_sizes.clone(),
                            |this, range, _, _| {
                                this.rows_visible = range.clone();
                                range
                                    .map(|ix| {
                                        div()
                                            .h(px(ROW_HEIGHT))
                                            .px_3()
                                            .flex()
                                            .items_center()
                                            .text_sm()
                                            .child(format!("笔记 {ix:05} · 第{ix}篇.md"))
                                    })
                                    .collect()
                            },
                        )
                        .track_scroll(&self.rows_scroll),
                    )
                    .vertical_scrollbar(&self.rows_scroll),
            );

        let editor = v_flex()
            .flex_1()
            .h_full()
            .p_4()
            .gap_3()
            .child(Input::new(&self.title))
            .child(Textarea::new(&self.textarea).h(px(340.)))
            .child(div().text_sm().text_color(muted).child(status))
            .child(div().text_sm().text_color(muted).child(preview))
            .child(
                v_flex()
                    .flex_1()
                    .min_h(px(0.))
                    .p_2()
                    .gap_0p5()
                    .border_1()
                    .border_color(border)
                    .rounded_md()
                    .font_family("Menlo")
                    .text_xs()
                    .child(div().text_color(muted).child("InputEvent 日志（最近 14 条）"))
                    .children(self.log.iter().cloned().map(|line| div().child(line))),
            );

        h_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(sidebar)
            .child(editor)
    }
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(move |cx| {
        gpui_kit::init(cx);

        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);

        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1100.), px(720.)), cx)),
            titlebar: Some(TitlebarOptions {
                title: Some("Aero spike · GPUI IME".into()),
                ..Default::default()
            }),
            ..Default::default()
        };

        cx.spawn(async move |cx| {
            cx.open_window(window_options, |window, cx| {
                let view = cx.new(|cx| Spike::new(window, cx));
                cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
            })
            .expect("failed to open window");
        })
        .detach();
    });
}
