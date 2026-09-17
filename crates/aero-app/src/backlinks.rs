//! 右侧反链面板：按来源分组，随索引更新，点击跳转到引用所在块。

use aero_core::VaultPath;
use aero_index::Backlink;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::search::{Phase, SearchService};
use crate::search_view::snippet;

const LIMIT: usize = 200;

pub struct BacklinkOpened(pub Backlink);
impl EventEmitter<BacklinkOpened> for BacklinksView {}

pub struct BacklinksView {
    service: Entity<SearchService>,
    note: Option<VaultPath>,
    hits: Vec<Backlink>,
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
        self.hits.clear();
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.refresh(cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.query = None;
        self.error = None;
        self.loading = false;
        cx.notify();
        let Some(note) = self.note.clone() else { return };
        if !matches!(self.service.read(cx).phase(), Phase::Idle) {
            return;
        }
        self.loading = true;
        let generation = self.generation;
        let task = self.service.read(cx).backlinks(note, LIMIT, cx);
        self.query = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(hits) => this.hits = hits,
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
        let (text, ranges) = snippet(&hit.body, std::slice::from_ref(&hit.label));
        let theme = cx.theme();
        let hover = theme.list_hover;
        let highlight = HighlightStyle {
            color: Some(theme.link),
            font_weight: Some(FontWeight::MEDIUM),
            ..Default::default()
        };
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
            .text_sm()
            .text_color(theme.muted_foreground)
            .line_height(relative(1.4))
            .child(StyledText::new(text).with_highlights(ranges.into_iter().map(|r| (r, highlight))))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(BacklinkOpened(hit.clone()))))
            .into_any_element()
    }
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
                Phase::Idle if self.hits.is_empty() => "还没有其他笔记链接到这里。".to_owned(),
                Phase::Idle if self.hits.len() >= LIMIT => format!("前 {LIMIT} 条反向链接"),
                Phase::Idle => format!("{} 条反向链接", self.hits.len()),
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
        v_flex().size_full().child(div().px_3().py_2().text_xs().text_color(muted).child(status)).child(
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
