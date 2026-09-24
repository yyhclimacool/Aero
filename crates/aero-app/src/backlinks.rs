//! 文档底部双标签引用列表；链接随文本快照更新，滚动交给外层 Editor。

use std::time::Duration;

use aero_core::{FileStamp, Snapshot, VaultPath};
use aero_index::Backlink;
use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme as _, Icon, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::search::{Phase, SearchService};

const LIMIT: usize = 200;

#[derive(Debug, PartialEq, Eq)]
struct OutgoingLink {
    label: String,
    url: String,
}

impl OutgoingLink {
    fn target_label(&self) -> String {
        let Some((target, fragment)) = aero_core::links::decode_note_url(&self.url) else {
            return self.url.clone();
        };
        let target = if target.is_empty() { "本文".into() } else { target };
        match fragment.filter(|fragment| !fragment.is_empty()) {
            Some(fragment) => format!("{target} › {fragment}"),
            None => target,
        }
    }
}

#[derive(Default)]
struct OutgoingLinks {
    links: Vec<OutgoingLink>,
    truncated: bool,
}

fn extract_outgoing(src: &str) -> OutgoingLinks {
    let mut analysis = aero_core::markup::analyze(src);
    // markup 为编辑器省略空标签的可点击区间，但仍留下相邻的开、闭隐藏区间。
    // 只给 AST 已确认的空标签填空格，再解析一次；不能扫描裸 []，否则会误认代码 / 转义。
    let empty_labels: Vec<_> = analysis
        .conceal
        .windows(2)
        .filter_map(|pair| {
            let (open, close) = (&pair[0], &pair[1]);
            let opening = src.get(open.clone())?;
            (open.end == close.start
                && (opening == "[" || opening == "![" || opening.starts_with("[["))
                && src.get(close.clone())?.starts_with(']'))
            .then_some(open.end)
        })
        .collect();
    let padded;
    let src = if empty_labels.is_empty() {
        src
    } else {
        let mut text = String::with_capacity(src.len() + empty_labels.len());
        let mut from = 0;
        for at in empty_labels {
            text.push_str(&src[from..at]);
            text.push(' ');
            from = at;
        }
        text.push_str(&src[from..]);
        padded = text;
        analysis = aero_core::markup::analyze(&padded);
        &padded
    };
    let mut page = OutgoingLinks::default();
    let mut seen = std::collections::HashSet::new();
    analysis.links.sort_by_key(|(range, _)| range.start);
    for (mut range, url) in analysis.links {
        if !seen.insert(url.clone()) {
            continue;
        }
        if page.links.len() == LIMIT {
            page.truncated = true;
            break;
        }
        // markup 的括号计数没有跳过转义的 ]；从标签起点恢复可见标签的边界。
        if src[..range.start].ends_with('[') && !src[..range.start].ends_with("[[") {
            range.end = markdown_label_end(src, range.start).unwrap_or(range.end);
        }
        let label =
            aero_core::index::plain_text(src.get(range).unwrap_or(""), &aero_core::BlockKind::Paragraph);
        let label = label.trim();
        page.links
            .push(OutgoingLink { label: if label.is_empty() { url.clone() } else { label.to_owned() }, url });
    }
    page
}

fn markdown_label_end(src: &str, start: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut at = start;
    let mut depth = 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 1,
            b'`' => {
                let count = bytes[at..].iter().take_while(|&&b| b == b'`').count();
                let delimiter = &src[at..at + count];
                if let Some(end) = src[at + count..].find(delimiter) {
                    at += count + end + count;
                    continue;
                }
                at += count;
                continue;
            }
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
        at += 1;
    }
    None
}

pub enum ReferenceOpened {
    Backlink(Backlink),
    Outgoing(String),
}
impl EventEmitter<ReferenceOpened> for BacklinksView {}

pub struct BacklinksView {
    service: Entity<SearchService>,
    note: Option<VaultPath>,
    hits: Vec<Backlink>,
    total: Option<u64>,
    matched: u64,
    show_outgoing: bool,
    selected_block: Option<String>,
    only_block: bool,
    editor_subscription: Option<Subscription>,
    editor_epoch: u64,
    outgoing: OutgoingLinks,
    outgoing_key: Option<(FileStamp, u64)>,
    outgoing_task: Option<Task<()>>,
    outgoing_loading: bool,
    generation: u64,
    loading: bool,
    error: Option<String>,
    query: Option<Task<()>>,
    _subscription: Subscription,
}

