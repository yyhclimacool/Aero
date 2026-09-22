//! 左侧栏的标签视图（`ui_design.md` 4.3）：嵌套树 + 计数，点击到搜索视图执行 `tag:#…`。

use std::collections::HashSet;
use std::sync::Arc;

use aero_core::Vault;
use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme as _, Icon, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::search::{Phase, SearchService};
use crate::tag_tree::{self, Node};

const ROW_HEIGHT: f32 = 28.;

pub enum TagsViewEvent {
    /// 在搜索视图执行 `tag:#{tag}`
    Search(String),
}

impl EventEmitter<TagsViewEvent> for TagsView {}

struct Row {
    path: String,
    name: String,
    count: u64,
    depth: usize,
    expandable: bool,
    expanded: bool,
}

pub struct TagsView {
    vault: Arc<Vault>,
    service: Entity<SearchService>,
    nodes: Vec<Node>,
    collapsed: HashSet<String>,
    generation: u64,
    loading: bool,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl TagsView {
    pub fn new(service: Entity<SearchService>, vault: Arc<Vault>, cx: &mut Context<Self>) -> Self {
        let service_sub = cx.observe(&service, |this, service, cx| {
            if matches!(service.read(cx).phase(), Phase::Idle | Phase::Failed(_)) {
                this.reload(cx);
            }
        });
        let mut view = TagsView {
            vault,
            service,
            nodes: Vec::new(),
            collapsed: HashSet::new(),
            generation: 0,
            loading: true,
            error: None,
            _subscriptions: vec![service_sub],
        };
        view.reload(cx);
        view
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.loading = self.nodes.is_empty();
        let task = self.service.read(cx).vault_tags(self.vault.clone(), cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(counts) => {
                        this.error = None;
                        this.nodes = tag_tree::build(&counts);
                    }
                    Err(err) => this.error = Some(err),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        flatten(&self.nodes, &self.collapsed, 0, &mut rows);
        rows
    }

    fn toggle(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.collapsed.remove(path) {
            self.collapsed.insert(path.to_owned());
        }
        cx.notify();
    }

    #[cfg(test)]
    pub fn labels(&self) -> Vec<(String, u64, usize)> {
        self.rows().into_iter().map(|r| (r.path, r.count, r.depth)).collect()
    }
}

fn flatten(nodes: &[Node], collapsed: &HashSet<String>, depth: usize, out: &mut Vec<Row>) {
    for node in nodes {
        let expandable = !node.children.is_empty();
        let expanded = expandable && !collapsed.contains(&node.path);
        out.push(Row {
            path: node.path.clone(),
            name: node.name.clone(),
            count: node.count,
            depth,
            expandable,
            expanded,
        });
        if expanded {
            flatten(&node.children, collapsed, depth + 1, out);
        }
    }
}

impl Render for TagsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let hover = theme.list_hover;
        let rows = self.rows();
        let body: AnyElement = if let Some(err) = &self.error {
            div().p_3().text_sm().text_color(theme.danger).child(err.clone()).into_any_element()
        } else if self.loading {
            div().p_3().text_sm().text_color(muted).child("正在读取标签…").into_any_element()
        } else if rows.is_empty() {
            div()
                .p_3()
                .text_sm()
                .text_color(muted)
                .child("还没有标签。在笔记里写 #标签，或在属性里加 tags。")
                .into_any_element()
        } else {
            v_flex()
                .id("tags-tree")
                .debug_selector(|| "tags-tree".into())
                .size_full()
                .overflow_y_scroll()
                .p_1()
                .children(rows.into_iter().enumerate().map(|(i, row)| {
                    let path = row.path.clone();
                    let search = path.clone();
                    h_flex()
                        .id(("tag-row", i))
                        .debug_selector({
                            let path = path.clone();
                            move || format!("tag-row:{path}")
                        })
                        .h(px(ROW_HEIGHT))
                        .pl(px(8. + row.depth as f32 * 14.))
                        .pr_2()
                        .gap_1()
                        .items_center()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .text_sm()
                        .child(
                            div()
                                .id(("tag-toggle", i))
                                .debug_selector({
                                    let path = path.clone();
                                    move || format!("tag-toggle:{path}")
                                })
                                .w(px(14.))
                                .h(px(14.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .when(row.expandable, |el| {
                                    el.cursor_pointer()
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener({
                                                let path = path.clone();
                                                move |this, _, _, cx| {
                                                    cx.stop_propagation();
                                                    this.toggle(&path, cx);
                                                }
                                            }),
                                        )
                                        .child(
                                            Icon::new(if row.expanded {
                                                IconName::ChevronDown
                                            } else {
                                                IconName::ChevronRight
                                            })
                                            .size(px(14.))
                                            .text_color(muted),
                                        )
                                }),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(row.name))
                        .child(div().flex_none().text_xs().text_color(muted).child(row.count.to_string()))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |_, _, _, cx| {
                                cx.emit(TagsViewEvent::Search(search.clone()));
                            }),
                        )
                        .into_any_element()
                }))
                .into_any_element()
        };
        v_flex().id("tags-view").debug_selector(|| "tags-view".into()).size_full().child(body)
    }
}
