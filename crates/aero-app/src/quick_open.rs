//! 快速打开（`ui_design.md` 3.2）：居中弹出的模糊搜索框，范围是笔记标题与路径。
//!
//! 空输入显示最近打开的笔记；`⏎` 打开，`⌘⏎` 或无匹配时新建同名笔记；`Esc` 关闭。
//! 复用当前目录缓存，后台读取标题与别名；大目录的模糊匹配也在后台完成。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

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
const SYNC_CATALOG_LIMIT: usize = 256;
const FILTER_DEBOUNCE: Duration = Duration::from_millis(100);

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

use aero_core::links::NoteSummary;

struct PendingConfirmation {
    query: String,
    filter_generation: u64,
    catalog_generation: u64,
    create: bool,
    selection: Option<VaultPath>,
}

pub struct QuickOpen {
    input: Entity<InputState>,
    candidates: Arc<Vec<NoteSummary>>,
    prepared: Arc<crate::matching::Notes>,
    prepared_for: Option<Arc<Vec<NoteSummary>>>,
    filter_cancel: Option<crate::matching::Cancel>,
    catalog_vault: Option<String>,
    /// 丢弃上一次 set_notes 尚未完成的目录结果。
    catalog_generation: u64,
    catalog_status: Option<Result<(), String>>,
    filter_generation: u64,
    filter_task: Option<Task<()>>,
    filter_query: String,
    filtering: bool,
    pending_selection: Option<VaultPath>,
    pending_confirmation: Option<PendingConfirmation>,
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
            candidates: Arc::default(),
            prepared: Arc::new(crate::matching::Notes::new(&[])),
            prepared_for: None,
            filter_cancel: None,
            catalog_vault: None,
            catalog_generation: 0,
            catalog_status: None,
            filter_generation: 0,
            filter_task: None,
            filter_query: String::new(),
            filtering: false,
            pending_selection: None,
            pending_confirmation: None,
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
        if self.catalog_vault.as_deref() != Some(vault.id()) {
            self.candidates = Arc::default();
            self.shown.clear();
            self.pending_selection = None;
        }
        self.catalog_vault = Some(vault.id().to_owned());
        self.recent = recent;
        self.has_vault = true;
        self.catalog_status = None;
        self.catalog_generation += 1;
        self.refilter(cx);
        let generation = self.catalog_generation;
        let notes = service.read(cx).catalog_snapshot_for(vault, cx);
        cx.spawn(async move |this, cx| {
            let notes = notes.await.map(|snapshot| (snapshot.notes.clone(), snapshot.prepared.clone()));
            let _ = this.update(cx, |this, cx| this.apply_catalog(generation, notes, cx));
        })
        .detach();
    }

    #[cfg(test)]
    fn receive_catalog(
        &mut self,
        generation: u64,
        notes: Result<Vec<NoteSummary>, String>,
        cx: &mut Context<Self>,
    ) {
        self.apply_catalog(
            generation,
            notes.map(|notes| {
                let prepared = Arc::new(crate::matching::Notes::new(&notes));
                (Arc::new(notes), prepared)
            }),
            cx,
        );
    }

    fn apply_catalog(
        &mut self,
        generation: u64,
        notes: Result<(Arc<Vec<NoteSummary>>, Arc<crate::matching::Notes>), String>,
        cx: &mut Context<Self>,
    ) {
        if self.catalog_generation != generation {
            return;
        }
        let selection = self.selected_path();
        self.catalog_status = Some(notes.map(|(notes, prepared)| {
            self.prepared = prepared;
            self.prepared_for = Some(notes.clone());
            self.candidates = notes;
        }));
        if !self.command_mode {
            self.refilter_with_selection(selection, cx);
        } else {
            cx.notify();
        }
    }

    fn selected_path(&self) -> Option<VaultPath> {
        if self.command_mode {
            return None;
        }
        if self.filtering {
            return self.pending_selection.clone();
        }
        self.shown.get(self.selected).and_then(|&i| self.candidates.get(i)).map(|note| note.path.clone())
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
            InputEvent::Blur => {
                self.pending_confirmation = None;
                cx.emit(QuickOpenEvent::Dismiss);
            }
        }
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        self.refilter_with_selection(self.selected_path(), cx);
    }

    fn refilter_with_selection(&mut self, selection: Option<VaultPath>, cx: &mut Context<Self>) {
        if self.prepared_for.as_ref().is_none_or(|notes| !Arc::ptr_eq(notes, &self.candidates)) {
            self.prepared = Arc::new(crate::matching::Notes::new(&self.candidates));
            self.prepared_for = Some(self.candidates.clone());
        }
        let q = self.query(cx);
        let selection = selection.filter(|_| self.filter_query == q);
        self.filter_query = q.clone();
        self.filter_generation += 1;
        let generation = self.filter_generation;
        self.pending_confirmation = self
            .pending_confirmation
            .take()
            .filter(|pending| pending.query == q && pending.catalog_generation == self.catalog_generation);
        if let Some(pending) = self.pending_confirmation.as_mut() {
            // 同一查询的目录完成会重新筛选；确认跟随这次筛选，不能接受旧结果。
            pending.filter_generation = generation;
        }
        self.filter_cancel = None;
        self.filter_task = None;
        self.filtering = false;
        self.pending_selection = selection.clone();
        self.command_mode = q.starts_with('>');
        self.has_query = false;
        self.shown.clear();
        self.selected = 0;
        self.scroll.scroll_to_item(0);
        if let Some(query) = q.strip_prefix('>') {
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
            self.shown = scored.into_iter().map(|(i, _, _)| i).collect();
        } else if self.candidates.len() <= SYNC_CATALOG_LIMIT {
            let shown =
                filter_prepared(&self.candidates, &self.prepared, &self.recent, &q, &AtomicBool::new(false));
            self.receive_filter(generation, shown, selection, cx);
            return;
        } else {
            self.filtering = true;
            let candidates = self.candidates.clone();
            let prepared = self.prepared.clone();
            let cancel = crate::matching::Cancel::default();
            let cancelled = cancel.0.clone();
            self.filter_cancel = Some(cancel);
            let recent = self.recent.clone();
            self.filter_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(FILTER_DEBOUNCE).await;
                let shown = cx
                    .background_spawn(async move {
                        filter_prepared(&candidates, &prepared, &recent, &q, &cancelled)
                    })
                    .await;
                let _ = this.update(cx, |this, cx| this.receive_filter(generation, shown, selection, cx));
            }));
        }
        cx.notify();
    }

    fn receive_filter(
        &mut self,
        generation: u64,
        shown: Vec<usize>,
        selection: Option<VaultPath>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.filter_generation {
            return;
        }
        self.filter_task = None;
        self.filtering = false;
        self.pending_selection = None;
        self.shown = shown;
        self.has_query = self.has_vault
            && !self.command_mode
            && !self.filter_query.is_empty()
            && matches!(self.catalog_status, Some(Ok(())));
        self.selected = selection
            .and_then(|path| self.shown.iter().position(|&i| self.candidates[i].path == path))
            .unwrap_or(0);
        self.scroll.scroll_to_item(self.selected);
        self.replay_pending_confirmation(cx);
        cx.notify();
    }

    fn replay_pending_confirmation(&mut self, cx: &mut Context<Self>) {
        if self.catalog_status.is_none() {
            return;
        }
        let Some(pending) = self.pending_confirmation.take().filter(|pending| {
            pending.filter_generation == self.filter_generation
                && pending.catalog_generation == self.catalog_generation
                && pending.query == self.query(cx)
                && matches!(self.catalog_status, Some(Ok(())))
        }) else {
            return;
        };
        if !pending.create
            && let Some(path) = pending.selection
        {
            let Some(selected) = self.shown.iter().position(|&i| self.candidates[i].path == path) else {
                return;
            };
            self.selected = selected;
        }
        self.confirm(pending.create, cx);
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
        self.pending_confirmation = None;
        cx.emit(QuickOpenEvent::Dismiss);
    }

    /// 可选行数：候选 + （有输入时）"新建"一行。
    fn rows(&self) -> usize {
        self.shown.len() + usize::from(self.has_query)
    }

    fn confirm(&mut self, create: bool, cx: &mut Context<Self>) {
        let q = self.query(cx);
        if q != self.filter_query {
            self.refilter(cx);
        }
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
        if self.filtering || self.catalog_status.is_none() {
            self.pending_confirmation = Some(PendingConfirmation {
                query: q,
                filter_generation: self.filter_generation,
                catalog_generation: self.catalog_generation,
                create,
                selection: self.selected_path(),
            });
            return;
        }
        self.pending_confirmation = None;
        if create || self.shown.is_empty() || self.selected >= self.shown.len() {
            if self.has_query {
                cx.emit(QuickOpenEvent::Create(q));
            }
            return;
        }
        let path = self.candidates[self.shown[self.selected]].path.clone();
        cx.emit(QuickOpenEvent::Open(path));
    }
}