impl BacklinksView {
    pub fn new(service: Entity<SearchService>, cx: &mut Context<Self>) -> Self {
        // 初次建库完成，以及保存 / 重命名 / 删除 / 外部变化完成后都会通知。
        let subscription = cx.observe(&service, |this, _, cx| this.refresh(cx));
        Self {
            service,
            note: None,
            hits: Vec::new(),
            total: None,
            matched: 0,
            show_outgoing: false,
            selected_block: None,
            only_block: false,
            editor_subscription: None,
            editor_epoch: 0,
            outgoing: OutgoingLinks::default(),
            outgoing_key: None,
            outgoing_task: None,
            outgoing_loading: false,
            generation: 0,
            loading: false,
            error: None,
            query: None,
            _subscription: subscription,
        }
    }

    pub fn set_note(&mut self, note: Option<VaultPath>, cx: &mut Context<Self>) {
        if self.note == note {
            return;
        }
        self.note = note;
        self.show_outgoing = false;
        self.selected_block = None;
        self.only_block = false;
        self.editor_subscription = None;
        self.editor_epoch += 1;
        self.outgoing_task = None;
        self.outgoing_key = None;
        self.outgoing = OutgoingLinks::default();
        self.outgoing_loading = false;
        self.refresh(cx);
    }

    pub fn set_editor(
        &mut self,
        editor: &Entity<crate::editor::Editor>,
        selected_block: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.editor_epoch += 1;
        let epoch = self.editor_epoch;
        self.editor_subscription = Some(cx.observe(editor, |this, editor, cx| {
            let editor = editor.read(cx);
            if this.note.as_ref() != Some(editor.path()) {
                return;
            }
            let selected_block = editor.selected_reference(cx);
            let (stamp, snapshot) = editor.reference_snapshot();
            this.set_selected_block(selected_block, cx);
            this.sync_outgoing(stamp, snapshot, cx);
        }));
        // connect_backlinks 正在 Editor::update 内，当前栈不能再次 read Editor。
        let editor = editor.downgrade();
        let this = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = this.update(cx, |this, cx| {
                if epoch != this.editor_epoch {
                    return;
                }
                let Some(editor) = editor.upgrade() else { return };
                let editor = editor.read(cx);
                if this.note.as_ref() != Some(editor.path()) {
                    return;
                }
                let (stamp, snapshot) = editor.reference_snapshot();
                this.set_selected_block(selected_block, cx);
                this.sync_outgoing(stamp, snapshot, cx);
            });
        });
    }

    fn sync_outgoing(&mut self, stamp: FileStamp, snapshot: Snapshot, cx: &mut Context<Self>) {
        let Some(note) = self.note.clone() else { return };
        let key = (stamp, snapshot.version());
        if self.outgoing_key == Some(key) {
            return;
        }
        self.outgoing_key = Some(key);
        self.outgoing_task = None;
        self.outgoing_loading = true;
        self.outgoing_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(150)).await;
            // Rope 物化和 Markdown 解析都在 debounce 之后、后台线程中进行。
            let links =
                cx.background_spawn(async move { extract_outgoing(&snapshot.text().to_string()) }).await;
            let _ = this.update(cx, |this, cx| {
                if this.note.as_ref() != Some(&note) || this.outgoing_key != Some(key) {
                    return;
                }
                this.outgoing = links;
                this.outgoing_loading = false;
                this.outgoing_task = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn set_selected_block(&mut self, block: Option<String>, cx: &mut Context<Self>) {
        if block == self.selected_block {
            return;
        }
        self.selected_block = block;
        if self.only_block {
            self.only_block = self.selected_block.is_some();
            self.refresh(cx);
        } else {
            cx.notify();
        }
    }

    #[cfg(test)]
    pub fn total(&self) -> Option<u64> {
        self.total
    }

    fn filter(&mut self, only_block: bool, cx: &mut Context<Self>) {
        if self.only_block == only_block {
            return;
        }
        self.only_block = only_block;
        self.refresh(cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.query = None;
        self.error = None;
        self.hits.clear();
        self.total = None;
        self.matched = 0;
        self.loading = false;
        cx.notify();
        let Some(note) = self.note.clone() else { return };
        if !matches!(self.service.read(cx).phase(), Phase::Idle) {
            return;
        }
        self.loading = true;
        let generation = self.generation;
        let fragment = self.selected_block.as_ref().filter(|_| self.only_block).map(|id| format!("^{id}"));
        let task = self.service.read(cx).backlinks(note, fragment, LIMIT, cx);
        self.query = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(page) => {
                        this.total = Some(page.total);
                        this.matched = page.matched;
                        this.hits = page.hits;
                    }
                    Err(error) => {
                        this.hits.clear();
                        this.error = Some(error);
                    }
                }
                cx.notify();
            });
        }));
    }

    #[cfg(test)]
    pub fn hits(&self) -> &[Backlink] {
        &self.hits
    }

    fn render_hit(&self, ix: usize, cx: &Context<Self>) -> AnyElement {
        let hit = self.hits[ix].clone();
        let hint = format!("{}\n{}", hit.path, hit.label);
        let link_color = cx.theme().info;
        h_flex()
            .id(SharedString::from(format!("backlink-{ix}")))
            .debug_selector(move || format!("backlink-{ix}"))
            .w_full()
            .min_w_0()
            .gap_2()
            .py_2()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .cursor_pointer()
            .hover(move |s| s.text_color(link_color))
            .child(Icon::new(IconName::FileText).size(px(18.)).text_color(link_color))
            .child(div().flex_1().min_w_0().truncate().child(hit.title.clone()))
            .tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(hint.clone()).build(window, cx)
            })
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.stop_propagation();
                cx.emit(ReferenceOpened::Backlink(hit.clone()));
            }))
            .into_any_element()
    }

    fn render_outgoing(&self, ix: usize, cx: &Context<Self>) -> AnyElement {
        let link = &self.outgoing.links[ix];
        let url = link.url.clone();
        let target = link.target_label();
        let label = if link.label == link.url { target.clone() } else { link.label.clone() };
        let link_color = cx.theme().info;
        h_flex()
            .id(SharedString::from(format!("outgoing-link-{ix}")))
            .debug_selector(move || format!("outgoing-link-{ix}"))
            .w_full()
            .min_w_0()
            .gap_2()
            .py_2()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .cursor_pointer()
            .hover(move |s| s.text_color(link_color))
            .child(Icon::new(IconName::FileText).size(px(18.)).text_color(link_color))
            .child(div().flex_1().min_w_0().truncate().child(label))
            .tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(target.clone()).build(window, cx)
            })
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.stop_propagation();
                cx.emit(ReferenceOpened::Outgoing(url.clone()));
            }))
            .into_any_element()
    }
}

