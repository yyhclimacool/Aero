//! `⌘,` 设置：左侧分组，右侧是已经能改的偏好。
//! 搜索与隐私仍由 [`crate::search_settings`] 管，这里只把它嵌进来。

use gpui_kit::StatefulInteractiveElement as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::paths::LinkPreviewMode;
use crate::search_settings::{self, SearchSettings};
use crate::workspace::Workspace;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Editor,
    Search,
    About,
}

pub fn open(workspace: Entity<Workspace>, window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| Settings::new(workspace, cx));
    window.open_dialog(cx, move |dialog, window, _| {
        dialog
            .title("设置")
            .width(px(680.).min(window.viewport_size().width - px(32.)))
            .child(view.clone())
            .footer(
                Button::new("close-settings").label("完成").on_click(|_, window, cx| window.close_dialog(cx)),
            )
    });
}

pub fn open_about(window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, |dialog, _, _| {
        dialog
            .title("关于 Aero")
            .width(px(360.))
            .child(info_panel(
                "about-panel",
                vec![format!("Aero {}", env!("CARGO_PKG_VERSION")), "本地 Markdown 笔记。".into()],
            ))
            .footer(Button::new("close-about").label("好").on_click(|_, window, cx| window.close_dialog(cx)))
    });
}

pub fn open_updates(window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, |dialog, _, _| {
        dialog
            .title("检查更新")
            .width(px(420.))
            .child(info_panel(
                "update-panel",
                vec![
                    format!("当前版本 {}", env!("CARGO_PKG_VERSION")),
                    "这个构建没有配置更新源，不会联网检查。换版本时替换应用程序即可。".into(),
                ],
            ))
            .footer(
                Button::new("close-updates").label("好").on_click(|_, window, cx| window.close_dialog(cx)),
            )
    });
}

fn info_panel(id: &'static str, lines: Vec<String>) -> impl IntoElement {
    v_flex()
        .id(id)
        .debug_selector(move || id.into())
        .gap_2()
        .text_sm()
        .children(lines.into_iter().map(|line| div().child(line)))
}

struct Settings {
    workspace: Entity<Workspace>,
    section: Section,
    search: Option<(EntityId, Entity<SearchSettings>)>,
    _subscription: Subscription,
}

impl Settings {
    fn new(workspace: Entity<Workspace>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&workspace, |_, _, cx| cx.notify());
        Self { workspace, section: Section::Editor, search: None, _subscription: subscription }
    }

    fn search_view(&mut self, cx: &mut Context<Self>) -> Option<Entity<SearchSettings>> {
        let service = self.workspace.read(cx).semantic_service()?;
        let id = service.entity_id();
        if self.search.as_ref().is_none_or(|(bound, _)| *bound != id) {
            self.search = Some((id, search_settings::view(service, cx)));
        }
        self.search.as_ref().map(|(_, view)| view.clone())
    }

    fn nav(
        &self,
        id: &'static str,
        label: &'static str,
        section: Section,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.section == section;
        let hover = cx.theme().list_hover;
        let active = cx.theme().accent.opacity(0.16);
        div()
            .id(id)
            .debug_selector(move || id.into())
            .w_full()
            .px_3()
            .py(px(6.))
            .rounded_md()
            .text_sm()
            .cursor_pointer()
            .when(selected, move |el| el.bg(active))
            .hover(move |style| style.bg(hover))
            .child(label)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.section = section;
                cx.notify();
            }))
            .into_any_element()
    }
}

