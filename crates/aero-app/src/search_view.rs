//! 左侧栏的搜索视图（`ui_design.md` 4.2，M1b 子集）。
//!
//! 输入 150 ms 防抖后查全文索引，结果按笔记分组、块级命中、关键词高亮；
//! `↑↓` 在命中间移动并让编辑区定位到该块，`⏎` 打开，`Esc` 回到编辑器。
//! `#tag` / `tag:tag` 按笔记级标签过滤（父标签含子标签），`-#tag` 排除；可选本地语义异步补充，不阻塞全文。

use std::ops::Range;
use std::time::Duration;

use aero_core::{FtsQuery, VaultPath};
use aero_index::SearchHit;
use gpui_kit::assets::IconName;
use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Icon, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::actions::{Dismiss, SelectNext, SelectPrev};
use crate::search::{Phase, SearchService};
use crate::semantic::{ModelStatus, SearchPreferences, SemanticPhase, SemanticService, SemanticStatus};

const DEBOUNCE: Duration = Duration::from_millis(150);
const LIMIT: usize = 200;
/// 摘要里命中词前后各留多少字符
const SNIPPET_CONTEXT: usize = 36;
const SNIPPET_MAX: usize = 110;

pub enum SearchViewEvent {
    /// 定位到某篇笔记的第 `block_ord` 个可索引块；`preview` 是 ↑↓ 预览（放进预览标签页），不是 ⏎ / 点击
    Open { path: VaultPath, block_ord: u32, preview: bool },
    /// Esc：把焦点交还给编辑器
    Dismiss,
    /// 由 Workspace 打开搜索与隐私设置。
    Settings,
}

impl EventEmitter<SearchViewEvent> for SearchView {}

struct Group {
    path: VaultPath,
    title: String,
    /// `hits` 里的下标
    hits: Vec<usize>,
}

/// 两个独立的响应通道：语义先完成时暂存，全文先完成时立刻展示。
#[derive(Default)]
struct SearchResults {
    fulltext: Option<Vec<SearchHit>>,
    semantic: Option<Vec<SearchHit>>,
    semantic_pending: bool,
}

impl SearchResults {
    fn visible(&self) -> Option<Vec<SearchHit>> {
        let fulltext = self.fulltext.as_ref()?;
        Some(match &self.semantic {
            Some(semantic) => aero_index::semantic::fuse_results(fulltext, semantic, LIMIT),
            None => fulltext.clone(),
        })
    }
}

/// 只追踪影响查询的变化；下载字节数、逐篇索引进度不触发重复推理。
#[derive(Default, PartialEq, Eq)]
struct SemanticRefreshKey {
    enabled: bool,
    model_ready: bool,
    completed: Option<(usize, usize, u64)>,
    scope_revision: u64,
}

impl SemanticRefreshKey {
    fn new(prefs: &SearchPreferences, status: &SemanticStatus) -> Self {
        Self {
            enabled: prefs.semantic_enabled,
            model_ready: matches!(status.model, ModelStatus::Ready | ModelStatus::Loading),
            scope_revision: status.scope_revision,
            completed: match status.phase {
                SemanticPhase::Ready { notes, chunks } => Some((notes, chunks, status.revision)),
                _ => None,
            },
        }
    }

    fn needs_refresh(&self, next: &Self) -> bool {
        self.enabled != next.enabled
            || self.scope_revision != next.scope_revision
            || self.model_ready != next.model_ready
            || (next.completed.is_some() && self.completed != next.completed)
    }
}

pub struct SearchView {
    service: Entity<SearchService>,
    semantic_service: Option<Entity<SemanticService>>,
    semantic_key: SemanticRefreshKey,
    hybrid: bool,
    results: SearchResults,
    /// 保留包装任务的所有权；丢弃时连带取消 SemanticService::query。
    semantic_task: Option<Task<()>>,
    semantic_error: Option<String>,
    _semantic_subscription: Option<Subscription>,
    input: Entity<InputState>,
    /// 最近一次真正发出的查询
    query: String,
    hits: Vec<SearchHit>,
    groups: Vec<Group>,
    selected: Option<usize>,
    /// 递增代号，丢弃过期结果
    generation: u64,
    debounce: Option<Task<()>>,
    searching: bool,
    error: Option<String>,
    was_ready: bool,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl SearchView {
    pub fn new(service: Entity<SearchService>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("搜索全文…  #标签 -#排除"));
        let input_sub = cx.subscribe_in(&input, window, Self::on_input_event);
        // 索引刚建好时把当前查询再跑一遍——用户可能在索引完成前就开始输入了
        let service_sub = cx.observe(&service, |this, service, cx| {
            let ready = matches!(service.read(cx).phase(), Phase::Idle);
            if ready && !this.was_ready && this.debounce.is_none() && !this.query.is_empty() {
                this.run_search(cx);
            }
            this.was_ready = ready;
            cx.notify();
        });
        let was_ready = matches!(service.read(cx).phase(), Phase::Idle);
        SearchView {
            service,
            semantic_service: None,
            semantic_key: SemanticRefreshKey::default(),
            hybrid: false,
            results: SearchResults::default(),
            semantic_task: None,
            semantic_error: None,
            _semantic_subscription: None,
            input,
            query: String::new(),
            hits: Vec::new(),
            groups: Vec::new(),
            selected: None,
            generation: 0,
            debounce: None,
            searching: false,
            error: None,
            was_ready,
            scroll: ScrollHandle::new(),
            _subscriptions: vec![input_sub, service_sub],
        }
    }

    /// 单独接线，保留全文视图原有构造函数（包括无模型测试）。
    pub fn connect_semantic(&mut self, service: Entity<SemanticService>, cx: &mut Context<Self>) {
        let semantic = service.read(cx);
        self.semantic_key = SemanticRefreshKey::new(semantic.preferences(), &semantic.status());
        self.hybrid = self.semantic_key.enabled;
        self._semantic_subscription = Some(cx.observe(&service, |this, service, cx| {
            let semantic = service.read(cx);
            let prefs = semantic.preferences().clone();
            let status = semantic.status();
            this.semantic_changed(&prefs, &status, cx);
        }));
        self.semantic_service = Some(service);
        if self.debounce.is_none() && !self.query.is_empty() {
            self.run_search(cx);
        }
        cx.notify();
    }

