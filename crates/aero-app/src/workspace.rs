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
use gpui_kit::component::tab::TabBar;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, TitleBar, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::actions::QuickOpen as QuickOpen_;
use crate::actions::*;
use crate::branding;
use crate::editor::{Editor, EditorEvent, SaveState};
use crate::file_tree::{FileTree, FileTreeEvent, unique_note_name};
use crate::paths::{PersistedState, app_data_dir, display_path};
use crate::quick_open::{QuickOpen, QuickOpenEvent};
use crate::search::{Phase, SearchService};
use crate::search_view::{SearchView, SearchViewEvent};

const SIDEBAR_WIDTH: f32 = 260.;
const PANEL_WIDTH: f32 = 300.;

/// 一个已打开的 vault 及其视图。
struct VaultSession {
    vault: Arc<Vault>,
    tree: Entity<FileTree>,
    editor: Option<Entity<Editor>>,
    /// 全文索引（后台）；状态栏显示进度，搜索视图查询它
    search: Entity<SearchService>,
    search_view: Entity<SearchView>,
    /// 本次会话里打开过的笔记，最近的在前（快速打开的"最近优先"）
    recent: Vec<VaultPath>,
    _subscriptions: Vec<Subscription>,
}

/// 左侧栏当前视图（`ui_design.md` 2.1：文件 / 搜索 / 标签；标签视图 M2）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarTab {
    Files,
    Search,
}