/// 只保留最好的 12 项，不为整个目录分配排序数组。大目录在后台调用。
#[cfg(test)]
fn filter_notes(candidates: &[NoteSummary], recent: &[VaultPath], query: &str) -> Vec<usize> {
    filter_prepared(
        candidates,
        &crate::matching::Notes::new(candidates),
        recent,
        query,
        &AtomicBool::new(false),
    )
}

fn filter_prepared(
    candidates: &[NoteSummary],
    prepared: &crate::matching::Notes,
    recent: &[VaultPath],
    query: &str,
    cancelled: &AtomicBool,
) -> Vec<usize> {
    let mut recent_positions = HashMap::new();
    for (i, path) in recent.iter().enumerate() {
        recent_positions.entry(path).or_insert(i);
    }
    if query.is_empty() {
        let mut recent_matches = vec![None; recent.len()];
        for (i, note) in candidates.iter().enumerate() {
            if let Some(&position) = recent_positions.get(&note.path) {
                recent_matches[position] = Some(i);
            }
        }
        let mut shown: Vec<_> = recent_matches.into_iter().flatten().take(RECENT_SHOWN).collect();
        for i in 0..candidates.len() {
            if shown.len() >= RECENT_SHOWN {
                break;
            }
            if !shown.contains(&i) {
                shown.push(i);
            }
        }
        return shown;
    }
    let query = crate::matching::prepare(query);
    crate::matching::top_k(candidates.len(), MAX_SHOWN, cancelled, |i| {
        let note = &candidates[i];
        let score = prepared.score(i, &query)?;
        let bonus = recent_positions.get(&note.path).map_or(0, |&p| 20 - p.min(10) as i32);
        Some((score + bonus, note.path.as_str()))
    })
}

