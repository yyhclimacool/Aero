//! 快速打开（`ui_design.md` 3.2）：居中弹出的模糊搜索框，范围是笔记标题与路径。
//!
//! 空输入显示最近打开的笔记；`⏎` 打开，`⌘⏎` 或无匹配时新建同名笔记；`Esc` 关闭。
//! 候选先用文件名（同步、立刻可用），索引就位后换成 front matter 标题。别名（aliases）随 M2 的属性解析加入。

use std::sync::Arc;

use aero_core::{Vault, VaultPath};
use gpui_kit::assets::IconName;
use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Icon, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::actions::{Dismiss, SelectNext, SelectPrev};
use crate::search::SearchService;

const MAX_SHOWN: usize = 12;
const RECENT_SHOWN: usize = 10;

pub enum QuickOpenEvent {
    Open(VaultPath),
    /// 新建名为该串的笔记
    Create(String),
    Dismiss,
}

impl EventEmitter<QuickOpenEvent> for QuickOpen {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub path: VaultPath,
    pub title: String,
}

pub struct QuickOpen {
    input: Entity<InputState>,
    candidates: Vec<Candidate>,
    /// 最近打开的在前
    recent: Vec<VaultPath>,
    /// 当前显示的候选下标（已排序）
    shown: Vec<usize>,
    /// 输入非空：列表末尾多一行"新建"
    has_query: bool,
    selected: usize,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl QuickOpen {
    pub fn new(
        vault: Arc<Vault>,
        service: Entity<SearchService>,
        recent: Vec<VaultPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("搜索笔记标题或路径…"));
        let sub = cx.subscribe_in(&input, window, Self::on_input_event);
        // 先用文件名；索引里的标题到了再换
        let candidates: Vec<Candidate> = vault
            .notes()
            .unwrap_or_default()
            .into_iter()
            .map(|p| Candidate { title: p.stem().to_owned(), path: p })
            .collect();
        let titles = service.read(cx).note_titles(cx);
        cx.spawn(async move |this, cx| {
            let titles = titles.await;
            if titles.is_empty() {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                for (path, title) in titles {
                    if let Some(c) = this.candidates.iter_mut().find(|c| c.path == path) {
                        c.title = title;
                    }
                }
                this.refilter(cx);
            });
        })
        .detach();
        let mut this = QuickOpen {
            input,
            candidates,
            recent,
            shown: Vec::new(),
            has_query: false,
            selected: 0,
            scroll: ScrollHandle::new(),
            _subscriptions: vec![sub],
        };
        this.refilter(cx);
        this.input.update(cx, |s, cx| s.focus(window, cx));
        this
    }

    #[cfg(test)]
    pub fn shown_paths(&self) -> Vec<&str> {
        self.shown.iter().map(|&i| self.candidates[i].path.as_str()).collect()
    }

    #[cfg(test)]
    pub fn selected(&self) -> usize {
        self.selected
    }

    fn query(&self, cx: &App) -> String {
        self.input.read(cx).value().trim().to_string()
    }

    fn on_input_event(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => self.refilter(cx),
            InputEvent::PressEnter { secondary, .. } => self.confirm(*secondary, cx),
            InputEvent::Focus => {}
            // 点到别处：关闭
            InputEvent::Blur => cx.emit(QuickOpenEvent::Dismiss),
        }
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let q = self.query(cx);
        self.has_query = !q.is_empty();
        self.shown = if q.is_empty() {
            // 最近打开优先，不足的按路径补
            let mut out: Vec<usize> = self
                .recent
                .iter()
                .filter_map(|r| self.candidates.iter().position(|c| &c.path == r))
                .take(RECENT_SHOWN)
                .collect();
            for i in 0..self.candidates.len() {
                if out.len() >= RECENT_SHOWN {
                    break;
                }
                if !out.contains(&i) {
                    out.push(i);
                }
            }
            out
        } else {
            let mut scored: Vec<(i32, usize)> = self
                .candidates
                .iter()
                .enumerate()
                .filter_map(|(i, c)| {
                    let s = fuzzy_score(&q, &c.title).max(fuzzy_score(&q, c.path.as_str()))?;
                    // 最近打开的加一点分
                    let bonus =
                        self.recent.iter().position(|r| r == &c.path).map_or(0, |p| 20 - (p as i32).min(10));
                    Some((s + bonus, i))
                })
                .collect();
            scored.sort_by(|a, b| {
                b.0.cmp(&a.0).then_with(|| self.candidates[a.1].path.cmp(&self.candidates[b.1].path))
            });
            scored.into_iter().take(MAX_SHOWN).map(|(_, i)| i).collect()
        };
        self.selected = 0;
        cx.notify();
    }

    fn on_select_prev(&mut self, _: &SelectPrev, _window: &mut Window, cx: &mut Context<Self>) {
        let n = self.rows();
        if n > 0 {
            self.selected = (self.selected + n - 1) % n;
            cx.notify();
        }
    }

    fn on_select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        let n = self.rows();
        if n > 0 {
            self.selected = (self.selected + 1) % n;
            cx.notify();
        }
    }

    fn on_dismiss(&mut self, _: &Dismiss, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(QuickOpenEvent::Dismiss);
    }

    /// 可选行数：候选 + （有输入时）"新建"一行。
    fn rows(&self) -> usize {
        self.shown.len() + usize::from(self.has_query)
    }

    fn confirm(&mut self, create: bool, cx: &mut Context<Self>) {
        let q = self.query(cx);
        if create || self.shown.is_empty() || self.selected >= self.shown.len() {
            if !q.is_empty() {
                cx.emit(QuickOpenEvent::Create(q));
            }
            return;
        }
        let path = self.candidates[self.shown[self.selected]].path.clone();
        cx.emit(QuickOpenEvent::Open(path));
    }
}