impl Render for Settings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = self.workspace.clone();
        let focus_source = workspace.read(cx).focus_shows_source();
        let preview = workspace.read(cx).link_preview();
        let muted = cx.theme().muted_foreground;
        let sidebar = cx.theme().sidebar;
        let border = cx.theme().border;
        let search = if self.section == Section::Search { self.search_view(cx) } else { None };
        let height = px(420.).min(window.viewport_size().height * 0.7);

        h_flex()
            .id("settings-panel")
            .debug_selector(|| "settings-panel".into())
            .w_full()
            .h(height)
            .child(
                v_flex()
                    .w(px(160.))
                    .h_full()
                    .p_2()
                    .gap_1()
                    .bg(sidebar)
                    .border_r_1()
                    .border_color(border)
                    .child(self.nav("settings-nav-editor", "编辑器", Section::Editor, cx))
                    .child(self.nav("settings-nav-search", "搜索与隐私", Section::Search, cx))
                    .child(self.nav("settings-nav-about", "关于", Section::About, cx)),
            )
            .child(
                v_flex()
                    .id("settings-body")
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .p_4()
                    .gap_3()
                    .overflow_y_scroll()
                    .when(self.section == Section::Editor, |el| {
                        let border = cx.theme().border;
                        let accent = cx.theme().accent;
                        el.child(editor_section(focus_source, preview, muted, border, accent, workspace))
                    })
                    .when(self.section == Section::Search, |el| el.child(search_section(search, muted)))
                    .when(self.section == Section::About, |el| el.child(about_section())),
            )
    }
}

fn editor_section(
    focus_source: bool,
    preview: LinkPreviewMode,
    muted: Hsla,
    border: Hsla,
    accent: Hsla,
    workspace: Entity<Workspace>,
) -> AnyElement {
    let preview_button = |id: &'static str, label: &'static str, mode: LinkPreviewMode| {
        let workspace = workspace.clone();
        let on = preview == mode;
        div()
            .id(id)
            .debug_selector(move || id.into())
            .px_2()
            .py(px(4.))
            .rounded_md()
            .text_sm()
            .cursor_pointer()
            .border_1()
            .border_color(if on { accent } else { border })
            .child(label)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                workspace.update(cx, |workspace, cx| workspace.set_link_preview(mode, window, cx));
            })
    };
    v_flex()
        .id("settings-editor")
        .debug_selector(|| "settings-editor".into())
        .w_full()
        .gap_4()
        .child(div().font_weight(FontWeight::MEDIUM).child("编辑器"))
        .child(
            div().debug_selector(|| "settings-focus-source".into()).child(
                Switch::new("settings-focus-source")
                    .small()
                    .label("聚焦时显示源码")
                    .checked(focus_source)
                    .on_click({
                        let workspace = workspace.clone();
                        move |checked: &bool, _, cx| {
                            let on = *checked;
                            workspace.update(cx, |workspace, cx| workspace.set_focus_shows_source(on, cx));
                        }
                    }),
            ),
        )
        .child(div().text_xs().text_color(muted).child("关掉时，点进段落仍隐藏 Markdown 标记。"))
        .child(div().font_weight(FontWeight::MEDIUM).child("链接预览"))
        .child(
            h_flex()
                .gap_2()
                .child(preview_button("settings-preview-hover", "悬停", LinkPreviewMode::Hover))
                .child(preview_button("settings-preview-command", "按住 ⌘", LinkPreviewMode::Command))
                .child(preview_button("settings-preview-off", "关闭", LinkPreviewMode::Off)),
        )
        .into_any_element()
}

fn search_section(search: Option<Entity<SearchSettings>>, muted: Hsla) -> AnyElement {
    let missing = search.is_none();
    v_flex()
        .id("settings-search")
        .debug_selector(|| "settings-search".into())
        .w_full()
        .gap_3()
        .child(div().font_weight(FontWeight::MEDIUM).child("搜索与隐私"))
        .children(search)
        .when(missing, |el| {
            el.child(div().text_sm().text_color(muted).child("请先打开一个知识库，再设置搜索与隐私。"))
        })
        .into_any_element()
}

fn about_section() -> impl IntoElement {
    v_flex()
        .id("settings-about")
        .debug_selector(|| "settings-about".into())
        .gap_2()
        .text_sm()
        .child(div().font_weight(FontWeight::MEDIUM).child("关于"))
        .child(format!("Aero {}", env!("CARGO_PKG_VERSION")))
        .child("本地 Markdown 笔记。菜单里的「检查更新」不会联网。")
}
