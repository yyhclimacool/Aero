//! 主窗口：三栏 + 状态栏（`ui_design.md` 第 2 节）、首次运行页（第 11 节）、右侧大纲（4.x / 8）。
//!
//! Workspace 持有当前 vault、文件树与编辑器实体，并处理应用级动作（打开 / 新建 vault、新建笔记、
//! 折叠面板、保存）。编辑器与文件树之间不直接通信，都经由这里转发。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use aero_core::{BlockId, Vault, VaultPath};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, TitleBar, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::actions::*;
use crate::editor::{Editor, EditorEvent, SaveState};
use crate::file_tree::{FileTree, FileTreeEvent, unique_note_name};
use crate::paths::{PersistedState, display_path};

const SIDEBAR_WIDTH: f32 = 260.;
const PANEL_WIDTH: f32 = 300.;

/// 一个已打开的 vault 及其视图。
struct VaultSession {
    vault: Arc<Vault>,
    tree: Entity<FileTree>,
    editor: Option<Entity<Editor>>,
    _subscriptions: Vec<Subscription>,
}

pub struct Workspace {
    state: PersistedState,
    open: Option<VaultSession>,
    sidebar_visible: bool,
    panel_visible: bool,
    resizable: Entity<ResizableState>,
    focus_handle: FocusHandle,
    /// 首次运行页上的错误（打开失败等）
    error: Option<String>,
    _quit_subscription: Subscription,
    _activation_subscription: Subscription,
}

