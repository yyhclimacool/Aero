//! 右侧反链面板：按来源分组，随索引更新，点击跳转到引用所在块。

use aero_core::VaultPath;
use aero_index::Backlink;
use gpui_kit::base::text::{TextView, TextViewStyle};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::search::{Phase, SearchService};

const LIMIT: usize = 200;

pub struct BacklinkOpened(pub Backlink);
impl EventEmitter<BacklinkOpened> for BacklinksView {}

pub struct BacklinksView {
    service: Entity<SearchService>,
    note: Option<VaultPath>,
    hits: Vec<Backlink>,
    total: Option<u64>,
    matched: u64,
    selected_block: Option<String>,
    only_block: bool,
    editor_subscription: Option<Subscription>,
    generation: u64,
    loading: bool,
    error: Option<String>,
    query: Option<Task<()>>,
    scroll: ScrollHandle,
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
            selected_block: None,
            only_block: false,
            editor_subscription: None,
            generation: 0,
            loading: false,
            error: None,
            query: None,
            scroll: ScrollHandle::new(),
            _subscription: subscription,
        }
    }

    pub fn set_note(&mut self, note: Option<VaultPath>, cx: &mut Context<Self>) {
        if self.note == note {
            return;
        }
        self.note = note;
        self.selected_block = None;
        self.only_block = false;
        self.editor_subscription = None;
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.refresh(cx);
    }

    pub fn set_editor(
        &mut self,
        editor: &Entity<crate::editor::Editor>,
        selected_block: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.set_selected_block(selected_block, cx);
        self.editor_subscription = Some(cx.observe(editor, |this, editor, cx| {
            if this.note.as_ref() != Some(editor.read(cx).path()) {
                return;
            }
            this.set_selected_block(editor.read(cx).selected_reference(cx), cx);
        }));
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

    pub fn count_label(&self, cx: &App) -> String {
        if let Some(total) = self.total {
            format!("{total} 条反链")
        } else if self.error.is_some() || matches!(self.service.read(cx).phase(), Phase::Failed(_)) {
            "反链不可用".into()
        } else {
            "反链…".into()
        }
    }

    fn filter(&mut self, only_block: bool, cx: &mut Context<Self>) {
        if self.only_block == only_block {
            return;
        }
        self.only_block = only_block;
        self.scroll.set_offset(point(px(0.), px(0.)));
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
        let hit = &self.hits[ix];
        let theme = cx.theme();
        let hover = theme.list_hover;
        let source = hit.source.clone();
        let hit = hit.clone();
        div()
            .id(SharedString::from(format!("backlink-{ix}")))
            .debug_selector(move || format!("backlink-{ix}"))
            .w_full()
            .px_3()
            .py_2()
            .rounded_md()
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .child(rich_snippet(ix, &source, cx))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(BacklinkOpened(hit.clone()))))
            .into_any_element()
    }
}

/// 来源块按笔记预览渲染：wikilink 变成链接，强调和代码保留。点整行仍跳到来源，片段里的链接不另开。
fn rich_snippet(ix: usize, source: &str, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let style = TextViewStyle::default()
        .with_foreground(theme.muted_foreground)
        .with_muted_foreground(theme.muted_foreground)
        .with_link(theme.link)
        .with_code_background(theme.muted)
        .with_border(theme.border)
        .with_paragraph_gap(rems(0.))
        .with_dark(theme.mode.is_dark());
    div()
        .debug_selector(move || format!("backlink-rich-{ix}"))
        .w_full()
        .text_sm()
        .line_height(relative(1.4))
        .child(
            TextView::markdown(
                SharedString::from(format!("backlink-md-{ix}")),
                aero_core::links::to_markdown(source),
            )
            .style(style)
            .selectable(false)
            .on_link_click(|_, _, _, cx| cx.stop_propagation()),
        )
        .into_any_element()
}

impl Render for BacklinksView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
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
        let mut rows = Vec::new();
        let mut previous = None;
        for (ix, hit) in self.hits.iter().enumerate() {
            if previous != Some(&hit.path) {
                previous = Some(&hit.path);
                rows.push(
                    v_flex()
                        .w_full()
                        .px_3()
                        .pt_3()
                        .pb_1()
                        .child(
                            div()
                                .truncate()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child(hit.title.clone()),
                        )
                        .child(
                            div().truncate().text_xs().text_color(muted).child(hit.path.as_str().to_owned()),
                        )
                        .into_any_element(),
                );
            }
            rows.push(self.render_hit(ix, cx));
        }
        v_flex()
            .size_full()
            .when_some(self.selected_block.clone(), |el, block| {
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
            .child(div().px_3().py_2().text_xs().text_color(muted).child(status))
            .child(
                v_flex()
                    .id("backlink-results")
                    .flex_1()
                    .min_h(px(0.))
                    .px_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children(rows)
                    .vertical_scrollbar(&self.scroll),
            )
    }
}