pub struct Workspace {
    state: PersistedState,
    open: Option<VaultSession>,
    /// ⌘P 弹层
    quick_open: Option<(Entity<QuickOpen>, Subscription)>,
    picker_focus: Option<FocusHandle>,
    recent_commands: Vec<&'static str>,
    sidebar_tab: SidebarTab,
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
            quick_open: None,
            picker_focus: None,
            recent_commands: Vec::new(),
            sidebar_tab: SidebarTab::Files,
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
                // 先丢掉上一个 vault 的索引服务（它的同步线程看到 cancelled 会停）
                self.open = None;
                let search = cx.new(|cx| SearchService::new(vault.clone(), app_data_dir().join("index"), cx));
                let search_sub = cx.observe(&search, |_, _, cx| cx.notify());
                let search_view = cx.new(|cx| SearchView::new(search.clone(), window, cx));
                let view_sub =
                    cx.subscribe_in(&search_view, window, |this, _, event, window, cx| match event {
                        SearchViewEvent::Open { path, block_ord } => {
                            this.open_hit(path.clone(), *block_ord, window, cx)
                        }
                        SearchViewEvent::Dismiss => {
                            if let Some(editor) = this.open.as_ref().and_then(|o| o.editor.clone()) {
                                editor.update(cx, |e, cx| e.focus_browse(window, cx));
                            } else {
                                this.focus_handle.focus(window, cx);
                            }
                        }
                    });
                self.open = Some(VaultSession {
                    vault: vault.clone(),
                    tree,
                    editor: None,
                    search,
                    search_view,
                    recent: Vec::new(),
                    _subscriptions: vec![sub, search_sub, view_sub],
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
        // 切换前把上一篇落盘，并让索引跟上
        if let Some(prev) = open.editor.clone() {
            let result = prev.update(cx, |e, _| e.flush_sync());
            if result.is_ok() {
                self.index_editor(&prev, cx);
            }
            self.report_flush(result, window, cx);
        }
        let Some(open) = self.open.as_mut() else { return };
        match Editor::load(open.vault.clone(), path.clone()) {
            Ok((doc, stamp)) => {
                let vault = open.vault.clone();
                let editor = cx.new(|cx| Editor::new(vault, path.clone(), doc, stamp, cx));
                let sub = cx.subscribe_in(&editor, window, |this, editor, event, window, cx| match event {
                    EditorEvent::Changed => cx.notify(),
                    EditorEvent::Saved | EditorEvent::Reloaded => {
                        this.index_editor(editor, cx);
                        cx.notify();
                    }
                    EditorEvent::SaveFailed(msg) => {
                        window.push_notification(
                            Notification::error(format!("保存失败：{msg}")).title("Aero"),
                            cx,
                        );
                    }
                    EditorEvent::Conflict(copy) => this.notify_conflict(copy, window, cx),
                    EditorEvent::RenameRequested(name) => this.rename_current(name.clone(), window, cx),
                });
                open._subscriptions.truncate(3);
                open._subscriptions.push(sub);
                open.editor = Some(editor.clone());
                open.recent.retain(|p| p != &path);
                open.recent.insert(0, path.clone());
                open.recent.truncate(20);
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

    /// 搜索命中：打开笔记（已打开则复用）并定位到块。不抢焦点——↑↓ 预览时焦点留在搜索框。
    fn open_hit(&mut self, path: VaultPath, block_ord: u32, window: &mut Window, cx: &mut Context<Self>) {
        let already =
            self.open.as_ref().and_then(|o| o.editor.as_ref()).is_some_and(|e| e.read(cx).path() == &path);
        if !already {
            let focused = window.focused(cx);
            self.open_note(path, window, cx);
            if let Some(f) = focused {
                window.focus(&f, cx);
            }
        }
        if let Some(editor) = self.open.as_ref().and_then(|o| o.editor.clone()) {
            editor.update(cx, |e, cx| e.reveal_indexed_block(block_ord, cx));
        }
    }

    fn on_quick_open(&mut self, _: &QuickOpen_, window: &mut Window, cx: &mut Context<Self>) {
        if self.open.is_none() {
            return;
        }
        if self.quick_open.is_some() {
            self.close_quick_open(window, cx);
            return;
        }
        self.open_picker(false, window, cx);
    }

    fn on_command_palette(&mut self, _: &CommandPalette, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((picker, _)) = &self.quick_open {
            picker.update(cx, |picker, cx| picker.show_commands(window, cx));
        } else {
            self.open_picker(true, window, cx);
        }
    }

    fn available_commands(&self, window: &Window, cx: &App) -> Vec<Command> {
        let editor = self.open.as_ref().and_then(|o| o.editor.as_ref());
        let target =
            self.open.as_ref().is_some_and(|o| o.tree.read(cx).selected().is_some() || editor.is_some());
        let mut commands = commands();
        commands.retain(|command| match command.scope {
            CommandScope::Global => true,
            CommandScope::Vault => self.open.is_some(),
            CommandScope::Note => editor.is_some(),
            CommandScope::NoteTarget => target,
            CommandScope::Editor => editor.is_some_and(|e| {
                e.read(cx).command_available(command.action.as_ref())
                    && window.is_action_available(command.action.as_ref(), cx)
            }),
        });
        if let Some(focus) = window.focused(cx) {
            for command in &mut commands {
                if let Some(binding) =
                    window.highest_precedence_binding_for_action_in(command.action.as_ref(), &focus)
                {
                    command.shortcut = binding
                        .keystrokes()
                        .iter()
                        .map(|key| gpui_kit::component::kbd::Kbd::format(key.as_keystroke()))
                        .collect::<Vec<_>>()
                        .join(" ");
                }
            }
        }
        commands
    }

    fn open_picker(&mut self, command_mode: bool, window: &mut Window, cx: &mut Context<Self>) {
        let commands = self.available_commands(window, cx);
        self.picker_focus = window.focused(cx);
        if let Some(editor) = self.open.as_ref().and_then(|o| o.editor.clone()) {
            editor.update(cx, |e, cx| e.set_picker_open(true, window, cx));
        }
        let notes =
            self.open.as_ref().map(|open| (open.vault.clone(), open.search.clone(), open.recent.clone()));
        let picker = cx.new(|cx| {
            let mut picker = QuickOpen::new(commands, self.recent_commands.clone(), window, cx);
            if let Some((vault, service, recent)) = notes {
                picker.set_notes(vault, service, recent, cx);
            }
            if command_mode {
                picker.show_commands(window, cx);
            }
            picker
        });
        let sub = cx.subscribe_in(&picker, window, |this, _, event, window, cx| match event {
            QuickOpenEvent::Open(path) => {
                this.close_quick_open(window, cx);
                this.open_note(path.clone(), window, cx);
            }
            QuickOpenEvent::Create(name) => {
                this.close_quick_open(window, cx);
                this.create_note_named(name, window, cx);
            }
            QuickOpenEvent::Execute { id, action } => {
                this.close_quick_open(window, cx);
                if this.available_commands(window, cx).iter().any(|command| command.id == *id) {
                    this.recent_commands.retain(|recent| recent != id);
                    this.recent_commands.insert(0, id);
                    this.recent_commands.truncate(20);
                    window.dispatch_action(action.boxed_clone(), cx);
                }
            }
            QuickOpenEvent::Dismiss => this.close_quick_open(window, cx),
        });
        self.quick_open = Some((picker, sub));
        cx.notify();
    }

    fn close_quick_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.quick_open.take().is_some() {
            if let Some(focus) =
                self.picker_focus.take().filter(|focus| self.focus_handle.contains(focus, window))
            {
                window.focus(&focus, cx);
            } else if let Some(editor) = self.open.as_ref().and_then(|o| o.editor.clone()) {
                editor.update(cx, |e, cx| e.focus_browse(window, cx));
            } else {
                self.focus_handle.focus(window, cx);
            }
            if let Some(editor) = self.open.as_ref().and_then(|o| o.editor.clone()) {
                editor.update(cx, |e, cx| e.set_picker_open(false, window, cx));
            }
            cx.notify();
        }
    }

    /// 快速打开里"新建 X"：在 vault 根（3.5 的默认位置）新建并进入编辑。
    fn create_note_named(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let path = match note_path_in("", name) {
            Ok(p) => p,
            Err(msg) => {
                window.push_notification(Notification::error(msg).title("新建笔记"), cx);
                return;
            }
        };
        if !open.vault.exists(&path) {
            let doc = aero_core::Document::from_text(&format!("# {}\n\n", name.trim()));
            if let Err(err) = open.vault.write(&path, &doc) {
                window.push_notification(Notification::error(format!("无法新建笔记：{err}")), cx);
                return;
            }
            open.tree.update(cx, |t, cx| {
                t.rescan();
                cx.notify();
            });
        }
        self.open_note(path, window, cx);
        if let Some(editor) = self.open.as_ref().and_then(|o| o.editor.clone()) {
            editor.update(cx, |e, cx| e.start_writing(window, cx));
        }
    }

    fn on_open_search(&mut self, _: &OpenSearch, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        self.sidebar_tab = SidebarTab::Search;
        if !self.sidebar_visible {
            self.sidebar_visible = true;
            self.state.sidebar_visible = Some(true);
            self.state.save();
        }
        open.search_view.update(cx, |v, cx| v.focus(window, cx));
        cx.notify();
    }

    fn on_open_files(&mut self, _: &OpenFiles, _window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_tab = SidebarTab::Files;
        if !self.sidebar_visible {
            self.sidebar_visible = true;
            self.state.sidebar_visible = Some(true);
            self.state.save();
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

    /// ⌘⇧D：今日日记 `daily/YYYY-MM-DD.md`，不存在则按模板创建（3.4）。
    fn on_open_daily(&mut self, _: &OpenDaily, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let vault = open.vault.clone();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let path = VaultPath::new(&format!("daily/{today}.md")).expect("date is a valid file name");
        let created = if vault.exists(&path) {
            false
        } else {
            let doc = aero_core::Document::from_text(&format!("# {today}\n\n"));
            if let Err(err) = vault.write(&path, &doc) {
                window.push_notification(Notification::error(format!("无法创建今日日记：{err}")), cx);
                return;
            }
            open.tree.update(cx, |t, cx| {
                t.rescan();
                cx.notify();
            });
            true
        };
        self.open_note(path, window, cx);
        if created && let Some(editor) = self.open.as_ref().and_then(|o| o.editor.clone()) {
            editor.update(cx, |e, cx| e.start_writing(window, cx));
        }
    }

    fn on_save_now(&mut self, _: &SaveNow, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.open.as_ref().and_then(|o| o.editor.clone()) {
            editor.update(cx, |e, cx| e.save_now(cx));
        }
    }

    // ---- 删除 / 定位（4.1） ----

    /// ⌘⌫ / 文件菜单：树里选中的笔记，没有则是打开中的那篇。
    fn on_delete_note(&mut self, _: &DeleteNote, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let target = open
            .tree
            .read(cx)
            .selected()
            .cloned()
            .or_else(|| open.editor.as_ref().map(|e| e.read(cx).path().clone()));
        if let Some(path) = target {
            self.trash_note(&path, window, cx);
        }
    }

    fn on_trash_note(&mut self, action: &TrashNote, window: &mut Window, cx: &mut Context<Self>) {
        if let Ok(path) = VaultPath::new(&action.0) {
            self.trash_note(&path, window, cx);
        }
    }

    /// 移到系统废纸篓。打开中的笔记被删时先丢掉编辑器——它排队中的自动保存会把文件写回来。
    fn trash_note(&mut self, path: &VaultPath, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else { return };
        let was_open = open.editor.as_ref().is_some_and(|e| e.read(cx).path() == path);
        if was_open {
            open.editor = None;
            open._subscriptions.truncate(3);
        }
        let fs_path = open.vault.fs_path(path);
        match crate::trash::move_to_trash(&fs_path) {
            Ok(()) => {
                open.tree.update(cx, |t, cx| {
                    t.rescan();
                    if t.selected() == Some(path) {
                        t.select(None, cx);
                    }
                    cx.notify();
                });
                window.push_notification(
                    Notification::info(format!("已移到废纸篓：{}", path.as_str())).title("Aero"),
                    cx,
                );
                let p = path.clone();
                open.search.update(cx, |s, cx| s.note_removed(p, cx));
                if was_open {
                    self.focus_handle.focus(window, cx);
                }
            }
            Err(err) => {
                window.push_notification(
                    Notification::error(format!("无法移到废纸篓：{err}")).title("Aero"),
                    cx,
                );
                // 文件还在：重新打开
                if was_open {
                    self.open_note(path.clone(), window, cx);
                }
            }
        }
        cx.notify();
    }

    fn on_reveal_in_finder(&mut self, action: &RevealInFinder, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        if let Ok(path) = VaultPath::new(&action.0) {
            cx.reveal_path(&open.vault.fs_path(&path));
        }
    }

    fn on_copy_note_path(&mut self, action: &CopyNotePath, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        if let Ok(path) = VaultPath::new(&action.0) {
            let fs = open.vault.fs_path(&path).display().to_string();
            cx.write_to_clipboard(ClipboardItem::new_string(fs));
        }
    }

    /// 标题处改了名字：在同一目录下重命名文件（5.1）。链接同步更新是 M2 的事。
    fn rename_current(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let Some(editor) = open.editor.clone() else { return };
        let old = editor.read(cx).path().clone();
        let target = match note_path_in(old.parent(), &name) {
            Ok(p) => p,
            Err(msg) => {
                window.push_notification(Notification::error(msg).title("重命名"), cx);
                return;
            }
        };
        if open.vault.exists(&target) {
            window.push_notification(
                Notification::error(format!("已有同名笔记：{}", target.as_str())).title("重命名"),
                cx,
            );
            return;
        }
        match open.vault.rename(&old, &target) {
            Ok(()) => {
                editor.update(cx, |e, cx| e.set_path(target.clone(), cx));
                open.tree.update(cx, |t, cx| {
                    t.rescan();
                    t.select(Some(target.clone()), cx);
                });
                open.search.update(cx, |s, cx| s.note_renamed(old, target, cx));
                cx.notify();
            }
            Err(err) => window
                .push_notification(Notification::error(format!("无法重命名：{err}")).title("重命名"), cx),
        }
    }

    /// 退出前落盘。这里没有窗口可以提示，冲突副本只进日志。索引也同步 flush 一次。
    fn flush_all(&mut self, cx: &mut Context<Self>) {
        if let Some(editor) = self.open.as_ref().and_then(|o| o.editor.clone()) {
            match editor.update(cx, |e, _| e.flush_sync()) {
                Ok(Some(copy)) => log::warn!("conflict copy written on quit: {}", copy.as_str()),
                Ok(None) => {}
                Err(err) => log::error!("flush: {err}"),
            }
        }
        if let Some(open) = self.open.as_ref() {
            open.search.read(cx).flush_blocking();
        }
    }

    /// 编辑器刚落盘（或重载）：把它的内容送进索引。
    fn index_editor(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let (path, text, stamp) = editor.read(cx).index_snapshot();
        open.search.update(cx, |s, cx| s.note_saved(path, text, stamp, cx));
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
            Err(err) => {
                window.push_notification(Notification::error(format!("保存失败：{err}")).title("Aero"), cx)
            }
        }
    }

    /// 磁盘版本已另存为冲突副本：提示，并让文件树显示出这个新文件。
    fn notify_conflict(&mut self, copy: &VaultPath, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(open) = self.open.as_ref() {
            open.tree.update(cx, |t, cx| {
                t.rescan();
                cx.notify();
            });
            let copy = copy.clone();
            open.search.update(cx, |s, cx| s.note_changed(copy, cx));
        }
        window.push_notification(
            Notification::warning(format!(
                "这篇笔记在磁盘上被别的程序改过。磁盘版本已另存为 {}，当前内容照常保存。",
                copy.as_str()
            ))
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
                .child(img(branding::ICON_PATH).size(px(88.)).object_fit(ObjectFit::Contain))
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
        let tab = self.sidebar_tab;
        let body: AnyElement = match tab {
            SidebarTab::Files => open.tree.clone().into_any_element(),
            SidebarTab::Search => open.search_view.clone().into_any_element(),
        };
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
            .child(
                div().px_2().pb_2().child(
                    TabBar::new("sidebar-tabs")
                        .segmented()
                        .w_full()
                        .xsmall()
                        .selected_index(match tab {
                            SidebarTab::Files => 0,
                            SidebarTab::Search => 1,
                        })
                        .children(["文件", "搜索"])
                        .on_click(|ix, window, cx| {
                            let action: Box<dyn Action> =
                                if *ix == 0 { Box::new(OpenFiles) } else { Box::new(OpenSearch) };
                            window.dispatch_action(action, cx);
                        }),
                ),
            )
            .child(div().flex_1().min_h(px(0.)).child(body))
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
        let index_item: Option<AnyElement> = match open.search.read(cx).phase() {
            Phase::Opening => Some(div().child("索引：打开中…").into_any_element()),
            Phase::Syncing { done, total } => {
                Some(div().child(format!("索引中 {done}/{total}")).into_any_element())
            }
            Phase::Failed(msg) => Some(
                div()
                    .id("index-failed")
                    .text_color(theme.danger)
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(msg.clone()).build(window, cx)
                    })
                    .child("索引不可用")
                    .into_any_element(),
            ),
            Phase::Idle => None,
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
                h_flex().gap_3().items_center().children(index_item).children(right).child(
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
            .on_action(cx.listener(Self::on_open_daily))
            .on_action(cx.listener(Self::on_delete_note))
            .on_action(cx.listener(Self::on_trash_note))
            .on_action(cx.listener(Self::on_reveal_in_finder))
            .on_action(cx.listener(Self::on_copy_note_path))
            .on_action(cx.listener(Self::on_save_now))
            .on_action(cx.listener(Self::on_open_search))
            .on_action(cx.listener(Self::on_open_files))
            .on_action(cx.listener(Self::on_quick_open))
            .on_action(cx.listener(Self::on_command_palette))
            .on_action(cx.listener(Self::on_toggle_sidebar))
            .on_action(cx.listener(Self::on_toggle_panel))
            .size_full()
            .bg(background)
            .text_color(foreground)
            .child(TitleBar::new().child(div().text_sm().text_color(muted).child(title)))
            .child(body)
            .when_some(self.quick_open.as_ref().map(|(p, _)| p.clone()), |el, picker| {
                // 遮罩 + 居中弹层（3.2）；点遮罩关闭
                el.child(deferred(
                    div()
                        .id("quick-open-overlay")
                        .absolute()
                        .inset_0()
                        .flex()
                        .justify_center()
                        .items_start()
                        .pt(px(120.))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| this.close_quick_open(window, cx)),
                        )
                        .child(
                            div()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .child(picker),
                        ),
                ))
            })
    }
}

/// 最近修改的笔记，作为打开 vault 时的默认文档。
/// `dir/<name>.md`；名字不能为空、不能含路径分隔符或以 `.` 开头。
fn note_path_in(dir: &str, name: &str) -> Result<VaultPath, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("名字不能为空".into());
    }
    if name.contains(['/', '\\']) {
        return Err("名字里不能有 / 或 \\".into());
    }
    if name.starts_with('.') {
        return Err("名字不能以 . 开头".into());
    }
    let file = format!("{name}.md");
    let s = if dir.is_empty() { file } else { format!("{dir}/{file}") };
    VaultPath::new(&s).map_err(|e| e.to_string())
}

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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aero_core::{Vault, VaultPath};
    use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext};

    use super::{SidebarTab, VaultSession, Workspace};
    use crate::editor::Editor;
    use crate::file_tree::FileTree;
    use crate::paths::PersistedState;
    use crate::search::SearchService;
    use crate::search_view::SearchView;

    fn open(
        cx: &mut TestAppContext,
        source: Option<&str>,
    ) -> (tempfile::TempDir, tempfile::TempDir, Entity<Workspace>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::actions::init(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let vault = source.map(|source| {
            std::fs::write(dir.path().join("a.md"), source).unwrap();
            Arc::new(Vault::open(dir.path()).unwrap())
        });
        let (workspace, _) = cx.add_window_view(|window, cx| {
            let open = vault.map(|vault| {
                let path = VaultPath::new("a.md").unwrap();
                let (doc, stamp) = Editor::load(vault.clone(), path.clone()).unwrap();
                let editor = cx.new(|cx| Editor::new(vault.clone(), path, doc, stamp, cx));
                let tree = cx.new(|cx| FileTree::new(vault.clone(), cx));
                let search = cx.new(|cx| SearchService::new(vault.clone(), data.path().into(), cx));
                let search_view = cx.new(|cx| SearchView::new(search.clone(), window, cx));
                VaultSession {
                    vault,
                    tree,
                    editor: Some(editor),
                    search,
                    search_view,
                    recent: vec![],
                    _subscriptions: vec![],
                }
            });
            let focus_handle = cx.focus_handle();
            focus_handle.focus(window, cx);
            Workspace {
                state: PersistedState::default(),
                open,
                quick_open: None,
                picker_focus: None,
                recent_commands: vec![],
                sidebar_tab: SidebarTab::Files,
                sidebar_visible: true,
                panel_visible: true,
                resizable: cx.new(|_| Default::default()),
                focus_handle,
                error: None,
                _quit_subscription: cx.on_app_quit(|_, _| async {}),
                _activation_subscription: cx.observe_window_activation(window, |_, _, _| {}),
            }
        });
        (dir, data, workspace)
    }

    #[gpui_kit::test]
    fn command_palette_shortcut_works_without_vault(cx: &mut TestAppContext) {
        let (_dir, _data, workspace) = open(cx, None);
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            let commands = workspace.read(cx).available_commands(window, cx);
            assert_eq!(
                commands.iter().map(|command| command.id).collect::<Vec<_>>(),
                ["open-vault", "new-vault", "quit"]
            );
            assert!(commands[0].shortcut.contains('O'));
        });
        cx.simulate_keystrokes("cmd-shift-p");
        cx.run_until_parked();
        cx.update(|_, cx| assert!(workspace.read(cx).quick_open.is_some()));
        cx.simulate_input("new note");
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("picker-row-0").is_none(), "note commands need a vault");
        cx.simulate_keystrokes("enter");
        cx.update(|_, cx| assert!(workspace.read(cx).quick_open.is_some()));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(workspace.read(cx).quick_open.is_none());
            assert_eq!(window.focused(cx), Some(workspace.read(cx).focus_handle.clone()));
        });
    }

    #[gpui_kit::test]
    fn command_palette_returns_to_search_without_editor_commands(cx: &mut TestAppContext) {
        let (_dir, _data, workspace) = open(cx, Some("# Heading\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.run_until_parked();
        let (editor, heading) = cx.update(|window, cx| {
            let editor = workspace.read(cx).open.as_ref().unwrap().editor.clone().unwrap();
            let heading = editor.read(cx).headings()[0].2;
            editor.update(cx, |e, cx| e.focus_block(heading, window, cx));
            (editor, heading)
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.simulate_keystrokes("cmd-shift-p");
        cx.run_until_parked();
        cx.simulate_input("全文搜索");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let search_focus = cx.update(|window, cx| {
            window.simulate_next_frame(cx);
            window.draw(cx).clear(cx);
            let workspace = workspace.read(cx);
            assert_eq!(workspace.sidebar_tab, SidebarTab::Search);
            assert!(workspace.quick_open.is_none());
            assert!(
                workspace
                    .available_commands(window, cx)
                    .iter()
                    .all(|command| !matches!(command.scope, crate::actions::CommandScope::Editor))
            );
            assert!(
                !editor.read(cx).command_available(&crate::actions::ConvertBlock(2)),
                "leaving the editor through a palette command must exit source editing"
            );
            window.focused(cx).unwrap()
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_hit(VaultPath::new("a.md").unwrap(), 0, window, cx);
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert_eq!(editor.read(cx).highlighted_block(), Some(heading));
            assert_eq!(window.focused(cx), Some(search_focus.clone()));
            assert_eq!(editor.read(cx).index_snapshot().1, "# Heading\n");
        });
        assert!(cx.debug_bounds("unit-1").is_some(), "hit renders its preview");
        cx.simulate_keystrokes("cmd-shift-p");
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.update(|window, cx| assert_eq!(window.focused(cx), Some(search_focus)));
    }

    #[gpui_kit::test]
    fn command_palette_can_open_note_picker_without_losing_caret(cx: &mut TestAppContext) {
        let (_dir, _data, workspace) = open(cx, Some("# Heading\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.run_until_parked();
        let editor = cx.update(|window, cx| {
            let editor = workspace.read(cx).open.as_ref().unwrap().editor.clone().unwrap();
            editor.update(cx, |e, cx| e.focus_block(e.headings()[0].2, window, cx));
            editor
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-left");
        let original = cx.update(|window, cx| window.focused(cx).unwrap());
        cx.simulate_keystrokes("cmd-shift-p");
        cx.run_until_parked();
        cx.simulate_input("快速打开笔记");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
            assert!(workspace.read(cx).quick_open.is_some());
            assert!(editor.read(cx).command_available(&crate::actions::ConvertBlock(2)));
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
            assert!(workspace.read(cx).quick_open.is_none());
            assert_eq!(window.focused(cx), Some(original));
        });
        cx.simulate_input("X");
        cx.update(|_, cx| assert_eq!(editor.read(cx).index_snapshot().1, "X# Heading\n"));
    }

    #[gpui_kit::test]
    fn command_palette_restores_caret_and_executes_editor_action(cx: &mut TestAppContext) {
        let (_dir, _data, workspace) = open(cx, Some("# Heading\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.run_until_parked();
        let editor = cx.update(|window, cx| {
            let editor = workspace.read(cx).open.as_ref().unwrap().editor.clone().unwrap();
            editor.update(cx, |e, cx| e.focus_block(e.headings()[0].2, window, cx));
            editor
        });
        cx.run_until_parked();
        cx.simulate_input("!");
        cx.simulate_keystrokes("cmd-left");
        let original = cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            window.focused(cx).unwrap()
        });
        cx.simulate_keystrokes("cmd-shift-p");
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
            assert_eq!(window.focused(cx), Some(original));
        });
        cx.simulate_input("X");
        cx.update(|_, cx| assert_eq!(editor.read(cx).index_snapshot().1, "X# Heading!\n"));
        cx.simulate_keystrokes("cmd-shift-p");
        cx.run_until_parked();
        cx.simulate_input("heading 2");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
            assert!(workspace.read(cx).quick_open.is_none());
            assert_eq!(editor.read(cx).index_snapshot().1, "## X# Heading!\n");
            assert!(editor.read(cx).command_available(&crate::actions::ConvertBlock(1)));
        });
        cx.simulate_keystrokes("cmd-shift-p");
        cx.run_until_parked();
        cx.simulate_input("undo");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
            assert_eq!(editor.read(cx).index_snapshot().1, "X# Heading!\n");
            assert!(editor.read(cx).command_available(&crate::actions::ConvertBlock(2)));
            assert_eq!(workspace.read(cx).recent_commands, ["undo", "heading-2"]);
        });
        cx.simulate_keystrokes("cmd-p");
        cx.run_until_parked();
        cx.simulate_input("> redo");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
            assert_eq!(editor.read(cx).index_snapshot().1, "## X# Heading!\n");
            assert!(editor.read(cx).command_available(&crate::actions::ConvertBlock(1)));
            assert_eq!(workspace.read(cx).recent_commands, ["redo", "undo", "heading-2"]);
        });
        cx.simulate_keystrokes("cmd-shift-p");
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let first = cx.debug_bounds("picker-row-0").unwrap();
        cx.simulate_click(first.center(), gpui_kit::Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert!(workspace.read(cx).quick_open.is_none());
            assert_eq!(
                editor.read(cx).index_snapshot().1,
                "X# Heading!\n",
                "most recent available command is undo"
            );
            assert_eq!(workspace.read(cx).recent_commands, ["undo", "redo", "heading-2"]);
        });
    }
}