impl Render for BacklinksView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let outgoing_status = if self.note.is_none() {
            "打开一篇笔记以查看链接。".to_owned()
        } else if self.outgoing_loading || self.outgoing_key.is_none() {
            "正在更新本文链接…".to_owned()
        } else if self.outgoing.links.is_empty() {
            "本文还没有引用链接。".to_owned()
        } else if self.outgoing.truncated {
            format!("按地址去重，仅显示前 {LIMIT} 条链接。")
        } else {
            format!("{} 条链接", self.outgoing.links.len())
        };
        let status = if self.note.is_none() {
            "打开一篇笔记以查看反链。".to_owned()
        } else if let Some(error) = &self.error {
            format!("无法读取反链：{error}")
        } else {
            match self.service.read(cx).phase() {
                Phase::Failed(_) => "索引不可用，暂时无法查看反链。".to_owned(),
                Phase::Opening | Phase::Syncing { .. } => "索引中，完成后显示反链…".to_owned(),
                Phase::Idle if self.loading => "正在更新反链…".to_owned(),
                Phase::Idle if self.matched == 0 => {
                    if self.only_block {
                        "此块还没有反向链接。".into()
                    } else {
                        "还没有其他笔记链接到这里。".into()
                    }
                }
                Phase::Idle if self.matched > self.hits.len() as u64 => format!(
                    "共 {} 条反向链接，显示 {} 条；其余超出上限或来源已变化。",
                    self.matched,
                    self.hits.len()
                ),
                Phase::Idle => format!("{} 条反向链接", self.matched),
            }
        };
        let incoming_count = self.total.map_or_else(|| "…".to_owned(), |total| total.to_string());
        let outgoing_count = if self.outgoing_key.is_none() {
            "…".to_owned()
        } else {
            format!("{}{}", self.outgoing.links.len(), if self.outgoing.truncated { "+" } else { "" })
        };
        let foreground = cx.theme().foreground;
        let tab = |outgoing: bool, label: String| {
            let active = self.show_outgoing == outgoing;
            let id = if outgoing { "references-tab-outgoing" } else { "references-tab-incoming" };
            let active_id =
                if outgoing { "references-outgoing-active" } else { "references-incoming-active" };
            div()
                .id(id)
                .debug_selector(move || id.into())
                .relative()
                .py_3()
                .text_sm()
                .text_color(if active { foreground } else { muted })
                .cursor_pointer()
                .child(label)
                .when(active, |el| {
                    el.child(
                        div()
                            .debug_selector(move || active_id.into())
                            .absolute()
                            .bottom(px(-1.))
                            .left_0()
                            .right_0()
                            .h(px(3.))
                            .rounded_md()
                            .bg(foreground),
                    )
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.show_outgoing != outgoing {
                        this.show_outgoing = outgoing;
                        cx.notify();
                    }
                }))
        };
        let rows: Vec<_> = if self.show_outgoing {
            (0..self.outgoing.links.len()).map(|ix| self.render_outgoing(ix, cx)).collect()
        } else {
            (0..self.hits.len()).map(|ix| self.render_hit(ix, cx)).collect()
        };
        let status = if self.show_outgoing {
            (self.outgoing_loading
                || self.outgoing_key.is_none()
                || self.outgoing.links.is_empty()
                || self.outgoing.truncated)
                .then_some(outgoing_status)
        } else {
            (self.loading
                || self.error.is_some()
                || self.matched == 0
                || self.matched > self.hits.len() as u64
                || !matches!(self.service.read(cx).phase(), Phase::Idle))
            .then_some(status)
        };
        v_flex()
            .id("document-links")
            .debug_selector(|| "document-links".into())
            .w_full()
            .min_w_0()
            .flex_shrink_0()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                h_flex()
                    .w_full()
                    .gap_6()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(tab(false, format!("本文被引用({incoming_count})")))
                    .child(tab(true, format!("本文引用({outgoing_count})"))),
            )
            .when_some(self.selected_block.clone().filter(|_| !self.show_outgoing), |el, block| {
                el.child(
                    gpui_kit::component::h_flex()
                        .px_3()
                        .pt_2()
                        .gap_2()
                        .text_xs()
                        .child(
                            div()
                                .id("backlinks-all")
                                .cursor_pointer()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .when(!self.only_block, |el| el.bg(cx.theme().accent))
                                .child("整篇笔记")
                                .on_click(cx.listener(|this, _, _, cx| this.filter(false, cx))),
                        )
                        .child(
                            div()
                                .id("backlinks-block")
                                .debug_selector(|| "backlinks-block".into())
                                .cursor_pointer()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .when(self.only_block, |el| el.bg(cx.theme().accent))
                                .child(format!("此块的引用 · ^{block}"))
                                .on_click(cx.listener(|this, _, _, cx| this.filter(true, cx))),
                        ),
                )
            })
            .when_some(status, |el, status| el.child(div().py_2().text_xs().text_color(muted).child(status)))
            .child(
                v_flex()
                    .id(if self.show_outgoing { "outgoing-links" } else { "backlink-results" })
                    .debug_selector({
                        let outgoing = self.show_outgoing;
                        move || if outgoing { "outgoing-links" } else { "backlink-results" }.into()
                    })
                    .w_full()
                    .min_w_0()
                    .pt_2()
                    .children(rows),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use aero_core::{Document, Edit, FileStamp, Vault, VaultPath};
    use gpui_kit::{AppContext as _, Entity, TestAppContext};

    use super::{BacklinksView, extract_outgoing};
    use crate::editor::Editor;
    use crate::search::SearchService;

    #[gpui_kit::test]
    fn reference_tabs_show_one_list_and_keep_selection_during_updates(cx: &mut TestAppContext) {
        use gpui_kit::{Modifiers, VisualTestContext};
        let f = fixture(cx);
        f.view.update(cx, |view, _| {
            view.total = Some(1);
            view.matched = 1;
            view.hits = vec![aero_index::Backlink {
                path: VaultPath::new("source.md").unwrap(),
                title: "来源文档".into(),
                range: 0..5,
                body: "正文".into(),
                source: "[[a]]".into(),
                label: "a".into(),
                source_hash: 0,
            }];
            view.outgoing_key = Some((stamp(1), 0));
            view.outgoing = extract_outgoing("[[target|目标文档]]");
        });
        let view = f.view.clone();
        let (_, cx) = cx.add_window_view(|_, _| ReferenceFixture(view));
        let draw = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
        };
        draw(cx);
        let incoming = cx.debug_bounds("references-tab-incoming").expect("incoming tab");
        let outgoing = cx.debug_bounds("references-tab-outgoing").expect("outgoing tab");
        assert_eq!(incoming.top(), outgoing.top());
        assert!(cx.debug_bounds("references-incoming-active").is_some());
        assert!(cx.debug_bounds("backlink-0").is_some());
        assert!(cx.debug_bounds("outgoing-link-0").is_none());
        assert!(cx.debug_bounds("backlink-rich-0").is_none());
        cx.simulate_click(outgoing.center(), Modifiers::default());
        draw(cx);
        assert!(cx.debug_bounds("references-outgoing-active").is_some());
        assert!(cx.debug_bounds("outgoing-link-0").is_some());
        assert!(cx.debug_bounds("backlink-0").is_none());
        f.view.update(cx, |view, cx| view.set_selected_block(Some("one".into()), cx));
        draw(cx);
        assert!(cx.debug_bounds("references-outgoing-active").is_some());
        assert!(cx.debug_bounds("backlinks-block").is_none());
        cx.simulate_click(incoming.center(), Modifiers::default());
        draw(cx);
        assert!(cx.debug_bounds("backlinks-block").is_some());
        assert!(cx.debug_bounds("outgoing-link-0").is_none());
    }

    struct ReferenceFixture(Entity<BacklinksView>);
    impl gpui_kit::Render for ReferenceFixture {
        fn render(
            &mut self,
            _: &mut gpui_kit::Window,
            _: &mut gpui_kit::Context<Self>,
        ) -> impl gpui_kit::IntoElement {
            self.0.clone()
        }
    }

    #[test]
    fn outgoing_note_target_labels_hide_internal_url_encoding() {
        let link = super::OutgoingLink {
            label: "别名".into(),
            url: aero_core::links::encode_note_url("相关资料", Some("使用:说明")),
        };
        assert_eq!(link.target_label(), "相关资料 › 使用:说明");
        let local = super::OutgoingLink {
            label: "跳转".into(),
            url: aero_core::links::encode_note_url("", Some("^one")),
        };
        assert_eq!(local.target_label(), "本文 › ^one");
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        _data: tempfile::TempDir,
        vault: Arc<Vault>,
        view: Entity<BacklinksView>,
    }

    fn fixture(cx: &mut TestAppContext) -> Fixture {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let service = cx.new(|cx| SearchService::new(vault.clone(), data.path().into(), cx));
        // outgoing 不依赖可用索引，也不读取链接目标。
        cx.update(|cx| service.read(cx).close_blocking());
        let view = cx.new(|cx| BacklinksView::new(service, cx));
        view.update(cx, |view, cx| view.set_note(Some(VaultPath::new("a.md").unwrap()), cx));
        Fixture { _dir: dir, _data: data, vault, view }
    }

    fn stamp(hash: u64) -> FileStamp {
        FileStamp { mtime: SystemTime::UNIX_EPOCH, size: 0, hash }
    }

    fn advance(cx: &mut TestAppContext, millis: u64) {
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(millis));
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn outgoing_snapshot_debounces_changes_but_not_selection_notifications(cx: &mut TestAppContext) {
        let f = fixture(cx);
        let mut doc = Document::from_text("[[first]]");
        f.view.update(cx, |view, cx| view.sync_outgoing(stamp(1), doc.snapshot(), cx));
        advance(cx, 100);
        f.view.update(cx, |view, cx| {
            view.set_selected_block(Some("block".into()), cx);
            view.sync_outgoing(stamp(1), doc.snapshot(), cx);
        });
        advance(cx, 49);
        f.view.read_with(cx, |view, _| assert!(view.outgoing.links.is_empty()));
        advance(cx, 1);
        f.view.read_with(cx, |view, _| {
            assert_eq!(view.outgoing.links[0].label, "first");
            assert!(!view.outgoing_loading);
            assert!(view.outgoing_task.is_none());
        });
        doc.edit(Edit::replace(0..doc.len(), "[[second]]")).unwrap();
        f.view.update(cx, |view, cx| view.sync_outgoing(stamp(1), doc.snapshot(), cx));
        advance(cx, 100);
        doc.edit(Edit::replace(0..doc.len(), "[[latest]]")).unwrap();
        f.view.update(cx, |view, cx| view.sync_outgoing(stamp(1), doc.snapshot(), cx));
        advance(cx, 50);
        f.view.read_with(cx, |view, _| assert_eq!(view.outgoing.links[0].label, "first"));
        advance(cx, 100);
        f.view.read_with(cx, |view, _| assert_eq!(view.outgoing.links[0].label, "latest"));
    }

    #[gpui_kit::test]
    fn outgoing_same_note_and_backlink_refresh_preserve_cached_links(cx: &mut TestAppContext) {
        let f = fixture(cx);
        let doc = Document::from_text("[[cached]]");
        f.view.update(cx, |view, cx| view.sync_outgoing(stamp(1), doc.snapshot(), cx));
        advance(cx, 150);
        f.view.update(cx, |view, cx| {
            view.set_note(Some(VaultPath::new("a.md").unwrap()), cx);
            view.refresh(cx);
            view.sync_outgoing(stamp(1), doc.snapshot(), cx);
            assert_eq!(view.outgoing.links[0].label, "cached");
            assert!(view.outgoing_task.is_none());
            assert!(!view.outgoing_loading);
            view.set_note(None, cx);
            view.sync_outgoing(stamp(1), doc.snapshot(), cx);
            assert!(view.outgoing_key.is_none());
            assert!(view.outgoing_task.is_none());
            assert!(view.outgoing.links.is_empty());
        });
    }

    #[gpui_kit::test]
    fn outgoing_stamp_change_reloads_even_when_document_version_restarts(cx: &mut TestAppContext) {
        let f = fixture(cx);
        let first = Document::from_text("[[first]]").snapshot();
        let reload = Document::from_text("[[reloaded]]").snapshot();
        assert_eq!(first.version(), reload.version());
        f.view.update(cx, |view, cx| view.sync_outgoing(stamp(1), first, cx));
        advance(cx, 150);
        f.view.update(cx, |view, cx| view.sync_outgoing(stamp(2), reload, cx));
        advance(cx, 150);
        f.view.read_with(cx, |view, _| assert_eq!(view.outgoing.links[0].label, "reloaded"));
    }

    #[gpui_kit::test]
    fn outgoing_note_switch_clears_results_key_and_pending_work(cx: &mut TestAppContext) {
        let f = fixture(cx);
        f.view.update(cx, |view, cx| {
            view.sync_outgoing(stamp(1), Document::from_text("[[old]]").snapshot(), cx)
        });
        advance(cx, 150);
        f.view.update(cx, |view, cx| {
            view.sync_outgoing(stamp(2), Document::from_text("[[stale]]").snapshot(), cx);
            view.set_selected_block(Some("old-block".into()), cx);
            view.set_note(Some(VaultPath::new("b.md").unwrap()), cx);
            assert!(view.outgoing.links.is_empty());
            assert!(view.outgoing_key.is_none());
            assert!(view.outgoing_task.is_none());
            assert!(!view.outgoing_loading);
            assert!(view.selected_block.is_none());
        });
        advance(cx, 300);
        f.view.read_with(cx, |view, _| assert!(view.outgoing.links.is_empty()));
        f.view.update(cx, |view, cx| {
            view.set_note(Some(VaultPath::new("a.md").unwrap()), cx);
            view.sync_outgoing(stamp(1), Document::from_text("[[old]]").snapshot(), cx);
        });
        advance(cx, 150);
        f.view.read_with(cx, |view, _| assert_eq!(view.outgoing.links[0].label, "old"));
        f.view.update(cx, |view, cx| view.set_note(None, cx));
        advance(cx, 300);
        f.view.read_with(cx, |view, _| assert!(view.outgoing.links.is_empty()));
    }

    #[gpui_kit::test]
    fn outgoing_editor_connection_is_deferred_and_ignores_other_notes(cx: &mut TestAppContext) {
        let f = fixture(cx);
        let editor = cx.new(|cx| {
            Editor::new(
                f.vault.clone(),
                VaultPath::new("a.md").unwrap(),
                Document::from_text("[[connected]]"),
                stamp(1),
                cx,
            )
        });
        // connect_backlinks 在 Editor 的可变借用中调用 set_editor。
        editor.update(cx, |editor, cx| editor.connect_backlinks(f.view.clone(), cx));
        advance(cx, 150);
        f.view.read_with(cx, |view, _| assert_eq!(view.outgoing.links[0].label, "connected"));
        editor.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        f.view.read_with(cx, |view, _| assert!(view.outgoing_task.is_none()));
        editor.update(cx, |editor, cx| editor.set_path(VaultPath::new("other.md").unwrap(), cx));
        cx.run_until_parked();
        f.view.read_with(cx, |view, _| assert!(view.outgoing_task.is_none()));
        f.view.update(cx, |view, cx| {
            view.set_note(Some(VaultPath::new("b.md").unwrap()), cx);
            view.set_editor(&editor, None, cx);
        });
        advance(cx, 300);
        f.view.read_with(cx, |view, _| {
            assert!(view.outgoing.links.is_empty());
            assert!(view.outgoing_key.is_none());
        });
    }

    fn pairs(src: &str) -> Vec<(String, String)> {
        extract_outgoing(src).links.into_iter().map(|link| (link.label, link.url)).collect()
    }

    #[test]
    fn outgoing_links_preserve_order_labels_and_distinct_fragments() {
        let source = "[[笔记#章节|**中文**别名]] [*网站* `代码`](https://example.com)\n\
            [[笔记#章节|重复]] [[#^block]] [[笔记#另一节]]";
        assert_eq!(
            pairs(source),
            vec![
                ("中文别名".into(), aero_core::links::encode_note_url("笔记", Some("章节"))),
                ("网站 代码".into(), "https://example.com".into()),
                ("#^block".into(), aero_core::links::encode_note_url("", Some("^block"))),
                ("笔记#另一节".into(), aero_core::links::encode_note_url("笔记", Some("另一节"))),
            ]
        );
    }

    #[test]
    fn outgoing_links_resolve_references_autolinks_and_escaped_labels() {
        let source = "[**参考**][ref] [简写][] [快捷] <https://auto.example>\n\
            https://bare.example <hi@example.com> [中\\]文](https://escaped.example)\n\n\
            [ref]: https://reference.example \"标题\"\n\
            [简写]: https://collapsed.example\n\
            [快捷]: https://shortcut.example\n\
            [unused]: https://unused.example\n";
        assert_eq!(
            pairs(source),
            vec![
                ("参考".into(), "https://reference.example".into()),
                ("简写".into(), "https://collapsed.example".into()),
                ("快捷".into(), "https://shortcut.example".into()),
                ("https://auto.example".into(), "https://auto.example".into()),
                ("https://bare.example".into(), "https://bare.example".into()),
                ("hi@example.com".into(), "mailto:hi@example.com".into()),
                ("中]文".into(), "https://escaped.example".into()),
            ]
        );
    }

    #[test]
    fn outgoing_links_exclude_code_escapes_and_unused_definitions() {
        let source = r#"`[[inline]] [code](https://inline.example)`

```md
[[fenced]] [code](https://fenced.example)
```

    [[indented]] [code](https://indented.example)

\[[escaped]] \[escaped](relative.md) \[](empty.md)

[unused]: https://unused.example

[[actual]]
"#;
        assert_eq!(pairs(source), vec![("actual".into(), "aero://note/actual".into())]);
    }

    #[test]
    fn outgoing_links_fall_back_to_url_for_empty_labels() {
        assert_eq!(
            pairs("[](empty.md) [ ](space.md) [[笔记|]] [][ref]\n\n[ref]: reference.md\n"),
            vec![
                ("empty.md".into(), "empty.md".into()),
                ("space.md".into(), "space.md".into()),
                (
                    aero_core::links::encode_note_url("笔记", None),
                    aero_core::links::encode_note_url("笔记", None)
                ),
                ("reference.md".into(), "reference.md".into()),
            ]
        );
    }

    #[test]
    fn outgoing_links_limit_counts_unique_urls_not_occurrences() {
        let source =
            (0..200).map(|i| format!("[first {i}](note-{i}.md) [again](note-{i}.md)\n")).collect::<String>();
        let page = extract_outgoing(&source);
        assert_eq!(page.links.len(), 200);
        assert!(!page.truncated);
        assert_eq!(page.links[199].label, "first 199");
        let page = extract_outgoing(&format!("{source}[more](overflow.md)"));
        assert_eq!(page.links.len(), 200);
        assert!(page.truncated);
    }
}