/// 子序列模糊匹配得分：不匹配返回 `None`。前缀 / 连续 / 词首命中加分，越长的候选略减分。
pub fn fuzzy_score(query: &str, target: &str) -> Option<i32> {
    let q: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    let t: Vec<char> = target.chars().flat_map(char::to_lowercase).collect();
    if q.is_empty() {
        return Some(0);
    }
    let mut score = 0i32;
    let mut ti = 0usize;
    let mut prev_hit: Option<usize> = None;
    for &qc in &q {
        let pos = t[ti..].iter().position(|&c| c == qc)? + ti;
        score += 10;
        if pos == 0 {
            score += 15;
        } else if !t[pos - 1].is_alphanumeric() {
            score += 8;
        }
        if prev_hit == Some(pos.saturating_sub(1)) && pos > 0 {
            score += 12;
        }
        prev_hit = Some(pos);
        ti = pos + 1;
    }
    Some(score - (t.len() as i32 / 8))
}

impl Render for QuickOpen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let (hover, active) = (theme.list_hover, theme.list_active);
        let q = self.query(cx);
        let rows: Vec<AnyElement> = self
            .shown
            .iter()
            .enumerate()
            .map(|(row, &i)| {
                let c = &self.candidates[i];
                let dir = c.path.parent();
                let dir_label = if dir.is_empty() { String::new() } else { format!("{dir}/") };
                let selected = row == self.selected;
                h_flex()
                    .id(("qo-row", row))
                    .w_full()
                    .px_3()
                    .py(px(6.))
                    .rounded_md()
                    .gap_3()
                    .items_baseline()
                    .cursor_pointer()
                    .when(selected, |el| el.bg(active))
                    .when(!selected, |el| el.hover(move |s| s.bg(hover)))
                    .child(div().flex_1().min_w_0().truncate().text_sm().child(c.title.clone()))
                    .child(div().flex_none().text_xs().text_color(muted).child(dir_label))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected = row;
                        this.confirm(false, cx);
                    }))
                    .into_any_element()
            })
            .collect();
        let create_row: Option<AnyElement> = (!q.is_empty()).then(|| {
            let row = self.shown.len();
            let selected = self.selected == row;
            h_flex()
                .id("qo-create")
                .w_full()
                .px_3()
                .py(px(6.))
                .rounded_md()
                .gap_2()
                .items_center()
                .cursor_pointer()
                .text_sm()
                .when(selected, |el| el.bg(active))
                .when(!selected, |el| el.hover(move |s| s.bg(hover)))
                .child(Icon::new(IconName::Plus).size(px(14.)).text_color(muted))
                .child(div().flex_1().min_w_0().truncate().child(format!("新建 “{q}”")))
                .child(div().text_xs().text_color(muted).child("⌘⏎"))
                .on_click(cx.listener(|this, _, _, cx| this.confirm(true, cx)))
                .into_any_element()
        });
        let empty = self.shown.is_empty() && create_row.is_none();

        v_flex()
            .id("quick-open")
            .key_context("Picker")
            .on_action(cx.listener(Self::on_select_prev))
            .on_action(cx.listener(Self::on_select_next))
            .on_action(cx.listener(Self::on_dismiss))
            .w(px(560.))
            .max_h(px(440.))
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .border_1()
            .border_color(theme.border)
            .rounded_lg()
            .shadow_lg()
            .overflow_hidden()
            .child(
                div().p_2().border_b_1().border_color(theme.border).child(
                    Input::new(&self.input)
                        .prefix(Icon::new(IconName::Search).text_color(muted).size(px(14.)))
                        .appearance(false),
                ),
            )
            .child(
                v_flex()
                    .id("qo-rows")
                    .flex_1()
                    .min_h(px(0.))
                    .p_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children(rows)
                    .when(empty, |el| {
                        el.child(
                            div()
                                .px_3()
                                .py_2()
                                .text_sm()
                                .text_color(muted)
                                .child("这个 vault 里还没有笔记。"),
                        )
                    })
                    .children(create_row)
                    .vertical_scrollbar(&self.scroll),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;

    use aero_core::{Vault, VaultPath};
    use gpui_kit::{AppContext as _, TestAppContext};

    // 不能 `use super::*`：它会把 GPUI 的 `test` 属性宏带进作用域，遮住标准 `#[test]`
    use super::{QuickOpen, QuickOpenEvent, fuzzy_score};
    use crate::search::SearchService;

    #[test]
    fn fuzzy_prefers_prefix_and_word_starts() {
        assert!(
            fuzzy_score("ripg", "ripgrep").unwrap()
                > fuzzy_score("ripg", "projects/ripgrep 性能笔记").unwrap()
        );
        assert_eq!(fuzzy_score("ripg", "Rust CLI"), None);
        assert!(fuzzy_score("rc", "Rust CLI").unwrap() > fuzzy_score("rc", "arch").unwrap());
        assert_eq!(fuzzy_score("GPUI", "gpui 笔记"), fuzzy_score("gpui", "GPUI 笔记"));
        assert_eq!(fuzzy_score("", "x"), Some(0));
    }

    #[gpui_kit::test]
    fn quick_open_lists_recent_filters_and_creates(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::actions::init(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("rust")).unwrap();
        for (p, body) in [
            ("rust/ripgrep.md", "# ripgrep\n"),
            ("rust/cli.md", "---\ntitle: Rust CLI\n---\n"),
            ("todo.md", ""),
        ] {
            std::fs::write(dir.path().join(p), body).unwrap();
        }
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let service =
            cx.update(|cx| cx.new(|cx| SearchService::new(vault.clone(), data.path().to_path_buf(), cx)));
        let recent = vec![VaultPath::new("todo.md").unwrap()];
        let (picker, cx) =
            cx.add_window_view(|window, cx| QuickOpen::new(vault, service, recent, window, cx));
        let events: Rc<RefCell<Vec<String>>> = Rc::default();
        let sink = events.clone();
        cx.update(|_, cx| {
            cx.subscribe(&picker, move |_, ev: &QuickOpenEvent, _| {
                sink.borrow_mut().push(match ev {
                    QuickOpenEvent::Open(p) => format!("open {}", p.as_str()),
                    QuickOpenEvent::Create(n) => format!("create {n}"),
                    QuickOpenEvent::Dismiss => "dismiss".into(),
                });
            })
            .detach();
        });
        cx.run_until_parked();

        // 空输入：最近打开的在前，其余按路径补齐
        cx.update(|_, cx| {
            assert_eq!(picker.read(cx).shown_paths(), ["todo.md", "rust/cli.md", "rust/ripgrep.md"])
        });

        // 过滤：标题 / 路径都算
        cx.simulate_input("rip");
        cx.update(|_, cx| assert_eq!(picker.read(cx).shown_paths(), ["rust/ripgrep.md"]));
        // ↓ 到"新建"行再 ⏎：新建
        cx.simulate_keystrokes("down");
        cx.update(|_, cx| assert_eq!(picker.read(cx).selected(), 1));
        cx.simulate_keystrokes("enter");
        assert_eq!(events.borrow().last().map(String::as_str), Some("create rip"));

        // 换一个词，直接 ⏎ 打开第一条
        picker.update_in(cx, |p, window, cx| p.input.update(cx, |s, cx| s.set_value("", window, cx)));
        picker.update_in(cx, |p, _, cx| p.refilter(cx));
        cx.simulate_input("cli");
        cx.update(|_, cx| assert_eq!(picker.read(cx).shown_paths(), ["rust/cli.md"]));
        cx.simulate_keystrokes("enter");
        assert_eq!(events.borrow().last().map(String::as_str), Some("open rust/cli.md"));

        // 无匹配：⏎ 就是新建；Esc 关闭
        picker.update_in(cx, |p, window, cx| p.input.update(cx, |s, cx| s.set_value("", window, cx)));
        picker.update_in(cx, |p, _, cx| p.refilter(cx));
        cx.simulate_input("zzz");
        cx.update(|_, cx| assert!(picker.read(cx).shown_paths().is_empty()));
        cx.simulate_keystrokes("enter");
        assert_eq!(events.borrow().last().map(String::as_str), Some("create zzz"));
        cx.simulate_keystrokes("escape");
        assert_eq!(events.borrow().last().map(String::as_str), Some("dismiss"));
    }
}