impl Workspace {
    pub fn new(initial_vault: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let state = PersistedState::load();
        let quit = cx.on_app_quit(|this, cx| {
            this.flush_all(cx);
            async {}
        });
        // 从别的程序切回来时立刻看一眼磁盘（方案 12）；平时靠编辑器自己的轮询
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active()
                && let Some(editor) = this.open.as_ref().and_then(|o| o.editor.clone())
            {
                editor.update(cx, |e, cx| e.check_external(cx));
            }
        });
        let mut this = Workspace {
            sidebar_visible: state.sidebar_visible.unwrap_or(true),
            panel_visible: state.panel_visible.unwrap_or(true),
            state,
            open: None,
            resizable: cx.new(|_| ResizableState::default()),
            focus_handle: cx.focus_handle(),
            error: None,
            _quit_subscription: quit,
            _activation_subscription: activation,
        };
        this.focus_handle.focus(window, cx);

        let target = initial_vault.or_else(|| this.state.recent_vaults.first().cloned());
        if let Some(root) = target {
            this.open_vault(&root, window, cx);
        }
        this
    }

    // ---- vault ----

    fn open_vault(&mut self, root: &Path, window: &mut Window, cx: &mut Context<Self>) {
        self.flush_all(cx);
        match Vault::open(root) {
            Ok(vault) => {
                let vault = Arc::new(vault);
                let tree = cx.new(|cx| FileTree::new(vault.clone(), cx));
                let sub = cx.subscribe_in(&tree, window, |this, _, event, window, cx| match event {
                    FileTreeEvent::Open(path) => this.open_note(path.clone(), window, cx),
                });
                self.open = Some(VaultSession {
                    vault: vault.clone(),
                    tree,
                    editor: None,
                    _subscriptions: vec![sub],
                });
                self.error = None;
                self.state.touch_vault(vault.root());
                self.state.save();
                // 默认打开最近修改的一篇；没有则停在空编辑区
                if let Some(path) = most_recent_note(&vault) {
                    self.open_note(path, window, cx);
                }
                self.focus_handle.focus(window, cx);
            }
            Err(err) => {
                self.error = Some(format!("无法打开 {}：{err}", display_path(root)));
                self.state.forget_vault(root);
                self.state.save();
            }
        }
        cx.notify();
    }

    fn on_open_vault(&mut self, _: &OpenVault, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("打开为 Vault".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(root) = paths.into_iter().next()
            {
                let _ = this.update_in(cx, |this, window, cx| this.open_vault(&root, window, cx));
            }
        })
        .detach();
    }

    fn on_new_vault(&mut self, _: &NewVault, window: &mut Window, cx: &mut Context<Self>) {
        let start = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&start, Some("Knowledge"));
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(root))) = rx.await {
                let _ = this.update_in(cx, |this, window, cx| match create_vault_skeleton(&root) {
                    Ok(()) => this.open_vault(&root, window, cx),
                    Err(err) => {
                        this.error = Some(format!("无法创建 {}：{err}", display_path(&root)));
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    fn on_close_vault(&mut self, _: &CloseVault, window: &mut Window, cx: &mut Context<Self>) {
        self.flush_all(cx);
        self.open = None;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn open_recent(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.open_vault(&root, window, cx);
    }

    // ---- 笔记 ----

    fn open_note(&mut self, path: VaultPath, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        if open.editor.as_ref().is_some_and(|e| e.read(cx).path() == &path) {
            return;
        }
        // 切换前把上一篇落盘
        if let Some(prev) = open.editor.clone() {
            let result = prev.update(cx, |e, _| e.flush_sync());
            self.report_flush(result, window, cx);
        }
        let Some(open) = self.open.as_mut() else { return };
        match Editor::load(open.vault.clone(), path.clone()) {
            Ok((doc, stamp)) => {
                let vault = open.vault.clone();
                let editor = cx.new(|cx| Editor::new(vault, path.clone(), doc, stamp, cx));
                let sub = cx.subscribe_in(&editor, window, |this, _, event, window, cx| match event {
                    EditorEvent::Changed | EditorEvent::Reloaded => cx.notify(),
                    EditorEvent::SaveFailed(msg) => {
                        window.push_notification(
                            Notification::error(format!("保存失败：{msg}")).title("Aero"),
                            cx,
                        );
                    }
                    EditorEvent::Conflict(copy) => this.notify_conflict(copy, window, cx),
                });
                open._subscriptions.truncate(1);
                open._subscriptions.push(sub);
                open.editor = Some(editor.clone());
                open.tree.update(cx, |t, cx| t.select(Some(path), cx));
                editor.update(cx, |e, cx| e.focus_browse(window, cx));
            }
            Err(err) => {
                window
                    .push_notification(Notification::error(format!("无法打开 {}：{err}", path.as_str())), cx);
            }
        }
        cx.notify();
    }

    fn on_new_note(&mut self, _: &NewNote, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let vault = open.vault.clone();
        let existing = vault.notes().unwrap_or_default();
        // 3.5：新建位置 = 当前选中的文件夹；否则 vault 根
        let dir = open.tree.read(cx).selected_dir().unwrap_or_default();
        let dir = if dir == "daily" { String::new() } else { dir };
        let path = unique_note_name(&existing, &dir, "未命名");
        let doc = aero_core::Document::from_text("");
        match vault.write(&path, &doc) {
            Ok(_) => {
                open.tree.update(cx, |t, cx| {
                    t.rescan();
                    cx.notify();
                });
                self.open_note(path, window, cx);
                if let Some(editor) = self.open.as_ref().and_then(|o| o.editor.clone()) {
                    editor.update(cx, |e, cx| e.start_writing(window, cx));
                }
            }
            Err(err) => window.push_notification(Notification::error(format!("无法新建笔记：{err}")), cx),
        }
    }

    fn on_save_now(&mut self, _: &SaveNow, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.open.as_ref().and_then(|o| o.editor.clone()) {
            editor.update(cx, |e, cx| e.save_now(cx));
        }
    }

    /// 退出前落盘。这里没有窗口可以提示，冲突副本只进日志。
    fn flush_all(&mut self, cx: &mut Context<Self>) {
        if let Some(editor) = self.open.as_ref().and_then(|o| o.editor.clone()) {
            match editor.update(cx, |e, _| e.flush_sync()) {
                Ok(Some(copy)) => log::warn!("conflict copy written on quit: {}", copy.as_str()),
                Ok(None) => {}
                Err(err) => log::error!("flush: {err}"),
            }
        }
    }

    fn report_flush(
        &mut self,
        result: Result<Option<VaultPath>, aero_core::VaultError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(Some(copy)) => self.notify_conflict(&copy, window, cx),
            Ok(None) => {}
            Err(err) => window.push_notification(Notification::error(format!("保存失败：{err}")).title("Aero"), cx),
        }
    }

    /// 磁盘版本已另存为冲突副本：提示，并让文件树显示出这个新文件。
    fn notify_conflict(&mut self, copy: &VaultPath, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(open) = self.open.as_ref() {
            open.tree.update(cx, |t, cx| {
                t.rescan();
                cx.notify();
            });
        }
        window.push_notification(
            Notification::warning(format!("这篇笔记在磁盘上被别的程序改过。磁盘版本已另存为 {}，当前内容照常保存。", copy.as_str()))
                .title("外部修改"),
            cx,
        );
    }

    fn on_toggle_sidebar(&mut self, _: &ToggleSidebar, _window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_visible = !self.sidebar_visible;
        self.state.sidebar_visible = Some(self.sidebar_visible);
        self.state.save();
        cx.notify();
    }

    fn on_toggle_panel(&mut self, _: &TogglePanel, _window: &mut Window, cx: &mut Context<Self>) {
        self.panel_visible = !self.panel_visible;
        self.state.panel_visible = Some(self.panel_visible);
        self.state.save();
        cx.notify();
    }

    // ---- 渲染 ----

    fn render_first_run(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let recents: Vec<AnyElement> = self
            .state
            .recent_vaults
            .iter()
            .enumerate()
            .map(|(ix, root)| {
                let root = root.clone();
                let name = root.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                let hover = theme.list_hover;
                h_flex()
                    .id(("recent", ix))
                    .w_full()
                    .px_3()
                    .py_2()
                    .gap_3()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(Icon::new(IconName::Folder).size(px(16.)).text_color(muted))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().child(name))
                            .child(div().text_xs().text_color(muted).truncate().child(display_path(&root))),
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.open_recent(root.clone(), window, cx)),
                    )
                    .into_any_element()
            })
            .collect();

        v_flex().size_full().items_center().justify_center().bg(theme.background).child(
            v_flex()
                .w(px(440.))
                .gap_4()
                .child(div().text_2xl().font_weight(FontWeight::SEMIBOLD).child("Aero"))
                .child(
                    div().text_sm().text_color(muted).child(
                        "一个 vault 就是一个普通文件夹，里面的 Markdown 文件就是你的笔记。选择一个开始。",
                    ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("open-vault")
                                .primary()
                                .icon(IconName::FolderOpen)
                                .label("打开文件夹…")
                                .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenVault), cx)),
                        )
                        .child(
                            Button::new("new-vault")
                                .outline()
                                .icon(IconName::FolderPlus)
                                .label("新建 Vault…")
                                .on_click(|_, window, cx| window.dispatch_action(Box::new(NewVault), cx)),
                        ),
                )
                .when_some(self.error.clone(), |el, err| {
                    el.child(div().text_sm().text_color(theme.danger).child(err))
                })
                .when(!recents.is_empty(), |el| {
                    el.child(div().mt_2().text_xs().text_color(muted).child("最近打开"))
                        .child(v_flex().children(recents))
                }),
        )
    }

    fn render_sidebar(&self, open: &VaultSession, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let name = open.vault.root().file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                h_flex()
                    .h(px(40.))
                    .px_3()
                    .items_center()
                    .justify_between()
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).truncate().child(name))
                    .child(
                        Button::new("new-note")
                            .ghost()
                            .xsmall()
                            .icon(IconName::FilePlus)
                            .tooltip("新建笔记 ⌘N")
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(NewNote), cx)),
                    ),
            )
            .child(div().flex_1().min_h(px(0.)).child(open.tree.clone()))
    }

    fn render_outline(&self, open: &VaultSession, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let hover = theme.list_hover;
        let headings: Vec<(u8, String, BlockId)> =
            open.editor.as_ref().map(|e| e.read(cx).headings()).unwrap_or_default();
        let editor = open.editor.clone();
        let rows: Vec<AnyElement> = headings
            .into_iter()
            .enumerate()
            .map(|(ix, (level, text, id))| {
                let editor = editor.clone();
                div()
                    .id(("heading", ix))
                    .w_full()
                    .pl(px(12. + (level.saturating_sub(1)) as f32 * 12.))
                    .pr_3()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .text_sm()
                    .when(level == 1, |s| s.font_weight(FontWeight::MEDIUM))
                    .when(level >= 3, |s| s.text_color(muted))
                    .truncate()
                    .child(if text.is_empty() { "（空标题）".to_owned() } else { text })
                    .on_click(move |_, window, cx| {
                        if let Some(editor) = &editor {
                            editor.update(cx, |e, cx| e.focus_block(id, window, cx));
                        }
                    })
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .border_l_1()
            .border_color(theme.sidebar_border)
            .child(
                h_flex()
                    .h(px(40.))
                    .px_3()
                    .items_center()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child("大纲"),
            )
            .child(
                v_flex()
                    .id("outline-rows")
                    .flex_1()
                    .min_h(px(0.))
                    .px_1()
                    .overflow_y_scroll()
                    .children(rows)
                    .when(empty, |el| {
                        el.child(div().px_3().py_2().text_xs().text_color(muted).child("文档里还没有标题。"))
                    }),
            )
    }

    fn render_editor_area(&self, open: &VaultSession, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        div().size_full().bg(theme.background).map(|el| match &open.editor {
            Some(editor) => el.child(editor.clone()),
            None => el.child(
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .text_sm()
                    .text_color(muted)
                    .child("从左侧选择一篇笔记，或按 ⌘N 新建。"),
            ),
        })
    }

    fn render_status_bar(&self, open: &VaultSession, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let (left, right): (String, Vec<AnyElement>) = match &open.editor {
            Some(editor) => {
                let e = editor.read(cx);
                let (label, color) = match e.save_state() {
                    SaveState::Clean => ("已保存".to_owned(), muted),
                    SaveState::Dirty => ("未保存".to_owned(), muted),
                    SaveState::Saving => ("保存中…".to_owned(), muted),
                    SaveState::Saved(at) => (format!("已保存 · {}", ago(*at)), muted),
                    SaveState::Failed(_) => ("保存失败".to_owned(), theme.danger),
                };
                let words = format!("{} 字", e.word_count());
                (
                    e.path().as_str().to_owned(),
                    vec![
                        div().text_color(color).child(label).into_any_element(),
                        div().child(words).into_any_element(),
                    ],
                )
            }
            None => (display_path(open.vault.root()), vec![]),
        };
        h_flex()
            .h(px(24.))
            .w_full()
            .px_3()
            .items_center()
            .justify_between()
            .text_xs()
            .text_color(muted)
            .bg(theme.sidebar)
            .border_t_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("toggle-sidebar")
                            .ghost()
                            .xsmall()
                            .icon(if self.sidebar_visible {
                                IconName::PanelLeftClose
                            } else {
                                IconName::PanelLeftOpen
                            })
                            .tooltip("左侧栏 ⌘\\")
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleSidebar), cx)),
                    )
                    .child(div().truncate().child(left)),
            )
            .child(
                h_flex().gap_3().items_center().children(right).child(
                    Button::new("toggle-panel")
                        .ghost()
                        .xsmall()
                        .icon(if self.panel_visible {
                            IconName::PanelRightClose
                        } else {
                            IconName::PanelRightOpen
                        })
                        .tooltip("右侧面板 ⌘⇧\\")
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(TogglePanel), cx)),
                ),
            )
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (background, foreground, muted) =
            (cx.theme().background, cx.theme().foreground, cx.theme().muted_foreground);
        let title = match &self.open {
            Some(open) => match &open.editor {
                Some(e) => format!("{} — Aero", e.read(cx).title()),
                None => format!("{} — Aero", display_path(open.vault.root())),
            },
            None => "Aero".to_owned(),
        };

        let body: AnyElement = match &self.open {
            None => self.render_first_run(cx).into_any_element(),
            Some(open) => v_flex()
                .flex_1()
                .min_h(px(0.))
                .child(
                    h_resizable("main")
                        .with_state(&self.resizable)
                        .child(
                            resizable_panel()
                                .size(px(SIDEBAR_WIDTH))
                                .size_range(px(200.)..px(480.))
                                .visible(self.sidebar_visible)
                                .child(self.render_sidebar(open, cx)),
                        )
                        .child(resizable_panel().child(self.render_editor_area(open, cx)))
                        .child(
                            resizable_panel()
                                .size(px(PANEL_WIDTH))
                                .size_range(px(220.)..px(480.))
                                .visible(self.panel_visible)
                                .child(self.render_outline(open, cx)),
                        ),
                )
                .child(self.render_status_bar(open, cx))
                .into_any_element(),
        };

        v_flex()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_open_vault))
            .on_action(cx.listener(Self::on_new_vault))
            .on_action(cx.listener(Self::on_close_vault))
            .on_action(cx.listener(Self::on_new_note))
            .on_action(cx.listener(Self::on_save_now))
            .on_action(cx.listener(Self::on_toggle_sidebar))
            .on_action(cx.listener(Self::on_toggle_panel))
            .size_full()
            .bg(background)
            .text_color(foreground)
            .child(TitleBar::new().child(div().text_sm().text_color(muted).child(title)))
            .child(body)
    }
}

