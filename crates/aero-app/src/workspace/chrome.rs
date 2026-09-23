//! 窗口外框（`ui_design.md` 2.1 / 2.3）：标题栏、三栏与分割线、编辑区顶部的路径、侧栏与面板的标题行。
//!
//! 标题栏与下面三栏对齐：左侧栏上方是 文件 / 搜索 / 标签 三个视图按钮和折叠按钮，中间是标签条，
//! 右侧面板上方是 反链 / 大纲 和折叠按钮；收起的一侧只留折叠按钮。
//! 两侧宽度固定，窗口变宽变窄只改编辑区；窗口太窄时先压右侧面板、再压左侧栏，都不小于各自的最小宽度，
//! 存下来的宽度不变，窗口放大后原样回来。
//! 拖分割线时整个窗口保持左右调整的光标、编辑区不响应悬停；往里拖过最小宽度一段就收起，拖回来再展开；
//! 双击分割线恢复默认宽度。

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Selectable as _, Sizable as _, TitleBar, h_flex, v_flex};

use super::tabs::stop_title_drag;
use super::*;

pub(super) const SIDEBAR_DEFAULT: f32 = 260.;
const SIDEBAR_RANGE: (f32, f32) = (200., 480.);
pub(super) const PANEL_DEFAULT: f32 = 300.;
const PANEL_RANGE: (f32, f32) = (220., 480.);
/// 编辑区至少留这么宽
const EDITOR_MIN: f32 = 360.;
/// 比最小宽度再往里拖这么多就收起
const COLLAPSE_SLOP: f32 = 60.;
/// 分割线能按住的宽度；看得见的线是侧栏自己的 1 px 边框
const HANDLE_WIDTH: f32 = 8.;
/// 交通灯占的宽度
const TRAFFIC_LIGHTS: f32 = 80.;
/// 编辑区顶部路径行、侧栏标题行的高度
const HEADER_HEIGHT: f32 = 32.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Side {
    Sidebar,
    Panel,
}

pub(super) struct SplitDrag {
    side: Side,
    start_x: Pixels,
    start_width: Pixels,
}

impl Workspace {
    /// 实际画出来的两侧宽度（收起为 0）。
    pub(super) fn column_widths(&self, window: &Window) -> (Pixels, Pixels) {
        let total = window.viewport_size().width;
        let mut sidebar = if self.sidebar_visible { self.sidebar_width } else { px(0.) };
        let mut panel = if self.panel_visible { self.panel_width } else { px(0.) };
        let mut over = sidebar + panel + px(EDITOR_MIN) - total;
        if over > px(0.) && self.panel_visible {
            let give = over.min(panel - px(PANEL_RANGE.0)).max(px(0.));
            panel -= give;
            over -= give;
        }
        if over > px(0.) && self.sidebar_visible {
            sidebar -= over.min(sidebar - px(SIDEBAR_RANGE.0)).max(px(0.));
        }
        (sidebar, panel)
    }

    pub(super) fn show_sidebar(&mut self, tab: SidebarTab, cx: &mut Context<Self>) {
        self.sidebar_tab = tab;
        if !self.sidebar_visible {
            self.sidebar_visible = true;
            self.save_layout();
        }
        cx.notify();
    }

    pub(super) fn show_panel(&mut self, tab: ContextTab, cx: &mut Context<Self>) {
        self.context_tab = tab;
        if !self.panel_visible {
            self.panel_visible = true;
            self.save_layout();
        }
        cx.notify();
    }

    pub(super) fn save_layout(&mut self) {
        self.state.sidebar_visible = Some(self.sidebar_visible);
        self.state.panel_visible = Some(self.panel_visible);
        self.state.sidebar_width = Some(self.sidebar_width.as_f32());
        self.state.panel_width = Some(self.panel_width.as_f32());
        self.state.save(&self.data_dir);
    }

    // ---- 分割线 ----