    fn semantic_changed(
        &mut self,
        prefs: &SearchPreferences,
        status: &SemanticStatus,
        cx: &mut Context<Self>,
    ) {
        let mut next = SemanticRefreshKey::new(prefs, status);
        // 输入活动会使 Ready 暂时变成 WaitingForIdle；保留上一次完成的索引身份。
        next.completed = next.completed.or(self.semantic_key.completed);
        if !next.enabled || !next.model_ready {
            self.semantic_task = None;
            self.results.semantic_pending = false;
        }
        let refresh = self.semantic_key.needs_refresh(&next)
            && (self.hybrid || self.semantic_key.enabled != next.enabled);
        if self.semantic_key.enabled != next.enabled {
            self.hybrid = next.enabled;
        }
        self.semantic_key = next;
        // 不绕过正在等待的输入防抖，也不因每次进度通知而重复搜索。
        if refresh && self.debounce.is_none() && !self.query.is_empty() {
            self.run_search(cx);
        }
        cx.notify();
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |s, cx| {
            s.focus(window, cx);
            s.select_all(window, cx);
        });
    }

    /// 标签树点进来：立刻搜，不等 150 ms 防抖。
    pub fn search_tag(&mut self, tag: &str, window: &mut Window, cx: &mut Context<Self>) {
        let query = format!("tag:#{tag}");
        self.input.update(cx, |s, cx| s.set_value(&query, window, cx));
        self.debounce = None;
        self.run_search(cx);
        self.focus(window, cx);
    }

    /// 最近一次真正发出的查询。
    #[cfg(test)]
    pub fn query(&self) -> &str {
        &self.query
    }

    #[cfg(test)]
    pub fn hits(&self) -> &[SearchHit] {
        &self.hits
    }

    #[cfg(test)]
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    fn on_input_event(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => self.schedule(cx),
            InputEvent::PressEnter { .. } => self.open_selected(cx),
            InputEvent::Focus | InputEvent::Blur => {}
        }
    }

    /// 防抖：停止输入 150 ms 后才真正查询。
    fn schedule(&mut self, cx: &mut Context<Self>) {
        // 输入一变就丢弃两个通道的在途结果，不等下一次防抖真正发出查询。
        self.semantic_task = None;
        self.generation += 1;
        self.debounce = None;
        self.results = SearchResults::default();
        self.error = None;
        self.semantic_error = None;
        if let Some(service) = &self.semantic_service {
            service.read(cx).touch_activity();
        }
        let text = self.input.read(cx).value().to_string();
        self.searching = !text.trim().is_empty();
        self.set_results(Vec::new(), cx);
        if !self.searching {
            self.query.clear();
            return;
        }
        let generation = self.generation;
        self.debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                if this.generation == generation {
                    this.debounce = None;
                    this.run_search(cx);
                }
            });
        }));
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        self.semantic_task = None;
        self.debounce = None;
        self.generation += 1;
        self.results = SearchResults::default();
        self.error = None;
        self.semantic_error = None;
        let text = self.input.read(cx).value().trim().to_string();
        self.searching = !text.is_empty();
        if !self.searching {
            self.query.clear();
            self.set_results(Vec::new(), cx);
            return;
        }
        if self.query != text {
            self.selected = None;
        }
        self.query = text.clone();
        let generation = self.generation;
        // 先发全文，再独立发语义；不要 join 或等待模型加载。
        let operation = crate::diagnostics::Operation::new(
            "search",
            crate::diagnostics::Facts { epoch: generation, bytes: text.len() as u64, ..Default::default() },
        );
        let task = operation.scope(|| self.service.read(cx).search(text.clone(), LIMIT, cx));
        let semantic_task = self.semantic_service.as_ref().and_then(|service| {
            (self.hybrid
                && self.semantic_key.enabled
                && self.semantic_key.model_ready
                && !FtsQuery::parse(&text).text.trim().is_empty())
            .then(|| service.read(cx).query(text, LIMIT, cx))
        });
        self.results = SearchResults { semantic_pending: semantic_task.is_some(), ..Default::default() };
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                operation.stage(
                    "ui_apply",
                    crate::diagnostics::Facts {
                        count: result.as_ref().map_or(0, |hits| hits.len()) as u64,
                        ..Default::default()
                    },
                );
                operation.finish(if this.generation != generation {
                    crate::diagnostics::Outcome::Stale
                } else if result.is_ok() {
                    crate::diagnostics::Outcome::Ok
                } else {
                    crate::diagnostics::Outcome::Failed
                });
                operation.scope(|| this.receive_fulltext(generation, result, cx));
            });
        })
        .detach();
        if let Some(task) = semantic_task {
            self.await_semantic(generation, task, cx);
        }
        cx.notify();
    }

    fn await_semantic(
        &mut self,
        generation: u64,
        task: Task<Result<Vec<SearchHit>, String>>,
        cx: &mut Context<Self>,
    ) {
        let operation = crate::diagnostics::Operation::new(
            "semantic_search",
            crate::diagnostics::Facts { epoch: generation, ..Default::default() },
        );
        self.semantic_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                operation.finish(if this.generation != generation {
                    crate::diagnostics::Outcome::Stale
                } else if result.is_ok() {
                    crate::diagnostics::Outcome::Ok
                } else {
                    crate::diagnostics::Outcome::Failed
                });
                operation.scope(|| this.receive_semantic(generation, result, cx));
            });
        }));
    }

    fn receive_fulltext(
        &mut self,
        generation: u64,
        result: Result<Vec<SearchHit>, String>,
        cx: &mut Context<Self>,
    ) {
        if self.generation != generation {
            return;
        }
        self.searching = false;
        self.results.fulltext = Some(match result {
            Ok(hits) => hits,
            Err(error) => {
                self.error = Some(error);
                Vec::new()
            }
        });
        if let Some(hits) = self.results.visible() {
            self.set_results(hits, cx);
        }
    }

    fn receive_semantic(
        &mut self,
        generation: u64,
        result: Result<Vec<SearchHit>, String>,
        cx: &mut Context<Self>,
    ) {
        if self.generation != generation || !self.results.semantic_pending {
            return;
        }
        self.semantic_task = None;
        self.results.semantic_pending = false;
        match result {
            Ok(hits) => self.results.semantic = Some(hits),
            Err(error) => self.semantic_error = Some(error),
        }
        if let Some(hits) = self.results.visible() {
            self.set_results(hits, cx);
        }
        cx.notify();
    }

    fn set_results(&mut self, hits: Vec<SearchHit>, cx: &mut Context<Self>) {
        let mut groups: Vec<Group> = Vec::new();
        for (i, h) in hits.iter().enumerate() {
            match groups.iter_mut().find(|g| g.path == h.path) {
                Some(g) => g.hits.push(i),
                None => groups.push(Group { path: h.path.clone(), title: h.title.clone(), hits: vec![i] }),
            }
        }
        // 晚到的语义会重排结果，但不改变用户用 ↑↓ 选中的块。
        self.selected = self.selected.and_then(|ix| self.hits.get(ix)).and_then(|previous| {
            hits.iter().position(|hit| hit.path == previous.path && hit.block_ord == previous.block_ord)
        });
        self.hits = hits;
        self.groups = groups;
        cx.notify();
    }

    /// 视觉顺序（按分组）的命中下标序列。
    fn visual_order(&self) -> Vec<usize> {
        self.groups.iter().flat_map(|g| g.hits.iter().copied()).collect()
    }

    fn select_step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let order = self.visual_order();
        if order.is_empty() {
            return;
        }
        let pos = self.selected.and_then(|s| order.iter().position(|&i| i == s));
        let next = match pos {
            None if delta > 0 => 0,
            None => order.len() - 1,
            Some(p) => (p as isize + delta).rem_euclid(order.len() as isize) as usize,
        };
        self.selected = Some(order[next]);
        // 预览定位（4.2：↑↓ 在编辑区预览，⏎ 才确认）
        self.emit_open(order[next], true, cx);
        cx.notify();
    }

    fn on_select_prev(&mut self, _: &SelectPrev, _window: &mut Window, cx: &mut Context<Self>) {
        self.select_step(-1, cx);
    }

    fn on_select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        self.select_step(1, cx);
    }

    fn on_dismiss(&mut self, _: &Dismiss, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(SearchViewEvent::Dismiss);
    }

    fn open_selected(&mut self, cx: &mut Context<Self>) {
        let ix = match self.selected {
            Some(ix) => ix,
            None => match self.visual_order().first() {
                Some(&ix) => ix,
                None => return,
            },
        };
        self.selected = Some(ix);
        self.emit_open(ix, false, cx);
    }

    fn emit_open(&mut self, ix: usize, preview: bool, cx: &mut Context<Self>) {
        if let Some(h) = self.hits.get(ix) {
            cx.emit(SearchViewEvent::Open { path: h.path.clone(), block_ord: h.block_ord, preview });
        }
    }

    fn select_mode(&mut self, hybrid: bool, cx: &mut Context<Self>) {
        if hybrid && (!self.semantic_key.enabled || !self.semantic_key.model_ready) {
            cx.emit(SearchViewEvent::Settings);
            return;
        }
        if self.hybrid != hybrid {
            self.hybrid = hybrid;
            self.schedule(cx);
        }
    }

    fn semantic_hint(&self) -> Option<String> {
        if self.results.semantic_pending {
            Some("正在补充语义… 全文结果先行显示".into())
        } else if let Some(error) = &self.semantic_error {
            Some(format!("语义暂不可用，已保留全文结果：{error}"))
        } else if self.hybrid && !self.semantic_key.model_ready {
            Some("全文仍可用；本地模型未就绪，可前往设置查看或下载。".into())
        } else {
            None
        }
    }

    /// 查询里用于高亮的词：去掉 `+` / 引号，`-` 开头的排除词不算。
    fn terms(&self) -> Vec<String> {
        query_terms(&self.query)
    }

    fn render_hit(&self, ix: usize, terms: &[String], cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let hit = &self.hits[ix];
        let selected = self.selected == Some(ix);
        let (snippet, ranges) = snippet(&hit.body, terms);
        let highlight = HighlightStyle {
            font_weight: Some(FontWeight::SEMIBOLD),
            color: Some(theme.foreground),
            background_color: Some(theme.accent),
            ..Default::default()
        };
        let text = StyledText::new(snippet).with_highlights(ranges.into_iter().map(|r| (r, highlight)));
        let (hover, active) = (theme.list_hover, theme.list_active);
        div()
            .id(("hit", ix))
            .w_full()
            .pl(px(20.))
            .pr_2()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .text_sm()
            .text_color(theme.muted_foreground)
            .line_height(relative(1.4))
            .when(selected, |el| el.bg(active))
            .when(!selected, |el| el.hover(move |s| s.bg(hover)))
            .child(text)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected = Some(ix);
                this.emit_open(ix, false, cx);
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_group(&self, group: &Group, terms: &[String], cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let dir = group.path.parent();
        let dir_label = if dir.is_empty() { String::new() } else { format!("{dir}/") };
        v_flex()
            .w_full()
            .child(
                h_flex()
                    .w_full()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .gap_2()
                    .items_baseline()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(group.title.clone()),
                    )
                    .child(div().flex_none().text_xs().text_color(theme.muted_foreground).child(dir_label)),
            )
            .children(group.hits.iter().map(|&ix| self.render_hit(ix, terms, cx)))
            .into_any_element()
    }
}