/// 最近修改的笔记，作为打开 vault 时的默认文档。
fn most_recent_note(vault: &Vault) -> Option<VaultPath> {
    let notes = vault.notes().ok()?;
    notes
        .into_iter()
        .filter_map(|p| vault.stat(&p).ok().map(|(mtime, _)| (mtime, p)))
        .max_by_key(|(mtime, _)| *mtime)
        .map(|(_, p)| p)
}

/// 新 vault 的骨架：目录、`daily/`、一篇欢迎笔记。`.aero/vault.toml` 由 `Vault::open` 生成。
fn create_vault_skeleton(root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root.join("daily"))?;
    let welcome = root.join("欢迎.md");
    if !welcome.exists() {
        std::fs::write(
            welcome,
            "# 欢迎使用 Aero\n\n\
             这是一个普通文件夹里的一篇普通 Markdown 文件。你可以用任何编辑器打开它。\n\n\
             - 点击任意段落开始编辑，按 Esc 回到浏览\n\
             - 按 ⏎ 新建段落，⇧⏎ 换行\n\
             - ⌘N 新建笔记，⌘S 手动保存（其实会自动保存）\n\n\
             祝写作愉快。\n",
        )?;
    }
    Ok(())
}

fn ago(at: Instant) -> String {
    let secs = at.elapsed().as_secs();
    if secs < 5 {
        "刚刚".to_owned()
    } else if secs < 60 {
        format!("{secs} 秒前")
    } else {
        format!("{} 分钟前", secs / 60)
    }
}
