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

use crate::actions::{Command, Dismiss, SelectNext, SelectPrev};
use crate::search::SearchService;

const MAX_SHOWN: usize = 12;
const RECENT_SHOWN: usize = 10;

pub enum QuickOpenEvent {
    Open(VaultPath),
    /// 新建名为该串的笔记
    Create(String),
    Execute {
        id: &'static str,
        action: Box<dyn Action>,
    },
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
    has_vault: bool,
    /// 最近打开的在前
    recent: Vec<VaultPath>,
    commands: Vec<Command>,
    recent_commands: Vec<&'static str>,
    command_mode: bool,
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
        commands: Vec<Command>,
        recent_commands: Vec<&'static str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("搜索笔记标题或路径，输入 > 搜索命令…"));
        let sub = cx.subscribe_in(&input, window, Self::on_input_event);
        let mut this = QuickOpen {
            input,
            candidates: Vec::new(),
            has_vault: false,
            recent: Vec::new(),
            commands,
            recent_commands,
            command_mode: false,
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

    pub fn set_notes(
        &mut self,
        vault: Arc<Vault>,
        service: Entity<SearchService>,
        recent: Vec<VaultPath>,
        cx: &mut Context<Self>,
    ) {
        self.candidates = vault
            .notes()
            .unwrap_or_default()
            .into_iter()
            .map(|p| Candidate { title: p.stem().to_owned(), path: p })
            .collect();
        self.recent = recent;
        self.has_vault = true;
        self.refilter(cx);
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
                if !this.command_mode {
                    this.refilter(cx);
                }
            });
        })
        .detach();
    }

    pub fn show_commands(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |s, cx| {
            s.set_value("> ", window, cx);
            s.set_selected_range(2..2, cx);
            s.focus(window, cx);
        });
        self.refilter(cx);
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
        self.command_mode = q.starts_with('>');
        self.has_query = self.has_vault && !self.command_mode && !q.is_empty();
        self.shown = if let Some(query) = q.strip_prefix('>') {
            let query = query.trim();
            let mut scored: Vec<_> = self
                .commands
                .iter()
                .enumerate()
                .filter_map(|(i, command)| {
                    let score =
                        fuzzy_score(query, command.label).max(fuzzy_score(query, command.keywords))?;
                    let recent =
                        self.recent_commands.iter().position(|id| *id == command.id).unwrap_or(usize::MAX);
                    Some((i, score, recent))
                })
                .collect();
            scored.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| b.1.cmp(&a.1)).then_with(|| a.0.cmp(&b.0)));
            scored.into_iter().map(|(i, _, _)| i).collect()
        } else if q.is_empty() {
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
        self.scroll.scroll_to_item(0);
        cx.notify();
    }

    fn on_select_prev(&mut self, _: &SelectPrev, _window: &mut Window, cx: &mut Context<Self>) {
        let n = self.rows();
        if n > 0 {
            self.selected = (self.selected + n - 1) % n;
            self.scroll.scroll_to_item(self.selected);
            cx.notify();
        }
    }

    fn on_select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        let n = self.rows();
        if n > 0 {
            self.selected = (self.selected + 1) % n;
            self.scroll.scroll_to_item(self.selected);
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
        if self.command_mode {
            if let Some(&i) = self.shown.get(self.selected) {
                let command = &self.commands[i];
                cx.emit(QuickOpenEvent::Execute { id: command.id, action: command.action.boxed_clone() });
            }
            return;
        }
        if !self.has_vault {
            return;
        }
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
                let (title, detail) = if self.command_mode {
                    let command = &self.commands[i];
                    (command.label.to_owned(), command.shortcut.clone())
                } else {
                    let c = &self.candidates[i];
                    let dir = c.path.parent();
                    (c.title.clone(), if dir.is_empty() { String::new() } else { format!("{dir}/") })
                };
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
                    .debug_selector(move || format!("picker-row-{row}"))
                    .child(div().flex_1().min_w_0().truncate().text_sm().child(title))
                    .child(div().flex_none().text_xs().text_color(muted).child(detail))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected = row;
                        this.confirm(false, cx);
                    }))
                    .into_any_element()
            })
            .collect();
        let create_row: Option<AnyElement> = self.has_query.then(|| {
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
                        el.child(div().px_3().py_2().text_sm().text_color(muted).child(
                            if self.command_mode {
                                "没有匹配的可用命令。"
                            } else if !self.has_vault {
                                "请先打开 Vault，或输入 > 搜索命令。"
                            } else {
                                "这个 vault 里还没有笔记。"
                            },
                        ))
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
    fn commands_filter_rank_and_confirm_without_creating(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::actions::init(cx);
        });
        let (picker, cx) = cx.add_window_view(|window, cx| {
            let mut picker =
                QuickOpen::new(crate::actions::commands(), vec!["open-daily", "new-note"], window, cx);
            picker.show_commands(window, cx);
            picker
        });
        let executed = Rc::new(RefCell::new(Vec::new()));
        let sink = executed.clone();
        cx.update(|_, cx| {
            cx.subscribe(&picker, move |_, event, _| {
                if let QuickOpenEvent::Execute { id, .. } = event {
                    sink.borrow_mut().push(*id);
                }
            })
            .detach();
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            let p = picker.read(cx);
            assert!(p.command_mode);
            assert!(!p.has_query);
            assert_eq!(p.commands[p.shown[0]].id, "open-daily");
            assert_eq!(p.commands[p.shown[1]].id, "new-note");
        });
        cx.simulate_input("新建笔记");
        cx.update(|_, cx| {
            let p = picker.read(cx);
            assert_eq!(p.shown.len(), 1);
            assert_eq!(p.commands[p.shown[0]].id, "new-note");
        });
        cx.simulate_keystrokes("enter");
        assert_eq!(*executed.borrow(), ["new-note"]);
        picker.update_in(cx, |p, window, cx| {
            p.input.update(cx, |s, cx| s.set_value("> DAILY", window, cx));
            p.refilter(cx);
        });
        cx.simulate_keystrokes("cmd-enter");
        assert_eq!(*executed.borrow(), ["new-note", "open-daily"]);
        picker.update_in(cx, |p, window, cx| {
            p.input.update(cx, |s, cx| s.set_value("", window, cx));
            p.refilter(cx);
        });
        cx.update(|_, cx| {
            let p = picker.read(cx);
            assert!(!p.command_mode);
            assert_eq!(p.rows(), 0);
        });
    }

    #[gpui_kit::test]
    fn palette_without_vault_cannot_create_notes_after_removing_prefix(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::actions::init(cx);
        });
        let (picker, cx) = cx.add_window_view(|window, cx| {
            let mut picker = QuickOpen::new(crate::actions::commands(), vec![], window, cx);
            picker.show_commands(window, cx);
            picker
        });
        let events: Rc<RefCell<usize>> = Rc::default();
        let sink = events.clone();
        cx.update(|_, cx| {
            cx.subscribe(&picker, move |_, _, _| *sink.borrow_mut() += 1).detach();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-a");
        cx.simulate_input("new note name");
        cx.run_until_parked();
        cx.update(|_, cx| assert_eq!(picker.read(cx).rows(), 0));
        cx.simulate_keystrokes("enter");
        cx.simulate_keystrokes("cmd-enter");
        assert_eq!(*events.borrow(), 0);
    }

    #[gpui_kit::test]
    fn command_keyboard_navigation_scrolls_and_wraps(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::actions::init(cx);
        });
        let (picker, cx) = cx.add_window_view(|window, cx| {
            let mut picker = QuickOpen::new(crate::actions::commands(), vec![], window, cx);
            picker.show_commands(window, cx);
            picker
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let count = cx.update(|_, cx| picker.read(cx).rows());
        assert!(count > 12, "all commands remain reachable");
        cx.simulate_keystrokes("up");
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|_, cx| {
            let p = picker.read(cx);
            assert_eq!(p.selected, count - 1);
            assert!(p.scroll.offset().y < gpui_kit::px(0.));
        });
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|_, cx| {
            let p = picker.read(cx);
            assert_eq!(p.selected, 0);
            assert!(p.scroll.offset().y >= gpui_kit::px(-4.), "first row is visible below the 4px padding");
        });
    }

    #[gpui_kit::test]
    fn command_prefix_never_creates_notes(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::actions::init(cx);
        });
        let (picker, cx) =
            cx.add_window_view(|window, cx| QuickOpen::new(crate::actions::commands(), vec![], window, cx));
        let events: Rc<RefCell<usize>> = Rc::default();
        let sink = events.clone();
        cx.update(|_, cx| {
            cx.subscribe(&picker, move |_, _, _| *sink.borrow_mut() += 1).detach();
        });
        cx.run_until_parked();
        cx.simulate_input("> definitely-no-command");
        cx.update(|_, cx| assert_eq!(picker.read(cx).rows(), 0, "command mode has no create row"));
        cx.simulate_keystrokes("enter");
        cx.simulate_keystrokes("cmd-enter");
        assert_eq!(*events.borrow(), 0);
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
        let (picker, cx) = cx.add_window_view(|window, cx| {
            let mut picker = QuickOpen::new(vec![], vec![], window, cx);
            picker.set_notes(vault, service.clone(), recent, cx);
            picker
        });
        let events: Rc<RefCell<Vec<String>>> = Rc::default();
        let sink = events.clone();
        cx.update(|_, cx| {
            cx.subscribe(&picker, move |_, ev: &QuickOpenEvent, _| {
                sink.borrow_mut().push(match ev {
                    QuickOpenEvent::Open(p) => format!("open {}", p.as_str()),
                    QuickOpenEvent::Create(n) => format!("create {n}"),
                    QuickOpenEvent::Execute { id, .. } => format!("execute {id}"),
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
        cx.update(|_, cx| service.read(cx).close_blocking());
    }
}