pub fn query_terms(query: &str) -> Vec<String> {
    FtsQuery::parse(query)
        .text
        .split_whitespace()
        .filter(|t| !t.starts_with('-'))
        .map(|t| t.trim_start_matches('+').trim_matches('"'))
        .filter(|t| !t.is_empty())
        .map(|t| t.to_owned())
        .collect()
}

/// 命中块的摘要：折叠空白、围绕第一个命中词截一段，并给出所有命中词在摘要里的字节区间。
pub fn snippet(body: &str, terms: &[String]) -> (String, Vec<Range<usize>>) {
    let flat: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = fold_case(&flat);
    let lower_terms: Vec<String> = terms.iter().map(|t| fold_case(t)).collect();
    let first = lower_terms.iter().filter_map(|t| lower.find(t.as_str())).min();

    // 截取窗口（按字符数），保证落在字符边界
    let chars: Vec<(usize, char)> = flat.char_indices().collect();
    let total = chars.len();
    let hit_char = first.map(|b| chars.iter().position(|(i, _)| *i >= b).unwrap_or(total)).unwrap_or(0);
    let start_char = hit_char.saturating_sub(SNIPPET_CONTEXT);
    let end_char = (start_char + SNIPPET_MAX).min(total);
    let start_byte = chars.get(start_char).map_or(flat.len(), |(i, _)| *i);
    let end_byte = chars.get(end_char).map_or(flat.len(), |(i, _)| *i);

    let mut out = String::new();
    if start_char > 0 {
        out.push('…');
    }
    let body_offset = out.len();
    out.push_str(&flat[start_byte..end_byte]);
    if end_char < total {
        out.push('…');
    }

    let mut ranges = Vec::new();
    let window_lower = &lower[start_byte..end_byte];
    for t in &lower_terms {
        if t.is_empty() {
            continue;
        }
        let mut from = 0;
        while let Some(pos) = window_lower[from..].find(t.as_str()) {
            let s = body_offset + from + pos;
            ranges.push(s..s + t.len());
            from += pos + t.len();
        }
    }
    ranges.sort_by_key(|r| r.start);
    // 合并重叠区间
    let mut merged: Vec<Range<usize>> = Vec::new();
    for r in ranges {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }
    (out, merged)
}