/// 子序列匹配，命令列表等小候选集可直接调用。
pub fn fuzzy_score(query: &str, target: &str) -> Option<i32> {
    crate::matching::score(&crate::matching::prepare(query), &crate::matching::prepare(target))
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
                    .when(
                        self.has_vault
                            && !self.command_mode
                            && (self.filtering || !matches!(self.catalog_status, Some(Ok(())))),
                        |el| {
                            let message = match &self.catalog_status {
                                Some(Err(error)) => format!("无法读取笔记目录：{error}"),
                                Some(Ok(())) => "正在筛选笔记…".to_owned(),
                                None => "正在读取标题与别名…".to_owned(),
                            };
                            el.child(div().px_3().py_2().text_sm().text_color(muted).child(message))
                        },
                    )
                    .when(
                        empty
                            && !self.filtering
                            && (!self.has_vault
                                || self.command_mode
                                || matches!(self.catalog_status, Some(Ok(())))),
                        |el| {
                            el.child(div().px_3().py_2().text_sm().text_color(muted).child(
                                if self.command_mode {
                                    "没有匹配的可用命令。"
                                } else if !self.has_vault {
                                    "请先打开 Vault，或输入 > 搜索命令。"
                                } else {
                                    "这个 vault 里还没有笔记。"
                                },
                            ))
                        },
                    )
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

    fn large_picker(window: &mut gpui_kit::Window, cx: &mut gpui_kit::Context<QuickOpen>) -> QuickOpen {
        let mut picker = QuickOpen::new(vec![], vec![], window, cx);
        picker.has_vault = true;
        picker.catalog_status = Some(Ok(()));
        picker.candidates = (0..512)
            .map(|i| aero_core::links::NoteSummary {
                title: format!("note {i}"),
                path: VaultPath::new(&format!("{i:04}.md")).unwrap(),
                aliases: vec![],
            })
            .collect::<Vec<_>>()
            .into();
        picker.refilter(cx);
        picker
    }

    fn record_note_events(
        picker: &gpui_kit::Entity<QuickOpen>,
        visual: &mut gpui_kit::VisualTestContext,
    ) -> Rc<RefCell<Vec<String>>> {
        let events = Rc::new(RefCell::new(Vec::new()));
        let sink = events.clone();
        visual.update(|_, cx| {
            cx.subscribe(picker, move |_, event, _| match event {
                QuickOpenEvent::Open(path) => sink.borrow_mut().push(format!("open {path}")),
                QuickOpenEvent::Create(name) => sink.borrow_mut().push(format!("create {name}")),
                _ => {}
            })
            .detach();
        });
        events
    }

    fn finish_filter(visual: &mut gpui_kit::VisualTestContext) {
        visual.run_until_parked();
        visual.executor().advance_clock(super::FILTER_DEBOUNCE);
        visual.run_until_parked();
    }

    #[gpui_kit::test]
    fn enter_during_debounce_opens_intended_query_once(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::actions::init(cx);
        });
        let (picker, visual) = cx.add_window_view(large_picker);
        let events = record_note_events(&picker, visual);
        finish_filter(visual);
        visual.simulate_input("0511");
        assert!(picker.read_with(visual, |p, _| p.filtering));
        visual.simulate_keystrokes("enter");
        assert!(events.borrow().is_empty());
        finish_filter(visual);
        assert_eq!(*events.borrow(), ["open 0511.md"]);
        picker.update(visual, |p, cx| {
            p.receive_filter(p.filter_generation, p.shown.clone(), None, cx);
            p.refilter(cx);
        });
        finish_filter(visual);
        assert_eq!(*events.borrow(), ["open 0511.md"], "confirmation is consumed exactly once");
    }

    #[gpui_kit::test]
    fn cmd_enter_during_debounce_creates_once(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::actions::init(cx);
        });
        let (picker, visual) = cx.add_window_view(large_picker);
        let events = record_note_events(&picker, visual);
        visual.simulate_input("note");
        visual.simulate_keystrokes("cmd-enter");
        assert!(events.borrow().is_empty());
        finish_filter(visual);
        assert_eq!(*events.borrow(), ["create note"], "Cmd-Enter must create even with matches");
        picker.update(visual, |p, cx| p.receive_filter(p.filter_generation, p.shown.clone(), None, cx));
        finish_filter(visual);
        assert_eq!(*events.borrow(), ["create note"]);
    }

    #[gpui_kit::test]
    fn changed_query_cancels_pending_confirmation(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (picker, visual) = cx.add_window_view(large_picker);
        let events = record_note_events(&picker, visual);
        picker.update_in(visual, |p, window, cx| {
            p.input.update(cx, |s, cx| s.set_value("0511", window, cx));
            p.refilter(cx);
            p.confirm(false, cx);
            p.input.update(cx, |s, cx| s.set_value("0510", window, cx));
            p.refilter(cx);
        });
        finish_filter(visual);
        assert!(events.borrow().is_empty());
        picker.update_in(visual, |p, window, cx| {
            p.input.update(cx, |s, cx| s.set_value("0511", window, cx));
            p.refilter(cx);
        });
        finish_filter(visual);
        assert!(events.borrow().is_empty(), "returning to the old query must not resurrect Enter");
    }

    #[gpui_kit::test]
    fn stale_filter_cannot_replay_pending_confirmation(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (picker, visual) = cx.add_window_view(large_picker);
        let events = record_note_events(&picker, visual);
        picker.update_in(visual, |p, window, cx| {
            p.input.update(cx, |s, cx| s.set_value("0511", window, cx));
            p.refilter(cx);
            let obsolete = p.filter_generation;
            p.input.update(cx, |s, cx| s.set_value("0510", window, cx));
            p.refilter(cx);
            p.confirm(false, cx);
            p.receive_filter(obsolete, vec![usize::MAX], None, cx);
            assert!(p.filtering);
        });
        assert!(events.borrow().is_empty(), "stale results must not confirm the current query");
        finish_filter(visual);
        assert_eq!(*events.borrow(), ["open 0510.md"]);
    }

    #[gpui_kit::test]
    fn pending_confirmation_preserves_selected_path_across_same_query_refresh(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (picker, visual) = cx.add_window_view(large_picker);
        let events = record_note_events(&picker, visual);
        visual.simulate_input("note");
        finish_filter(visual);
        picker.update_in(visual, |p, window, cx| {
            p.on_select_next(&crate::actions::SelectNext, window, cx);
            assert_eq!(p.selected_path().unwrap().as_str(), "0001.md");
            p.refilter(cx);
            p.confirm(false, cx);
            let notes = p.candidates.iter().cloned().rev().collect();
            p.receive_catalog(p.catalog_generation, Ok(notes), cx);
        });
        finish_filter(visual);
        assert_eq!(*events.borrow(), ["open 0001.md"], "preserve the path, not its old row index");
    }

    #[gpui_kit::test]
    fn dismissal_cancels_pending_confirmation(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (picker, visual) = cx.add_window_view(large_picker);
        let events = record_note_events(&picker, visual);
        for blur in [false, true] {
            picker.update_in(visual, |p, window, cx| {
                p.input.update(cx, |s, cx| s.set_value("0511", window, cx));
                p.refilter(cx);
                p.confirm(false, cx);
                if blur {
                    p.on_input_event(&p.input.clone(), &gpui_kit::base::input::InputEvent::Blur, window, cx);
                } else {
                    p.on_dismiss(&crate::actions::Dismiss, window, cx);
                }
            });
            finish_filter(visual);
            assert!(events.borrow().is_empty(), "dismissal must cancel Enter (blur={blur})");
        }
    }

    #[gpui_kit::test]
    fn vault_switch_cancels_pending_confirmation(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("0511.md"), "other vault").unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let service = cx.new(|cx| SearchService::new(vault.clone(), data.path().into(), cx));
        cx.update(|cx| service.read(cx).close_blocking());
        let (picker, visual) = cx.add_window_view(large_picker);
        let events = record_note_events(&picker, visual);
        picker.update_in(visual, |p, window, cx| {
            p.input.update(cx, |s, cx| s.set_value("0511", window, cx));
            p.refilter(cx);
            p.confirm(false, cx);
            let obsolete = p.filter_generation;
            p.set_notes(vault, service, vec![], cx);
            p.receive_filter(obsolete, vec![usize::MAX], None, cx);
        });
        finish_filter(visual);
        assert_eq!(picker.read_with(visual, |p, _| p.shown_paths().len()), 1);
        assert!(events.borrow().is_empty(), "Enter must not carry over to the new vault");
    }

    #[gpui_kit::test]
    fn pending_confirmation_waits_for_catalog_before_creating(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (picker, visual) = cx.add_window_view(large_picker);
        let events = record_note_events(&picker, visual);
        picker.update_in(visual, |p, window, cx| {
            p.catalog_status = None;
            p.input.update(cx, |s, cx| s.set_value("missing", window, cx));
            p.refilter(cx);
            p.confirm(true, cx);
        });
        finish_filter(visual);
        assert!(events.borrow().is_empty(), "a partial catalog must never allow creation");
        picker.update(visual, |p, cx| p.receive_catalog(p.catalog_generation, Ok(p.candidates.to_vec()), cx));
        finish_filter(visual);
        assert_eq!(*events.borrow(), ["create missing"]);
    }

    #[gpui_kit::test]
    fn startup_catalog_loads_in_background_and_applies_pending_query(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("n.md"), "---\naliases: silver\n---\n").unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let service = cx.new(|cx| SearchService::new(vault.clone(), data.path().into(), cx));
        cx.update(|cx| service.read(cx).close_blocking());
        let (picker, visual) = cx.add_window_view(|window, cx| {
            let mut picker = QuickOpen::new(vec![], vec![], window, cx);
            picker.set_notes(vault, service, vec![], cx);
            assert!(picker.candidates.is_empty(), "set_notes must not enumerate the vault synchronously");
            assert!(picker.catalog_status.is_none());
            picker.input.update(cx, |s, cx| s.set_value("silver", window, cx));
            picker.refilter(cx);
            assert_eq!(picker.rows(), 0, "loading must not imply a missing note");
            picker
        });
        visual.run_until_parked();
        picker.read_with(visual, |picker, _| {
            assert_eq!(picker.catalog_status, Some(Ok(())));
            assert_eq!(picker.shown_paths(), ["n.md"]);
        });
    }

    #[gpui_kit::test]
    fn large_catalog_waits_for_latest_query(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (picker, visual) = cx.add_window_view(|window, cx| {
            let mut picker = QuickOpen::new(vec![], vec![], window, cx);
            picker.has_vault = true;
            picker.catalog_status = Some(Ok(()));
            picker.candidates = (0..2048)
                .map(|i| aero_core::links::NoteSummary {
                    title: format!("note {i}"),
                    path: VaultPath::new(&format!("{i:04}.md")).unwrap(),
                    aliases: vec![],
                })
                .collect::<Vec<_>>()
                .into();
            picker.input.update(cx, |s, cx| s.set_value("note", window, cx));
            picker.refilter(cx);
            assert_eq!(picker.rows(), 0, "large catalogs must defer scoring and creation");
            picker
        });
        let events = Rc::new(RefCell::new(0));
        visual.update(|_, cx| {
            let events = events.clone();
            cx.subscribe(&picker, move |_, event, _| {
                if matches!(event, QuickOpenEvent::Open(_) | QuickOpenEvent::Create(_)) {
                    *events.borrow_mut() += 1;
                }
            })
            .detach();
        });
        visual.run_until_parked();
        let obsolete = picker.read_with(visual, |p, _| p.filter_generation);
        picker.update_in(visual, |p, window, cx| {
            p.input.update(cx, |s, cx| s.set_value("2047", window, cx));
            p.refilter(cx);
            p.receive_filter(obsolete, vec![usize::MAX], None, cx);
            assert!(p.filtering, "obsolete results must not end the pending query");
            assert_eq!(p.rows(), 0);
            p.confirm(false, cx);
        });
        assert_eq!(*events.borrow(), 0, "Enter must wait for matching results");
        visual.run_until_parked();
        visual.executor().advance_clock(super::FILTER_DEBOUNCE - std::time::Duration::from_millis(1));
        visual.run_until_parked();
        picker.read_with(visual, |p, _| assert!(p.filtering, "scoring is debounced"));
        visual.executor().advance_clock(std::time::Duration::from_millis(1));
        visual.run_until_parked();
        picker.update_in(visual, |p, window, cx| {
            assert!(!p.filtering);
            assert_eq!(p.shown_paths(), ["2047.md"]);
            assert_eq!(p.selected(), 0);
            assert_eq!(p.rows(), 2);
            p.on_select_next(&crate::actions::SelectNext, window, cx);
            assert_eq!(p.selected(), 1);
            p.on_select_next(&crate::actions::SelectNext, window, cx);
            assert_eq!(p.selected(), 0);
            p.receive_filter(obsolete, vec![usize::MAX], None, cx);
            assert_eq!(p.shown_paths(), ["2047.md"]);
        });
        assert_eq!(*events.borrow(), 1, "the queued Enter must run without a second keypress");
    }

    #[test]
    fn bounded_top_matches_full_ranking_with_aliases_recent_bonus_and_ties() {
        let candidates: Vec<_> = (0..384)
            .rev()
            .map(|i| aero_core::links::NoteSummary {
                title: format!("Note {}", i % 5),
                path: VaultPath::new(&format!("dir/{i:04}.md")).unwrap(),
                aliases: vec![format!("雪 山 {}", i % 7)],
            })
            .collect();
        let recent = vec![candidates[94].path.clone(), candidates[53].path.clone()];
        for query in ["Note", "n1", "雪山", "23", "unmatched"] {
            let mut expected: Vec<_> = candidates
                .iter()
                .enumerate()
                .filter_map(|(i, note)| {
                    let score =
                        fuzzy_score(query, &note.title)
                            .max(fuzzy_score(query, note.path.as_str()))
                            .max(note.aliases.iter().filter_map(|alias| fuzzy_score(query, alias)).max())?;
                    let bonus =
                        recent.iter().position(|p| p == &note.path).map_or(0, |p| 20 - p.min(10) as i32);
                    Some((score + bonus, i))
                })
                .collect();
            expected
                .sort_by(|a, b| b.0.cmp(&a.0).then_with(|| candidates[a.1].path.cmp(&candidates[b.1].path)));
            let expected: Vec<_> = expected.into_iter().take(super::MAX_SHOWN).map(|(_, i)| i).collect();
            assert_eq!(super::filter_notes(&candidates, &recent, query), expected, "query: {query}");
        }
        let shown = super::filter_notes(&candidates, &recent, "");
        assert_eq!(&shown[..2], &[94, 53]);
        assert_eq!(shown.len(), super::RECENT_SHOWN);
        assert_eq!(shown.iter().collect::<std::collections::HashSet<_>>().len(), shown.len());
    }

    #[gpui_kit::test]
    fn cached_catalog_refresh_preserves_keyboard_selection_by_path(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        for name in ["b.md", "c.md"] {
            std::fs::write(dir.path().join(name), "body").unwrap();
        }
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let service = cx.new(|cx| SearchService::new(vault.clone(), data.path().into(), cx));
        cx.update(|cx| service.read(cx).close_blocking());
        let (picker, visual) = cx.add_window_view(|window, cx| {
            let mut picker = QuickOpen::new(vec![], vec![], window, cx);
            picker.set_notes(vault.clone(), service.clone(), vec![], cx);
            picker
        });
        visual.run_until_parked();
        std::fs::write(dir.path().join("a.md"), "new").unwrap();
        service.update(visual, |s, _| s.catalog.touch([VaultPath::new("a.md").unwrap()]));
        picker.update_in(visual, |p, window, cx| {
            assert_eq!(p.shown_paths(), ["b.md", "c.md"]);
            p.on_select_next(&crate::actions::SelectNext, window, cx);
            let cached = p.candidates.clone();
            p.set_notes(vault, service, vec![], cx);
            assert!(Arc::ptr_eq(&cached, &p.candidates), "reuse the current view's catalog until refreshed");
            assert_eq!(p.shown_paths(), ["b.md", "c.md"]);
            assert_eq!(p.selected_path().unwrap().as_str(), "c.md");
        });
        visual.run_until_parked();
        picker.read_with(visual, |p, _| {
            assert_eq!(p.catalog_status, Some(Ok(())));
            assert_eq!(p.shown_paths(), ["a.md", "b.md", "c.md"]);
            assert_eq!(p.selected(), 2);
            assert_eq!(p.selected_path().unwrap().as_str(), "c.md");
        });
    }

    #[gpui_kit::test]
    fn catalog_completion_preserves_selection_during_a_debounced_refresh(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (picker, visual) = cx.add_window_view(|window, cx| {
            let mut picker = QuickOpen::new(vec![], vec![], window, cx);
            picker.has_vault = true;
            let notes = (0..512)
                .map(|i| aero_core::links::NoteSummary {
                    title: format!("note {i}"),
                    path: VaultPath::new(&format!("{i:04}.md")).unwrap(),
                    aliases: vec![],
                })
                .collect();
            picker.receive_catalog(picker.catalog_generation, Ok(notes), cx);
            picker
        });
        visual.run_until_parked();
        visual.executor().advance_clock(super::FILTER_DEBOUNCE);
        visual.run_until_parked();
        picker.update_in(visual, |p, window, cx| {
            assert_eq!(p.shown.len(), super::RECENT_SHOWN);
            p.on_select_next(&crate::actions::SelectNext, window, cx);
            assert_eq!(p.selected_path().unwrap().as_str(), "0001.md");
            p.catalog_status = None;
            p.refilter(cx);
            assert!(p.filtering);
            p.receive_catalog(p.catalog_generation, Ok(p.candidates.to_vec()), cx);
        });
        visual.run_until_parked();
        visual.executor().advance_clock(super::FILTER_DEBOUNCE);
        visual.run_until_parked();
        picker.read_with(visual, |p, _| {
            assert_eq!(p.selected_path().unwrap().as_str(), "0001.md");
            assert_eq!(p.selected(), 1);
        });
    }

    #[gpui_kit::test]
    fn stale_catalog_is_ignored_after_switching_vaults(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let old_dir = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(old_dir.path().join("old.md"), "old").unwrap();
        std::fs::write(dir.path().join("new.md"), "---\naliases: silver\n---\n").unwrap();
        let old_vault = Arc::new(Vault::open(old_dir.path()).unwrap());
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let service = cx.new(|cx| SearchService::new(old_vault.clone(), data.path().into(), cx));
        cx.update(|cx| service.read(cx).close_blocking());
        let (picker, visual) = cx.add_window_view(|window, cx| {
            let mut picker = QuickOpen::new(vec![], vec![], window, cx);
            picker.set_notes(old_vault, service.clone(), vec![], cx);
            let obsolete = picker.catalog_generation;
            picker.set_notes(vault, service, vec![], cx);
            picker.receive_catalog(obsolete, Err("obsolete error".into()), cx);
            assert_eq!(picker.catalog_status, None);
            picker.receive_catalog(
                obsolete,
                Ok(vec![aero_core::links::NoteSummary {
                    title: "old".into(),
                    path: VaultPath::new("old.md").unwrap(),
                    aliases: vec![],
                }]),
                cx,
            );
            assert!(picker.candidates.is_empty());
            picker.input.update(cx, |s, cx| s.set_value("silver", window, cx));
            picker.refilter(cx);
            picker
        });
        visual.run_until_parked();
        picker.read_with(visual, |p, _| {
            assert_eq!(p.catalog_status, Some(Ok(())));
            assert_eq!(p.shown_paths(), ["new.md"]);
        });
    }

    #[gpui_kit::test]
    fn switching_to_commands_invalidates_pending_note_filter(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (picker, visual) = cx.add_window_view(|window, cx| {
            let mut picker = QuickOpen::new(crate::actions::commands(), vec![], window, cx);
            picker.has_vault = true;
            picker.catalog_status = Some(Ok(()));
            picker.candidates = Arc::new(
                (0..512)
                    .map(|i| aero_core::links::NoteSummary {
                        title: format!("note {i}"),
                        path: VaultPath::new(&format!("{i}.md")).unwrap(),
                        aliases: vec![],
                    })
                    .collect(),
            );
            picker.input.update(cx, |s, cx| s.set_value("note", window, cx));
            picker.refilter(cx);
            let obsolete = picker.filter_generation;
            picker.show_commands(window, cx);
            picker.receive_filter(obsolete, vec![usize::MAX], None, cx);
            assert!(picker.command_mode);
            assert!(!picker.filtering);
            assert!(!picker.has_query);
            assert_eq!(picker.shown.len(), picker.commands.len());
            picker
        });
        visual.run_until_parked();
        visual.executor().advance_clock(super::FILTER_DEBOUNCE);
        visual.run_until_parked();
        picker.read_with(visual, |p, _| {
            assert!(p.command_mode);
            assert_eq!(p.shown.len(), p.commands.len());
        });
    }

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
    fn front_matter_alias_filters_without_changing_display_title(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::actions::init(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("n.md"),
            "---\ntitle: Display\naliases: [quicksilver, '雪 山']\n---\n",
        )
        .unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let service = cx.new(|cx| SearchService::new(vault.clone(), data.path().into(), cx));
        // With no engine available, the async disk catalog must still provide aliases.
        cx.update(|cx| service.read(cx).close_blocking());
        let (picker, cx) = cx.add_window_view(|window, cx| {
            let mut picker = QuickOpen::new(vec![], vec![], window, cx);
            picker.set_notes(vault, service, vec![], cx);
            picker
        });
        cx.run_until_parked();
        cx.simulate_input("quicksilver");
        cx.update(|_, cx| {
            assert_eq!(picker.read(cx).shown_paths(), ["n.md"]);
            assert_eq!(picker.read(cx).candidates[0].title, "Display");
        });
        picker.update_in(cx, |p, window, cx| {
            p.input.update(cx, |s, cx| s.set_value("雪山", window, cx));
            p.refilter(cx);
        });
        cx.update(|_, cx| assert_eq!(picker.read(cx).shown_paths(), ["n.md"]));
    }

    #[gpui_kit::test]
    fn pending_alias_catalog_never_creates_from_an_empty_match(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("n.md"), "---\naliases: shared-name\n---\nbody\n").unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let service = cx.new(|cx| SearchService::new(vault.clone(), data.path().into(), cx));
        cx.update(|cx| service.read(cx).close_blocking());
        let (picker, visual) = cx.add_window_view(|window, cx| QuickOpen::new(vec![], vec![], window, cx));
        let created = Rc::new(RefCell::new(Vec::new()));
        visual.update(|window, cx| {
            let sink = created.clone();
            cx.subscribe(&picker, move |_, event, _| {
                if let QuickOpenEvent::Create(name) = event {
                    sink.borrow_mut().push(name.clone());
                }
            })
            .detach();
            picker.update(cx, |p, cx| {
                p.set_notes(vault, service, vec![], cx);
                p.input.update(cx, |s, cx| s.set_value("shared-name", window, cx));
                p.refilter(cx);
                p.confirm(false, cx);
            });
        });
        visual.run_until_parked();
        assert!(created.borrow().is_empty(), "partial catalog must not imply a missing note");
        assert_eq!(
            picker.read_with(visual, |p, _| p
                .shown_paths()
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()),
            ["n.md"]
        );
    }

    #[gpui_kit::test]
    fn failed_alias_catalog_does_not_offer_creation(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let service = cx.new(|cx| SearchService::new(vault.clone(), data.path().into(), cx));
        cx.update(|cx| service.read(cx).close_blocking());
        dir.close().unwrap();
        let (picker, visual) = cx.add_window_view(|window, cx| {
            let mut picker = QuickOpen::new(vec![], vec![], window, cx);
            picker.set_notes(vault, service, vec![], cx);
            picker
        });
        visual.run_until_parked();
        picker.update_in(visual, |p, window, cx| {
            p.input.update(cx, |s, cx| s.set_value("missing", window, cx));
            p.refilter(cx);
            assert_eq!(p.rows(), 0, "catalog failure is not an empty vault");
        });
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