    fn render_split_handle(&self, side: Side, x: Pixels, cx: &mut Context<Self>) -> impl IntoElement {
        let ring = cx.theme().ring;
        let dragging = self.split.as_ref().is_some_and(|drag| drag.side == side);
        let id = match side {
            Side::Sidebar => "split-sidebar",
            Side::Panel => "split-panel",
        };
        div()
            .id(id)
            .debug_selector(move || id.into())
            .absolute()
            .top_0()
            .bottom_0()
            .left(x - px(HANDLE_WIDTH / 2.))
            .w(px(HANDLE_WIDTH))
            .flex()
            .justify_center()
            .cursor_col_resize()
            .group(id)
            .child(
                div()
                    .w(px(2.))
                    .h_full()
                    .when(dragging, |s| s.bg(ring))
                    .group_hover(id, move |s| s.bg(ring.opacity(0.6))),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.prevent_default();
                    if event.click_count >= 2 {
                        this.split = None;
                        match side {
                            Side::Sidebar => this.sidebar_width = px(SIDEBAR_DEFAULT),
                            Side::Panel => this.panel_width = px(PANEL_DEFAULT),
                        }
                        this.save_layout();
                    } else {
                        let (sidebar, panel) = this.column_widths(window);
                        let start_width = if side == Side::Sidebar { sidebar } else { panel };
                        this.split = Some(SplitDrag { side, start_x: event.position.x, start_width });
                    }
                    cx.notify();
                }),
            )
    }

    /// 拖动中盖住整个窗口的一层：光标一直是左右调整，下面的编辑区不响应悬停和选字。
    fn render_split_overlay(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        self.split.as_ref()?;
        Some(
            deferred(
                div()
                    .id("split-overlay")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .cursor_col_resize()
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                        this.drag_split(event, window, cx)
                    }))
                    .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| this.finish_split(cx)))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _, _, cx| this.finish_split(cx))),
            )
            .with_priority(10),
        )
    }

    pub(super) fn drag_split(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.split.as_ref() else { return };
        // 在窗口外松开、没收到 mouse up：下一次移动时按拖动结束处理
        if event.pressed_button != Some(MouseButton::Left) {
            self.finish_split(cx);
            return;
        }
        let (side, start, dx) = (drag.side, drag.start_width, event.position.x - drag.start_x);
        let total = window.viewport_size().width;
        let (sidebar, panel) = self.column_widths(window);
        let (raw, (min, max), room) = match side {
            Side::Sidebar => (start + dx, SIDEBAR_RANGE, total - panel - px(EDITOR_MIN)),
            Side::Panel => (start - dx, PANEL_RANGE, total - sidebar - px(EDITOR_MIN)),
        };
        let visible = raw >= px(min - COLLAPSE_SLOP);
        let width = raw.clamp(px(min), px(max).min(room).max(px(min)));
        match side {
            Side::Sidebar => {
                self.sidebar_visible = visible;
                if visible {
                    self.sidebar_width = width;
                }
            }
            Side::Panel => {
                self.panel_visible = visible;
                if visible {
                    self.panel_width = width;
                }
            }
        }
        cx.notify();
    }

    pub(super) fn finish_split(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.split.take() else { return };
        // 拖到收起：下次展开回到拖之前的宽度
        match drag.side {
            Side::Sidebar if !self.sidebar_visible => self.sidebar_width = drag.start_width,
            Side::Panel if !self.panel_visible => self.panel_width = drag.start_width,
            _ => {}
        }
        self.save_layout();
        cx.notify();
    }

    pub(super) fn render_columns(
        &self,
        open: &VaultSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (sidebar, panel) = self.column_widths(window);
        let total = window.viewport_size().width;
        h_flex()
            .id("columns")
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .when(self.sidebar_visible, |el| {
                el.child(
                    div()
                        .debug_selector(|| "sidebar".into())
                        .flex_none()
                        .w(sidebar)
                        .h_full()
                        .child(self.render_sidebar(open, cx)),
                )
            })
            .child(
                div()
                    .debug_selector(|| "editor-column".into())
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.render_editor_area(open, cx)),
            )
            .when(self.panel_visible, |el| {
                el.child(
                    div()
                        .debug_selector(|| "context-panel".into())
                        .flex_none()
                        .w(panel)
                        .h_full()
                        .child(self.render_context(open, cx)),
                )
            })
            .when(self.sidebar_visible, |el| el.child(self.render_split_handle(Side::Sidebar, sidebar, cx)))
            .when(self.panel_visible, |el| el.child(self.render_split_handle(Side::Panel, total - panel, cx)))
            .children(self.render_split_overlay(cx))
    }

    // ---- 标题栏 ----

    pub(super) fn render_title_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (sidebar_bg, sidebar_border, muted) =
            (theme.sidebar, theme.sidebar_border, theme.muted_foreground);
        // 全屏时没有交通灯，标题栏自己留 12 px
        let lights = px(if window.is_fullscreen() { 0. } else { TRAFFIC_LIGHTS });
        let Some(open) = self.open.as_ref() else {
            return TitleBar::new().child(div().text_sm().text_color(muted).child("Aero"));
        };
        let (sidebar, panel) = self.column_widths(window);
        let sidebar_toggle = stop_title_drag(
            Button::new("toggle-sidebar")
                .ghost()
                .xsmall()
                .icon(IconName::PanelLeft)
                .tooltip_with_action(
                    if self.sidebar_visible { "收起左侧栏" } else { "展开左侧栏" },
                    &ToggleSidebar,
                    None,
                )
                .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleSidebar), cx)),
        );
        let panel_toggle = stop_title_drag(
            Button::new("toggle-panel")
                .ghost()
                .xsmall()
                .icon(IconName::PanelRight)
                .tooltip_with_action(
                    if self.panel_visible { "收起右侧面板" } else { "展开右侧面板" },
                    &TogglePanel,
                    None,
                )
                .on_click(|_, window, cx| window.dispatch_action(Box::new(TogglePanel), cx)),
        );
        let left = if self.sidebar_visible {
            let view = |id: &'static str,
                        icon: IconName,
                        label: &'static str,
                        tab: SidebarTab,
                        action: &dyn Action| {
                stop_title_drag(
                    Button::new(id)
                        .ghost()
                        .xsmall()
                        .icon(icon)
                        .selected(self.sidebar_tab == tab)
                        .tooltip_with_action(label, action, None)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.show_sidebar(tab, cx);
                            if tab == SidebarTab::Search
                                && let Some(open) = this.open.as_ref()
                            {
                                open.search_view.update(cx, |v, cx| v.focus(window, cx));
                            }
                        })),
                )
            };
            h_flex()
                .flex_none()
                .w(sidebar)
                .h_full()
                .pl(lights)
                .pr_1()
                .gap_0p5()
                .items_center()
                .bg(sidebar_bg)
                .border_r_1()
                .border_color(sidebar_border)
                .child(view("sidebar-files", IconName::Folder, "文件", SidebarTab::Files, &OpenFiles))
                .child(view("sidebar-search", IconName::Search, "搜索", SidebarTab::Search, &OpenSearch))
                .child(view("sidebar-tags", IconName::Tag, "标签", SidebarTab::Tags, &OpenTags))
                .child(div().flex_1())
                .child(sidebar_toggle)
        } else {
            h_flex().flex_none().h_full().pl(lights).pr_1().items_center().child(sidebar_toggle)
        };
        let right = if self.panel_visible {
            let view = |id: &'static str,
                        icon: IconName,
                        label: &'static str,
                        tab: ContextTab,
                        action: &dyn Action| {
                stop_title_drag(
                    Button::new(id)
                        .ghost()
                        .xsmall()
                        .icon(icon)
                        .selected(self.context_tab == tab)
                        .tooltip_with_action(label, action, None)
                        .on_click(cx.listener(move |this, _, _, cx| this.show_panel(tab, cx))),
                )
            };
            h_flex()
                .flex_none()
                .w(panel)
                .h_full()
                .px_1()
                .gap_0p5()
                .items_center()
                .bg(sidebar_bg)
                .border_l_1()
                .border_color(sidebar_border)
                .child(view("panel-backlinks", IconName::Link, "反链", ContextTab::Backlinks, &ShowBacklinks))
                .child(view(
                    "panel-outline",
                    IconName::TableOfContents,
                    "大纲",
                    ContextTab::Outline,
                    &ShowOutline,
                ))
                .child(div().flex_1())
                .child(panel_toggle)
        } else {
            h_flex().flex_none().h_full().px_1().items_center().child(panel_toggle)
        };
        TitleBar::new().pl_0().child(
            h_flex()
                .size_full()
                .items_center()
                .child(left)
                .child(self.render_tab_strip(open, cx))
                .child(right),
        )
    }

    // ---- 编辑区 ----

    /// 当前笔记的路径（`vault › 文件夹 › 笔记.md`）：点文件夹在文件树里定位到它，点笔记定位到笔记；右边是收藏开关。
    pub(super) fn render_breadcrumb(
        &self,
        open: &VaultSession,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let path = open.editor()?.read(cx).path().clone();
        let theme = cx.theme();
        let (muted, foreground, hover, warning) =
            (theme.muted_foreground, theme.foreground, theme.list_hover, theme.warning);
        let crumb = |id: SharedString, label: String, current: bool| {
            div()
                .id(id.clone())
                .debug_selector(move || id.to_string())
                .flex_none()
                .max_w(px(240.))
                .truncate()
                .px_1()
                .py_0p5()
                .rounded_sm()
                .cursor_pointer()
                .when(current, |s| s.text_color(foreground))
                .hover(move |s| s.bg(hover).text_color(foreground))
                .child(label)
        };
        let separator = || Icon::new(IconName::ChevronRight).size(px(12.)).text_color(muted);
        let name =
            open.vault.root().file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut row = h_flex()
            .id("breadcrumb")
            .debug_selector(|| "breadcrumb".into())
            .flex_none()
            .h(px(HEADER_HEIGHT))
            .pl_3()
            .pr_2()
            .gap_0p5()
            .items_center()
            .text_xs()
            .text_color(muted)
            .child(crumb("crumb-vault".into(), name, false).on_click(cx.listener(|this, _, _, cx| {
                this.show_sidebar(SidebarTab::Files, cx);
            })));
        let mut prefix = String::new();
        for seg in path.parent().split('/').filter(|s| !s.is_empty()) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(seg);
            let dir = prefix.clone();
            row = row.child(separator()).child(
                crumb(format!("crumb:{dir}").into(), seg.to_owned(), false).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.show_sidebar(SidebarTab::Files, cx);
                        if let Some(open) = this.open.as_ref() {
                            open.tree.update(cx, |t, cx| t.reveal_folder(&dir, cx));
                        }
                    },
                )),
            );
        }
        let favorite = self.favorites().contains(&path);
        let note = path.clone();
        Some(
            row.child(separator())
                .child(crumb("crumb-note".into(), path.file_name().to_owned(), true).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.show_sidebar(SidebarTab::Files, cx);
                        if let Some(open) = this.open.as_ref() {
                            open.tree.update(cx, |t, cx| t.select(Some(note.clone()), cx));
                        }
                    },
                )))
                .child(div().flex_1())
                .child(
                    Button::new("crumb-favorite")
                        .ghost()
                        .xsmall()
                        .icon(
                            Icon::new(if favorite { IconName::StarFill } else { IconName::Star })
                                .text_color(if favorite { warning } else { muted }),
                        )
                        .tooltip_with_action(
                            if favorite { "取消收藏" } else { "收藏" },
                            &ToggleFavorite,
                            None,
                        )
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleFavorite), cx)),
                )
                .into_any_element(),
        )
    }

    /// 空白的「新标签页」：新建、打开、关闭三条入口（与 Obsidian 的新标签页一致）。
    pub(super) fn render_empty_tab(&self, open: &VaultSession, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (link, hover, muted) = (theme.link, theme.link_hover, theme.muted_foreground);
        let entry = |id: &'static str, label: &'static str, action: Box<dyn Action>| {
            div()
                .id(id)
                .debug_selector(move || id.into())
                .text_color(link)
                .cursor_pointer()
                .hover(move |s| s.text_color(hover))
                .child(label)
                .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
        };
        v_flex()
            .id("empty-tab")
            .debug_selector(|| "empty-tab".into())
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .text_sm()
            .child(entry("empty-tab-new-note", "新建笔记（⌘N）", Box::new(NewNote)))
            .child(entry("empty-tab-open", "打开笔记…（⌘P）", Box::new(QuickOpen)))
            .when(open.tabs.len() > 1, |el| {
                el.child(entry("empty-tab-close", "关闭标签页（⌘W）", Box::new(CloseTab)))
            })
            .child(div().mt_4().text_xs().text_color(muted).child("从左侧文件树打开的笔记会放进这一页"))
    }

    /// 侧栏 / 面板顶部的标题行：视图名，右边放这个视图自己的按钮。
    pub(super) fn render_pane_header(
        &self,
        title: &'static str,
        buttons: Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .flex_none()
            .h(px(HEADER_HEIGHT))
            .pl_3()
            .pr_1()
            .gap_0p5()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(cx.theme().muted_foreground)
                    .child(title),
            )
            .children(buttons)
    }

    /// 文件视图标题行上的按钮：新建笔记、全部收起、定位当前笔记。
    pub(super) fn explorer_buttons(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        vec![
            Button::new("explorer-new-note")
                .ghost()
                .xsmall()
                .icon(IconName::FilePlus)
                .tooltip_with_action("新建笔记", &NewNote, None)
                .on_click(|_, window, cx| window.dispatch_action(Box::new(NewNote), cx))
                .into_any_element(),
            Button::new("explorer-collapse")
                .ghost()
                .xsmall()
                .icon(IconName::ChevronsDownUp)
                .tooltip("全部收起")
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(open) = this.open.as_ref() {
                        open.tree.update(cx, |t, cx| t.collapse_all(cx));
                    }
                }))
                .into_any_element(),
            Button::new("explorer-locate")
                .ghost()
                .xsmall()
                .icon(IconName::LocateFixed)
                .tooltip_with_action("定位当前笔记", &OpenFiles, None)
                .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenFiles), cx))
                .into_any_element(),
        ]
    }

    // ---- 收藏 ----

    /// 当前 vault 的收藏（`state.toml` 里按 vault_id 记着）。
    pub(super) fn favorites(&self) -> Vec<VaultPath> {
        let Some(open) = self.open.as_ref() else { return Vec::new() };
        self.state
            .vaults
            .get(open.vault.id())
            .map(|ui| ui.favorites.iter().filter_map(|p| VaultPath::new(p).ok()).collect())
            .unwrap_or_default()
    }

    pub(super) fn set_favorites(&mut self, favorites: Vec<VaultPath>, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let ui = self.state.vaults.entry(open.vault.id().to_owned()).or_default();
        ui.favorites = favorites.iter().map(|p| p.as_str().to_owned()).collect();
        self.state.save(&self.data_dir);
        open.tree.update(cx, |t, cx| t.set_favorites(favorites, cx));
        cx.notify();
    }

    pub(super) fn toggle_favorite(&mut self, path: VaultPath, cx: &mut Context<Self>) {
        let mut favorites = self.favorites();
        match favorites.iter().position(|p| p == &path) {
            Some(ix) => {
                favorites.remove(ix);
            }
            None => favorites.push(path),
        }
        self.set_favorites(favorites, cx);
    }

    pub(super) fn on_toggle_favorite(&mut self, _: &ToggleFavorite, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.open.as_ref().and_then(|o| o.editor()).map(|e| e.read(cx).path().clone()) {
            self.toggle_favorite(path, cx);
        }
    }

    pub(super) fn on_toggle_favorite_note(
        &mut self,
        action: &ToggleFavoriteNote,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Ok(path) = VaultPath::new(&action.0) {
            self.toggle_favorite(path, cx);
        }
    }

    pub(super) fn on_open_in_new_tab(
        &mut self,
        action: &OpenInNewTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Ok(path) = VaultPath::new(&action.0) {
            self.open_note_in(path, OpenIn::NewTab, window, cx);
        }
    }

    pub(super) fn on_show_backlinks(&mut self, _: &ShowBacklinks, _: &mut Window, cx: &mut Context<Self>) {
        self.show_panel(ContextTab::Backlinks, cx);
    }

    pub(super) fn on_show_outline(&mut self, _: &ShowOutline, _: &mut Window, cx: &mut Context<Self>) {
        self.show_panel(ContextTab::Outline, cx);
    }
}