/// 大小写折叠，但只在不改变字节长度时使用（否则区间会错位）。
fn fold_case(s: &str) -> String {
    let lower = s.to_lowercase();
    if lower.len() == s.len() { lower } else { s.to_owned() }
}

impl Render for SearchView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let terms = self.terms();
        let phase = self.service.read(cx).phase();
        let status: Option<String> = if let Some(e) = &self.error {
            Some(format!("搜索出错：{e}"))
        } else if let Phase::Failed(_) = phase {
            Some("索引不可用".into())
        } else if self.query.is_empty() {
            None
        } else if self.searching {
            Some("搜索中…".into())
        } else {
            let n = self.hits.len();
            let suffix = match phase {
                Phase::Opening | Phase::Syncing { .. } => " · 索引尚未完成",
                _ => "",
            };
            Some(if n >= LIMIT {
                format!("前 {n} 条结果{suffix}")
            } else {
                format!("{n} 条结果{suffix}")
            })
        };
        let groups: Vec<AnyElement> = self.groups.iter().map(|g| self.render_group(g, &terms, cx)).collect();
        let empty_hint = if !self.query.is_empty()
            && !self.searching
            && !self.results.semantic_pending
            && self.hits.is_empty()
            && self.error.is_none()
        {
            Some("没有匹配的块。")
        } else if self.query.is_empty() {
            Some(
                "输入关键词、\"短语\"、+必含 -排除；#标签 / tag:标签 筛选笔记（父标签含子标签，多标签须全部匹配），-#标签 排除；点击结果定位到块。",
            )
        } else {
            None
        };

        v_flex()
            .id("search-view")
            .key_context("Picker")
            .on_action(cx.listener(Self::on_select_prev))
            .on_action(cx.listener(Self::on_select_next))
            .on_action(cx.listener(Self::on_dismiss))
            .size_full()
            .min_w_0()
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .px_2()
                    .pb_1()
                    .gap_1()
                    .child(
                        div().debug_selector(|| "semantic-mode-fts".into()).child(
                            Button::new("semantic-mode-fts")
                                .ghost()
                                .xsmall()
                                .label("全文")
                                .selected(!self.hybrid)
                                .on_click(cx.listener(|this, _, _, cx| this.select_mode(false, cx))),
                        ),
                    )
                    .child(
                        div().debug_selector(|| "semantic-mode-hybrid".into()).child(
                            Button::new("semantic-mode-hybrid")
                                .ghost()
                                .xsmall()
                                .label("混合")
                                .selected(self.hybrid)
                                .tooltip(if self.semantic_key.enabled && self.semantic_key.model_ready {
                                    "全文优先，本地语义稍后补充"
                                } else {
                                    "在搜索与隐私中启用或下载本地模型"
                                })
                                .on_click(cx.listener(|this, _, _, cx| this.select_mode(true, cx))),
                        ),
                    )
                    .child(div().flex_1())
                    .child(
                        div().debug_selector(|| "search-settings".into()).child(
                            Button::new("search-settings")
                                .ghost()
                                .xsmall()
                                .label("设置")
                                .tooltip("搜索与隐私")
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(SearchViewEvent::Settings))),
                        ),
                    ),
            )
            .child(
                div().px_2().pb_1().child(
                    Input::new(&self.input)
                        .prefix(Icon::new(IconName::Search).text_color(muted).size(px(14.)))
                        .cleanable(true),
                ),
            )
            .when_some(status, |el, s| el.child(div().px_3().py_1().text_xs().text_color(muted).child(s)))
            .when_some(self.semantic_hint(), |el, hint| {
                el.child(
                    div()
                        .debug_selector(|| "semantic-search-status".into())
                        .w_full()
                        .min_w_0()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(muted)
                        .child(hint),
                )
            })
            .child(
                v_flex()
                    .id("search-results")
                    .flex_1()
                    .min_h(px(0.))
                    .px_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children(groups)
                    .when_some(empty_hint, |el, hint| {
                        el.child(div().px_3().py_2().text_xs().text_color(muted).child(hint))
                    })
                    .vertical_scrollbar(&self.scroll),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::Duration;

    use aero_core::Vault;
    use gpui_kit::{AppContext as _, TestAppContext};

    // 不能 `use super::*`：它会把 GPUI 的 `test` 属性宏带进作用域，遮住标准 `#[test]`
    use super::{SearchView, SearchViewEvent, query_terms, snippet};
    use crate::search::{Phase, SearchService};

    #[test]
    fn terms_drop_operators_and_exclusions() {
        assert_eq!(query_terms("+brew \"upgrade\" -ripgrep zvec"), ["brew", "upgrade", "zvec"]);
        assert!(query_terms("-only").is_empty());
        assert_eq!(query_terms("anchor #rust tag:#cli"), ["anchor"]);
        assert!(query_terms("#rust tag:#cli").is_empty());
        assert_eq!(query_terms("\"#literal\" #filter"), ["#literal"]);
    }

    #[test]
    fn snippet_windows_around_first_hit_and_highlights_all() {
        let body = "第一行\n\n很长的前文 ".repeat(6) + "命中 anchor 之后还有 Anchor 再来一次";
        let (s, r) = snippet(&body, &["anchor".into()]);
        assert!(s.starts_with('…'), "{s}");
        assert!(s.contains("命中 anchor"));
        assert_eq!(r.len(), 2);
        for range in &r {
            assert!(s[range.clone()].eq_ignore_ascii_case("anchor"));
        }

        // 短正文不截、无命中不加省略号
        let (s, r) = snippet("short body", &["zzz".into()]);
        assert_eq!(s, "short body");
        assert!(r.is_empty());

        // 重叠 / 相邻区间合并
        let (s, r) = snippet("aaaa", &["aa".into(), "aaa".into()]);
        assert_eq!(s, "aaaa");
        assert_eq!(r, vec![0..4]);
    }

    fn hit(path: &str) -> aero_index::SearchHit {
        aero_index::SearchHit {
            path: aero_core::VaultPath::new(path).unwrap(),
            title: path.into(),
            block_ord: 0,
            body: "normal body".into(),
            score: 1.,
        }
    }

    #[test]
    fn fulltext_is_visible_while_semantic_is_pending() {
        let results = super::SearchResults {
            fulltext: Some(vec![hit("fts.md")]),
            semantic_pending: true,
            ..Default::default()
        };
        assert_eq!(results.visible(), Some(vec![hit("fts.md")]));
        assert!(results.semantic_pending);
    }

    #[test]
    fn early_semantic_waits_for_fulltext_then_fuses() {
        let mut results = super::SearchResults {
            semantic: Some(vec![hit("semantic.md"), hit("both.md")]),
            ..Default::default()
        };
        assert!(results.visible().is_none());
        results.fulltext = Some(vec![hit("both.md"), hit("fts.md")]);
        let visible = results.visible().unwrap();
        assert_eq!(visible.len(), 3);
        assert_eq!(visible[0].path.as_str(), "both.md");
    }

    #[test]
    fn loading_an_available_model_does_not_restart_the_current_query() {
        use super::SemanticRefreshKey;
        use crate::semantic::{ModelStatus, SearchPreferences, SemanticPhase, SemanticStatus};
        let prefs = SearchPreferences { semantic_enabled: true, ..Default::default() };
        let ready = SemanticRefreshKey::new(
            &prefs,
            &SemanticStatus {
                scope_revision: 0,
                revision: 0,
                model: ModelStatus::Ready,
                phase: SemanticPhase::Ready { notes: 4, chunks: 8 },
            },
        );
        let loading = SemanticRefreshKey::new(
            &prefs,
            &SemanticStatus {
                scope_revision: 0,
                revision: 0,
                model: ModelStatus::Loading,
                phase: SemanticPhase::Ready { notes: 4, chunks: 8 },
            },
        );
        assert!(!ready.needs_refresh(&loading));
        assert!(!loading.needs_refresh(&ready));
    }

    #[test]
    fn ignored_membership_refreshes_visible_results_even_while_paused() {
        use crate::semantic::{ModelStatus, SearchPreferences, SemanticPhase, SemanticStatus};
        let prefs = SearchPreferences { semantic_enabled: true, ..Default::default() };
        let before =
            SemanticStatus { model: ModelStatus::Ready, phase: SemanticPhase::Paused, ..Default::default() };
        let after = SemanticStatus { scope_revision: 1, ..before.clone() };
        let before = super::SemanticRefreshKey::new(&prefs, &before);
        let after = super::SemanticRefreshKey::new(&prefs, &after);
        assert!(before.needs_refresh(&after));
        assert!(!after.needs_refresh(&after));
    }

    #[test]
    fn updated_content_refreshes_even_when_index_counts_are_unchanged() {
        use crate::semantic::{ModelStatus, SearchPreferences, SemanticPhase, SemanticStatus};
        let prefs = SearchPreferences { semantic_enabled: true, ..Default::default() };
        let first = SemanticStatus {
            scope_revision: 0,
            model: ModelStatus::Ready,
            phase: SemanticPhase::Ready { notes: 4, chunks: 8 },
            revision: 1,
        };
        let updated = SemanticStatus { scope_revision: 0, revision: 2, ..first.clone() };
        let first = super::SemanticRefreshKey::new(&prefs, &first);
        let updated = super::SemanticRefreshKey::new(&prefs, &updated);
        assert!(first.needs_refresh(&updated));
        assert!(!updated.needs_refresh(&updated));
    }

    #[test]
    fn semantic_progress_does_not_trigger_repeated_queries() {
        use crate::semantic::{ModelStatus, SearchPreferences, SemanticPhase, SemanticStatus};
        let prefs = SearchPreferences { semantic_enabled: true, ..Default::default() };
        let key = |phase| {
            super::SemanticRefreshKey::new(
                &prefs,
                &SemanticStatus { scope_revision: 0, revision: 0, model: ModelStatus::Ready, phase },
            )
        };
        let initial = key(SemanticPhase::WaitingForIdle);
        let indexing = key(SemanticPhase::Indexing { done: 1, total: 100 });
        assert!(!initial.needs_refresh(&indexing));
        assert!(!indexing.needs_refresh(&key(SemanticPhase::Indexing { done: 99, total: 100 })));
        assert!(!indexing.needs_refresh(&key(SemanticPhase::Paused)));
        let complete = key(SemanticPhase::Ready { notes: 100, chunks: 200 });
        assert!(indexing.needs_refresh(&complete));
        assert!(!complete.needs_refresh(&complete));
        let disabled = super::SemanticRefreshKey::new(
            &SearchPreferences::default(),
            &SemanticStatus {
                scope_revision: 0,
                revision: 0,
                model: ModelStatus::Ready,
                phase: SemanticPhase::Disabled,
            },
        );
        assert!(complete.needs_refresh(&disabled));
    }

    struct CancelSentinel(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for CancelSentinel {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn assert_semantic_cancelled(
        cx: &mut TestAppContext,
        change: impl FnOnce(&mut SearchView, &mut gpui_kit::Window, &mut gpui_kit::Context<SearchView>),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let service = cx.new(|cx| SearchService::new(vault, data.path().into(), cx));
        let (view, cx) = cx.add_window_view(|window, cx| SearchView::new(service.clone(), window, cx));
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sentinel = CancelSentinel(cancelled.clone());
        view.update_in(cx, |v, window, cx| {
            v.input.update(cx, |s, cx| s.set_value("old", window, cx));
            v.query = "old".into();
            v.hybrid = true;
            v.semantic_key.enabled = true;
            v.semantic_key.model_ready = true;
            v.results.semantic_pending = true;
            // The real wrapper owns this pending task, just as it owns SemanticService::query.
            let task = cx.background_spawn(async move {
                let _sentinel = sentinel;
                std::future::pending().await
            });
            v.await_semantic(v.generation, task, cx);
        });
        cx.run_until_parked();
        assert!(!cancelled.load(std::sync::atomic::Ordering::SeqCst));
        view.update_in(cx, change);
        cx.run_until_parked();
        assert!(
            cancelled.load(std::sync::atomic::Ordering::SeqCst),
            "drop the semantic query immediately, without advancing the debounce clock"
        );
        cx.update(|_, cx| {
            assert!(!view.read(cx).results.semantic_pending);
            service.read(cx).close_blocking();
        });
    }

    #[gpui_kit::test]
    fn input_changes_cancel_semantic_before_debounce(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        for text in ["new", ""] {
            assert_semantic_cancelled(cx, |v, window, cx| {
                v.input.update(cx, |s, cx| s.set_value(text, window, cx));
                v.on_input_event(&v.input.clone(), &super::InputEvent::Change, window, cx);
            });
        }
    }

    #[gpui_kit::test]
    fn new_search_cancels_previous_semantic_even_when_empty(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        for text in ["new", ""] {
            assert_semantic_cancelled(cx, |v, window, cx| {
                v.input.update(cx, |s, cx| s.set_value(text, window, cx));
                v.run_search(cx);
            });
        }
    }

    #[gpui_kit::test]
    fn mode_switch_cancels_semantic_even_when_empty(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        for text in ["old", ""] {
            for hybrid in [false, true] {
                assert_semantic_cancelled(cx, |v, window, cx| {
                    v.input.update(cx, |s, cx| s.set_value(text, window, cx));
                    v.hybrid = !hybrid;
                    v.select_mode(hybrid, cx);
                });
            }
        }
    }

    #[gpui_kit::test]
    fn disabling_or_losing_model_cancels_semantic_even_when_empty(cx: &mut TestAppContext) {
        use crate::semantic::{ModelStatus, SearchPreferences, SemanticPhase, SemanticStatus};
        cx.update(gpui_kit::init);
        for text in ["old", ""] {
            for (enabled, model) in [
                (false, ModelStatus::Ready),
                (true, ModelStatus::Missing),
                (true, ModelStatus::Failed("unavailable".into())),
            ] {
                assert_semantic_cancelled(cx, |v, window, cx| {
                    v.input.update(cx, |s, cx| s.set_value(text, window, cx));
                    v.query = text.into();
                    let generation = v.generation;
                    let prefs = SearchPreferences { semantic_enabled: enabled, ..Default::default() };
                    v.semantic_changed(
                        &prefs,
                        &SemanticStatus {
                            scope_revision: 0,
                            revision: 0,
                            model,
                            phase: SemanticPhase::WaitingForModel,
                        },
                        cx,
                    );
                    v.receive_semantic(generation, Ok(vec![hit("stale.md")]), cx);
                    v.receive_semantic(generation, Err("stale error".into()), cx);
                    assert!(v.results.semantic.is_none());
                    assert!(v.semantic_error.is_none());
                });
            }
        }
    }

    #[gpui_kit::test]
    fn completed_identity_survives_transient_semantic_phases(cx: &mut TestAppContext) {
        use crate::semantic::{ModelStatus, SearchPreferences, SemanticPhase, SemanticStatus};
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let service = cx.new(|cx| SearchService::new(vault, data.path().into(), cx));
        let (view, cx) = cx.add_window_view(|window, cx| SearchView::new(service.clone(), window, cx));
        let prefs = SearchPreferences { semantic_enabled: true, ..Default::default() };
        view.update_in(cx, |v, window, cx| {
            v.input.update(cx, |s, cx| s.set_value("meaning", window, cx));
            v.query = "meaning".into();
            let ready = SemanticStatus {
                scope_revision: 0,
                revision: 0,
                model: ModelStatus::Ready,
                phase: SemanticPhase::Ready { notes: 4, chunks: 8 },
            };
            v.semantic_changed(&prefs, &ready, cx);
            let generation = v.generation;
            for phase in [
                SemanticPhase::WaitingForIdle,
                SemanticPhase::Paused,
                SemanticPhase::Indexing { done: 1, total: 4 },
            ] {
                v.semantic_changed(
                    &prefs,
                    &SemanticStatus { scope_revision: 0, revision: 0, model: ModelStatus::Loading, phase },
                    cx,
                );
                assert_eq!(v.generation, generation, "transient status must not start inference");
                v.semantic_changed(&prefs, &ready, cx);
                assert_eq!(v.generation, generation, "the same completed index must not restart inference");
                assert_eq!(v.semantic_key.completed, Some((4, 8, 0)));
            }
            v.semantic_changed(
                &prefs,
                &SemanticStatus {
                    scope_revision: 0,
                    revision: 0,
                    model: ModelStatus::Ready,
                    phase: SemanticPhase::Ready { notes: 4, chunks: 9 },
                },
                cx,
            );
            assert!(v.generation > generation, "a newly completed index still refreshes the query");
        });
        cx.update(|_, cx| service.read(cx).close_blocking());
    }

    #[gpui_kit::test]
    fn asynchronous_fulltext_first_and_late_semantic_preserve_selection(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let service = cx.new(|cx| SearchService::new(vault, data.path().into(), cx));
        let (view, cx) = cx.add_window_view(|window, cx| SearchView::new(service.clone(), window, cx));
        view.update_in(cx, |v, _, cx| {
            v.generation = 10;
            v.query = "meaning".into();
            v.searching = true;
            v.results.semantic_pending = true;
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_millis(10)).await;
                let _ = this
                    .update(cx, |v, cx| v.receive_fulltext(10, Ok(vec![hit("fts.md"), hit("both.md")]), cx));
            })
            .detach();
            let executor = cx.background_executor().clone();
            let task = cx.background_spawn(async move {
                executor.timer(Duration::from_millis(100)).await;
                Ok(vec![hit("both.md"), hit("semantic.md")])
            });
            v.await_semantic(10, task, cx);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(20));
        cx.run_until_parked();
        view.update_in(cx, |v, _, _| {
            assert_eq!(v.hits(), [hit("fts.md"), hit("both.md")]);
            assert!(!v.searching);
            assert!(v.results.semantic_pending);
            assert!(v.semantic_task.is_some());
            assert!(v.semantic_hint().unwrap().contains("正在补充语义"));
            v.selected = Some(0);
        });
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        cx.update(|_, cx| {
            let v = view.read(cx);
            assert_eq!(v.hits().len(), 3);
            assert_eq!(v.hits()[0].path.as_str(), "both.md");
            assert_eq!(v.hits()[v.selected().unwrap()].path.as_str(), "fts.md");
            assert!(!v.results.semantic_pending);
            assert!(v.semantic_task.is_none());
            service.read(cx).close_blocking();
        });
    }

    #[gpui_kit::test]
    fn semantic_failure_keeps_fulltext_and_unavailable_mode_opens_settings(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let service = cx.new(|cx| SearchService::new(vault, data.path().into(), cx));
        let (view, cx) = cx.add_window_view(|window, cx| SearchView::new(service.clone(), window, cx));
        let settings = Rc::new(RefCell::new(0));
        let sink = settings.clone();
        cx.update(|_, cx| {
            cx.subscribe(&view, move |_, event, _| {
                if matches!(event, SearchViewEvent::Settings) {
                    *sink.borrow_mut() += 1;
                }
            })
            .detach();
        });
        view.update_in(cx, |v, _, cx| {
            v.generation = 5;
            v.results.semantic_pending = true;
            v.receive_fulltext(5, Ok(vec![hit("fts.md")]), cx);
            v.receive_semantic(5, Err("model unavailable".into()), cx);
            assert_eq!(v.hits(), [hit("fts.md")]);
            assert!(v.error.is_none());
            assert!(!v.results.semantic_pending);
            assert!(v.semantic_hint().unwrap().contains("已保留全文"));
            v.select_mode(true, cx);
            assert!(!v.hybrid);
        });
        assert_eq!(*settings.borrow(), 1, "unavailable mode is a settings entry, not a dead button");
        cx.update(|_, cx| service.read(cx).close_blocking());
    }

    #[gpui_kit::test]
    fn input_invalidates_in_flight_query_before_debounce(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let service = cx.new(|cx| SearchService::new(vault, data.path().into(), cx));
        let (view, cx) = cx.add_window_view(|window, cx| SearchView::new(service.clone(), window, cx));
        view.update_in(cx, |v, window, cx| {
            v.input.update(cx, |s, cx| s.set_value("old", window, cx));
            v.run_search(cx);
        });
        let previous = cx.update(|_, cx| view.read(cx).generation);
        view.update_in(cx, |v, window, cx| {
            v.input.update(cx, |s, cx| s.set_value("new", window, cx));
            v.schedule(cx);
            assert!(v.generation > previous, "invalidate on change, not 150 ms later");
            assert_eq!(v.query(), "old", "replacement query is still debounced");
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_millis(10)).await;
                let _ = this.update(cx, |v, cx| {
                    v.receive_fulltext(previous, Ok(vec![hit("stale-fts.md")]), cx);
                    v.receive_semantic(previous, Ok(vec![hit("stale-semantic.md")]), cx);
                    v.receive_semantic(previous, Err("stale error".into()), cx);
                });
            })
            .detach();
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(20));
        cx.run_until_parked();
        cx.update(|_, cx| {
            let v = view.read(cx);
            assert!(v.hits().is_empty(), "neither response may flash back while replacement is debounced");
            assert!(v.semantic_error.is_none());
            assert!(v.searching, "stale completion must not finish the replacement query");
            service.read(cx).close_blocking();
        });
    }

    /// 打开索引 → 输入防抖 → 结果分组 → ↑↓ / ⏎ / Esc 事件。用真实 zvec 引擎，索引放临时目录。
    #[gpui_kit::test]
    fn search_view_end_to_end(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::actions::init(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(
            dir.path().join("a.md"),
            "---\ntags: [rust]\n---\n\n# Alpha\n\n第一段 anchor #cli\n\n第二段也有 anchor\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("sub/b.md"),
            "---\ntitle: Bravo\ntags: [rust/cli]\n---\n\nanchor 在这里\n",
        )
        .unwrap();
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let service = cx.update(|cx| cx.new(|cx| SearchService::new(vault, data.path().to_path_buf(), cx)));
        let (view, cx) = cx.add_window_view(|window, cx| SearchView::new(service.clone(), window, cx));
        let events: Rc<RefCell<Vec<String>>> = Rc::default();
        let sink = events.clone();
        cx.update(|_, cx| {
            cx.subscribe(&view, move |_, ev: &SearchViewEvent, _| {
                sink.borrow_mut().push(match ev {
                    SearchViewEvent::Open { path, block_ord, .. } => {
                        format!("open {}#{block_ord}", path.as_str())
                    }
                    SearchViewEvent::Dismiss => "dismiss".into(),
                    SearchViewEvent::Settings => "settings".into(),
                });
            })
            .detach();
        });

        // 同步线程是真线程：等它到 Idle（测试执行器的时钟是假的，这里用真实等待）
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            let phase = cx.update(|_, cx| service.read(cx).phase());
            if phase == Phase::Idle {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "index never became idle: {phase:?}");
            std::thread::sleep(Duration::from_millis(30));
        }
        let report = cx.update(|_, cx| service.read(cx).last_report()).unwrap();
        assert_eq!((report.indexed, report.errors.len()), (2, 0), "{report:?}");

        // 输入 → 150 ms 防抖 → 后台查询
        view.update_in(cx, |v, window, cx| v.focus(window, cx));
        cx.simulate_input("anchor");
        cx.update(|_, cx| assert!(view.read(cx).hits().is_empty(), "debounced: no query yet"));
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
        cx.update(|_, cx| {
            let v = view.read(cx);
            assert_eq!(v.query(), "anchor");
            assert_eq!(v.hits().len(), 3);
            assert_eq!(v.groups.len(), 2, "grouped by note");
            assert!(v.selected().is_none());
        });

        // ↓ 选中第一条并预览定位；⏎ 打开；Esc 交还焦点
        cx.simulate_keystrokes("down");
        cx.update(|_, cx| assert_eq!(view.read(cx).selected(), Some(view.read(cx).visual_order()[0])));
        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("enter");
        cx.simulate_keystrokes("escape");
        let got = events.borrow().clone();
        assert_eq!(got.len(), 4, "{got:?}");
        assert!(got[0].starts_with("open "));
        assert_eq!(got[1], got[2], "enter opens the current selection");
        assert_eq!(got[3], "dismiss");
        // 每条命中都能对应到 a.md / sub/b.md 的某个块
        for e in &got[..3] {
            assert!(e.starts_with("open a.md#") || e.starts_with("open sub/b.md#"), "{e}");
        }

        // 排除词、清空
        view.update_in(cx, |v, window, cx| {
            v.input.update(cx, |s, cx| s.set_value("anchor -第二段", window, cx))
        });
        view.update_in(cx, |v, _, cx| v.schedule(cx));
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
        cx.update(|_, cx| assert_eq!(view.read(cx).hits().len(), 2));
        let cases: [(&str, usize, &[&str]); 8] = [
            // `cli` 不命中 b.md 的 `rust/cli`（层级只向下）
            ("anchor #RUST tag:#cli", 2, &["a.md"]),
            // 纯标签：标签是笔记级的，一篇只列一条；父标签 `rust` 命中 b.md 的 `rust/cli`
            ("#rust", 2, &["a.md", "sub/b.md"]),
            ("tag:#rust/cli", 1, &["sub/b.md"]),
            ("#rust #rust/cli", 1, &["sub/b.md"]),
            ("anchor -#cli", 1, &["sub/b.md"]),
            ("anchor -#rust", 0, &[]),
            ("-#rust", 0, &[]),
            ("anchor #missing", 0, &[]),
        ];
        for (query, count, paths) in cases {
            view.update_in(cx, |v, window, cx| v.input.update(cx, |s, cx| s.set_value(query, window, cx)));
            view.update_in(cx, |v, _, cx| v.schedule(cx));
            cx.executor().advance_clock(Duration::from_millis(200));
            cx.run_until_parked();
            cx.update(|_, cx| {
                let v = view.read(cx);
                assert!(v.error.is_none(), "{:?}", v.error);
                assert_eq!(v.hits().len(), count, "{query}");
                assert!(v.hits().iter().all(|hit| paths.contains(&hit.path.as_str())), "{query}");
            });
            if count > 0 {
                view.update_in(cx, |v, _, cx| v.open_selected(cx));
                let last = events.borrow().last().cloned().unwrap();
                assert!(paths.iter().any(|p| last.starts_with(&format!("open {p}#"))), "{query}: {last}");
            }
        }
        view.update_in(cx, |v, window, cx| {
            v.input.update(cx, |s, cx| s.set_value("anchor #rust", window, cx));
            v.run_search(cx);
            v.input.update(cx, |s, cx| s.set_value("", window, cx));
            v.schedule(cx);
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            let v = view.read(cx);
            assert!(v.hits().is_empty(), "clearing must invalidate in-flight tag queries");
            assert!(v.query().is_empty());
            assert!(!v.searching);
        });
    }
}
