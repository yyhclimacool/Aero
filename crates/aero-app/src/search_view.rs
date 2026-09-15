//! 左侧栏的搜索视图（`ui_design.md` 4.2，M1b 子集）。
//!
//! 输入 150 ms 防抖后查全文索引，结果按笔记分组、块级命中、关键词高亮；
//! `↑↓` 在命中间移动并让编辑区定位到该块，`⏎` 打开，`Esc` 回到编辑器。
//! `#tag` / `tag:#tag` 按笔记级标签过滤；语义开关、`path:` 与搜索历史随后续里程碑加入。

use std::ops::Range;
use std::time::Duration;

use aero_core::{FtsQuery, VaultPath};
use aero_index::SearchHit;
use gpui_kit::assets::IconName;
use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Icon, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::actions::{Dismiss, SelectNext, SelectPrev};
use crate::search::{Phase, SearchService};

const DEBOUNCE: Duration = Duration::from_millis(150);
const LIMIT: usize = 200;
/// 摘要里命中词前后各留多少字符
const SNIPPET_CONTEXT: usize = 36;
const SNIPPET_MAX: usize = 110;

pub enum SearchViewEvent {
    /// 定位到某篇笔记的第 `block_ord` 个可索引块
    Open { path: VaultPath, block_ord: u32 },
    /// Esc：把焦点交还给编辑器
    Dismiss,
}

impl EventEmitter<SearchViewEvent> for SearchView {}

struct Group {
    path: VaultPath,
    title: String,
    /// `hits` 里的下标
    hits: Vec<usize>,
}

pub struct SearchView {
    service: Entity<SearchService>,
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
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("搜索全文…  #标签 tag:#标签"));
        let input_sub = cx.subscribe_in(&input, window, Self::on_input_event);
        // 索引刚建好时把当前查询再跑一遍——用户可能在索引完成前就开始输入了
        let service_sub = cx.observe(&service, |this, service, cx| {
            let ready = matches!(service.read(cx).phase(), Phase::Idle);
            if ready && !this.was_ready && !this.query.is_empty() {
                this.run_search(cx);
            }
            this.was_ready = ready;
            cx.notify();
        });
        let was_ready = matches!(service.read(cx).phase(), Phase::Idle);
        SearchView {
            service,
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

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |s, cx| {
            s.focus(window, cx);
            s.select_all(window, cx);
        });
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
        let text = self.input.read(cx).value().to_string();
        if text.trim().is_empty() {
            self.debounce = None;
            self.generation += 1;
            self.searching = false;
            self.error = None;
            self.query.clear();
            self.set_results(Vec::new(), cx);
            return;
        }
        self.debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| this.run_search(cx));
        }));
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.query = text.clone();
        self.generation += 1;
        let generation = self.generation;
        self.searching = true;
        let task = self.service.read(cx).search(text, LIMIT, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.searching = false;
                match result {
                    Ok(hits) => {
                        this.error = None;
                        this.set_results(hits, cx);
                    }
                    Err(e) => {
                        this.error = Some(e);
                        this.set_results(Vec::new(), cx);
                    }
                }
            });
        })
        .detach();
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
        self.hits = hits;
        self.groups = groups;
        self.selected = None;
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
        // 预览定位（4.2：↑↓ 在编辑区预览，⏎ 才确认）——M1b 没有历史，两者行为相同
        self.emit_open(order[next], cx);
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
        self.emit_open(ix, cx);
    }

    fn emit_open(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(h) = self.hits.get(ix) {
            cx.emit(SearchViewEvent::Open { path: h.path.clone(), block_ord: h.block_ord });
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
                this.emit_open(ix, cx);
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
            && self.hits.is_empty()
            && self.error.is_none()
        {
            Some("没有匹配的块。")
        } else if self.query.is_empty() {
            Some(
                "输入关键词、\"短语\"、+必含 -排除；用 #标签 或 tag:#标签 筛选笔记，多标签须全部匹配，点击结果定位到块。",
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
            .child(
                div().px_2().pb_1().child(
                    Input::new(&self.input)
                        .prefix(Icon::new(IconName::Search).text_color(muted).size(px(14.)))
                        .cleanable(true),
                ),
            )
            .when_some(status, |el, s| el.child(div().px_3().py_1().text_xs().text_color(muted).child(s)))
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
                    SearchViewEvent::Open { path, block_ord } => {
                        format!("open {}#{block_ord}", path.as_str())
                    }
                    SearchViewEvent::Dismiss => "dismiss".into(),
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
        for (query, count, path) in [
            ("anchor #RUST tag:#cli", 2, "a.md"),
            // 纯标签：标签是笔记级的，一篇只列一条
            ("#rust", 1, "a.md"),
            ("tag:#rust/cli", 1, "sub/b.md"),
            ("#rust #rust/cli", 0, ""),
            ("anchor #missing", 0, ""),
        ] {
            view.update_in(cx, |v, window, cx| v.input.update(cx, |s, cx| s.set_value(query, window, cx)));
            view.update_in(cx, |v, _, cx| v.schedule(cx));
            cx.executor().advance_clock(Duration::from_millis(200));
            cx.run_until_parked();
            cx.update(|_, cx| {
                let v = view.read(cx);
                assert!(v.error.is_none(), "{:?}", v.error);
                assert_eq!(v.hits().len(), count, "{query}");
                assert!(v.hits().iter().all(|hit| hit.path.as_str() == path), "{query}");
            });
            if count > 0 {
                view.update_in(cx, |v, _, cx| v.open_selected(cx));
                assert!(events.borrow().last().unwrap().starts_with(&format!("open {path}#")));
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
