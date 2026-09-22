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
use crate::backlinks::{BacklinkOpened, BacklinksView};
use crate::branding;
use crate::editor::{Editor, EditorEvent, SaveState};
use crate::file_tree::{FileTree, FileTreeEvent, unique_note_name};
use crate::paths::{PersistedState, display_path};
use crate::quick_open::{QuickOpen, QuickOpenEvent};
use crate::search::{Phase, SearchService};
use crate::search_view::{SearchView, SearchViewEvent};
use crate::watch::{Batch, VaultWatcher, WatchEvent};

const SIDEBAR_WIDTH: f32 = 260.;
const PANEL_WIDTH: f32 = 300.;

/// 一个已打开的 vault 及其视图。
struct VaultSession {
    vault: Arc<Vault>,
    tree: Entity<FileTree>,
    editor: Option<Entity<Editor>>,
    /// 当前编辑器的事件订阅；换 / 关编辑器时一起丢
    editor_sub: Option<Subscription>,
    /// 全文索引（后台）；状态栏显示进度，搜索视图查询它
    search: Entity<SearchService>,
    search_view: Entity<SearchView>,
    backlinks: Entity<BacklinksView>,
    /// 重内容渲染服务（9.9）：图片等按需渲染，缓存落在应用数据目录
    images: Entity<crate::render::RenderService>,
    /// vault 目录的文件监听；起不来时编辑器退回轮询、索引只在启动时对齐
    _watcher: Entity<VaultWatcher>,
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContextTab {
    Backlinks,
    Outline,
}

pub struct Workspace {
    /// 应用数据目录：状态文件、索引缓存与默认示例库都在这里
    data_dir: PathBuf,
    state: PersistedState,
    open: Option<VaultSession>,
    /// ⌘P 弹层
    quick_open: Option<(Entity<QuickOpen>, Subscription)>,
    picker_focus: Option<FocusHandle>,
    recent_commands: Vec<&'static str>,
    sidebar_tab: SidebarTab,
    context_tab: ContextTab,
    rename_pending: bool,
    sidebar_visible: bool,
    panel_visible: bool,
    resizable: Entity<ResizableState>,
    focus_handle: FocusHandle,
    /// 首次运行页上的错误（打开失败等）
    error: Option<String>,
    _quit_subscription: Subscription,
    _activation_subscription: Subscription,
}

/// 预览里点 wikilink 后的动作：打开已存在的笔记，或提示同名候选，或新建（路径 + 初始内容），或失败原因。
enum LinkAction {
    Open { path: VaultPath, fragment: Option<String> },
    Ambiguous { target: String, candidates: Vec<VaultPath> },
    Created { path: VaultPath, body: String },
    Failed(String),
}

impl Workspace {
    pub fn new(
        initial_vault: Option<PathBuf>,
        data_dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = PersistedState::load(&data_dir);
        let workspace = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            workspace.update(cx, |this, cx| this.flush_before_leaving(window, cx)).unwrap_or(true)
        });
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
            data_dir,
            quick_open: None,
            picker_focus: None,
            recent_commands: Vec::new(),
            sidebar_tab: SidebarTab::Files,
            context_tab: ContextTab::Backlinks,
            rename_pending: false,
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

        // 显式路径优先；否则恢复最近打开且还在的库（11 节）。都没有就停在库选择页，示例库由用户自己选。
        match initial_vault {
            Some(root) => this.open_vault(&root, window, cx),
            None => {
                if let Some(root) = this.state.recent_vaults.iter().find(|p| p.is_dir()).cloned() {
                    this.open_recent(root, window, cx);
                }
            }
        }
        this
    }

    // ---- vault ----

    fn open_vault(&mut self, root: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if !self.flush_before_leaving(window, cx) {
            return;
        }
        match Vault::open(root) {
            Ok(vault) => {
                let vault = Arc::new(vault);
                let tree = cx.new(|cx| FileTree::new(vault.clone(), cx));
                let sub = self.subscribe_tree(&tree, window, cx);
                // 新 vault 打得开才离开旧的：落盘并同步关掉旧索引句柄（再打开同一个 vault 时锁才能拿到），
                // 然后丢掉上一个会话
                self.flush_all(cx);
                self.open = None;
                let index_dir = self.data_dir.join("index");
                let search = cx.new(|cx| SearchService::new(vault.clone(), index_dir, cx));
                let search_sub = cx.observe(&search, |_, _, cx| cx.notify());
                let search_view = cx.new(|cx| SearchView::new(search.clone(), window, cx));
                let backlinks = cx.new(|cx| BacklinksView::new(search.clone(), cx));
                let backlinks_sub = self.subscribe_backlinks(&backlinks, window, cx);
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
                // 渲染缓存按 vault 分目录，和索引一样可以随时删掉重建
                let cache_dir = self.data_dir.join("cache").join(vault.id());
                let images = cx.new(|_| crate::render::RenderService::new(vault.clone(), cache_dir));
                let images_sub = cx.observe(&images, |_, _, cx| cx.notify());
                let watcher = cx.new(|cx| VaultWatcher::new(vault.clone(), cx));
                if let Some(err) = watcher.read(cx).error.clone() {
                    // 监听起不来时编辑器退回 2 s 轮询、索引只在启动时对齐；告诉用户一声，别静默变慢
                    window.push_notification(
                        Notification::warning(format!("无法监听文件变化，改为定时检查：{err}")).title("Aero"),
                        cx,
                    );
                }
                let watch_sub = cx.subscribe_in(&watcher, window, |this, _, event, window, cx| match event {
                    WatchEvent::Changed(batch) => this.on_vault_changed(batch, window, cx),
                });
                self.open = Some(VaultSession {
                    vault: vault.clone(),
                    tree,
                    editor: None,
                    editor_sub: None,
                    search,
                    search_view,
                    backlinks,
                    images,
                    _watcher: watcher,
                    recent: Vec::new(),
                    _subscriptions: vec![sub, search_sub, view_sub, watch_sub, backlinks_sub, images_sub],
                });
                self.error = None;
                self.state.touch_vault(vault.root());
                self.state.save(&self.data_dir);
                // 默认打开最近修改的一篇；没有则停在空编辑区
                if let Some(path) = most_recent_note(&vault) {
                    self.open_note(path, window, cx);
                }
                self.focus_handle.focus(window, cx);
            }
            Err(err) => {
                self.error = Some(format!("无法打开 {}：{err}", display_path(root)));
                self.state.forget_vault(root);
                self.state.save(&self.data_dir);
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
        if !self.flush_before_leaving(window, cx) {
            return;
        }
        self.flush_all(cx);
        self.open = None;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn open_recent(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_demo_vault(&root) {
            self.open_demo_vault(window, cx);
        } else {
            self.open_vault(&root, window, cx);
        }
    }

    /// 库选择页的示例库入口：先把内嵌示例交付到应用数据目录（已存在则更新未被改动的那些），
    /// 再打开并停在欢迎页。用户每次从这里进来都会看到当前版本的示例。
    fn open_demo_vault(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match crate::demo_vault::prepare(&self.data_dir) {
            Ok(root) => {
                self.open_vault(&root, window, cx);
                let welcome = VaultPath::new(crate::demo_vault::WELCOME).expect("bundled welcome path");
                if self.open.as_ref().is_some_and(|open| open.vault.exists(&welcome)) {
                    self.open_note(welcome, window, cx);
                }
            }
            Err(error) => {
                self.error = Some(format!("无法准备示例库：{error}"));
                cx.notify();
            }
        }
    }

    /// 最近列表里的示例库也走示例库入口，否则更新过的示例会被旧副本挡住。
    /// 最近列表存的是规范化过的路径，所以两边都按实际目录比。
    fn is_demo_vault(&self, root: &Path) -> bool {
        let demo = crate::demo_vault::root(&self.data_dir);
        root == demo
            || std::fs::canonicalize(root)
                .ok()
                .zip(std::fs::canonicalize(&demo).ok())
                .is_some_and(|(a, b)| a == b)
    }

    // ---- 笔记 ----

    fn open_note(&mut self, path: VaultPath, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        if open.editor.as_ref().is_some_and(|e| e.read(cx).path() == &path) {
            return;
        }
        // 保存失败时保留当前编辑器，不能只弹通知后丢掉未保存内容。
        if !self.flush_before_leaving(window, cx) {
            return;
        }
        let Some(open) = self.open.as_mut() else { return };
        match Editor::load(open.vault.clone(), path.clone()) {
            Ok((doc, stamp)) => {
                let vault = open.vault.clone();
                let editor = cx.new(|cx| Editor::new(vault, path.clone(), doc, stamp, cx));
                editor.update(cx, |e, cx| {
                    e.connect_link_preview(open.search.clone(), cx);
                    e.connect_images(open.images.clone(), cx);
                });
                let sub = self.subscribe_editor(&editor, window, cx);
                let Some(open) = self.open.as_mut() else { return };
                open.editor_sub = Some(sub);
                open.editor = Some(editor.clone());
                open.recent.retain(|p| p != &path);
                open.recent.insert(0, path.clone());
                open.recent.truncate(20);
                open.backlinks.update(cx, |v, cx| v.set_note(Some(path.clone()), cx));
                editor.update(cx, |e, cx| e.connect_backlinks(open.backlinks.clone(), cx));
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

    /// 反链点击 → 打开来源；切换被保存保护否决时不改动当前编辑器。
    fn subscribe_backlinks(
        &mut self,
        view: &Entity<BacklinksView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(view, window, |this, _, event: &BacklinkOpened, window, cx| {
            let hit = &event.0;
            this.open_note(hit.path.clone(), window, cx);
            if let Some(editor) = this.open.as_ref().and_then(|o| o.editor.clone())
                && editor.read(cx).path() == &hit.path
                && !editor.update(cx, |e, cx| e.reveal_backlink(hit, cx))
            {
                window.push_notification(
                    Notification::info("来源笔记已变化，已打开笔记；反链将在索引更新后刷新。"),
                    cx,
                );
            }
        })
    }

    /// 编辑器事件 → 工作区动作（索引、文件树、横幅上的请求）。
    fn subscribe_tree(
        &mut self,
        tree: &Entity<FileTree>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(tree, window, |this, _, event, window, cx| match event {
            FileTreeEvent::Open(path) => {
                this.open_note(path.clone(), window, cx);
                if let Some(open) = &this.open {
                    open.tree.update(cx, |t, cx| t.focus(window, cx));
                }
            }
            FileTreeEvent::Move { from, to } => this.move_entry(from.clone(), to.clone(), window, cx),
        })
    }

    fn subscribe_editor(
        &mut self,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(editor, window, |this, editor, event, window, cx| match event {
            EditorEvent::Changed => cx.notify(),
            EditorEvent::BacklinksRequested => {
                this.panel_visible = true;
                this.context_tab = ContextTab::Backlinks;
                cx.notify();
            }
            EditorEvent::Saved | EditorEvent::Reloaded => {
                this.index_editor(editor, cx);
                cx.notify();
            }
            EditorEvent::SaveFailed(msg) => {
                window.push_notification(Notification::error(format!("保存失败：{msg}")).title("Aero"), cx);
            }
            EditorEvent::PathsTouched(paths) => this.on_editor_touched_paths(editor, paths.clone(), cx),
            EditorEvent::Deleted => this.on_editor_deleted(editor, cx),
            EditorEvent::RenameRequested(name) => this.rename_current(name.clone(), window, cx),
            EditorEvent::RetitleRequested(title) => this.retitle_current(title.clone(), window, cx),
            EditorEvent::OpenRequested(path) => this.open_note(path.clone(), window, cx),
            EditorEvent::LinkClicked { target, fragment } => {
                this.on_link_clicked(target.clone(), fragment.clone(), window, cx)
            }
            EditorEvent::CloseRequested => this.close_editor(window, cx),
            EditorEvent::LinkCompletionRequested(generation) => {
                let Some(open) = this.open.as_ref() else { return };
                let notes = open.search.read(cx).completion_notes(open.vault.clone(), cx);
                let recent = open.recent.clone();
                editor.update(cx, |e, cx| e.load_wiki_notes(*generation, notes, recent, cx));
            }
            EditorEvent::TagCompletionRequested(generation) => {
                let Some(open) = this.open.as_ref() else { return };
                let e = editor.read(cx);
                let task = open.search.read(cx).completion_tags(
                    open.vault.clone(),
                    e.path().clone(),
                    e.tag_completion_source(),
                    cx,
                );
                editor.update(cx, |e, cx| e.load_tags(*generation, task, cx));
            }
        })
    }

    /// 预览里点 wikilink：后台解析（已索引笔记 + 磁盘文件，见 `SearchService::link_resolver`）→ 打开目标；
    /// 带标题片段则定位到对应标题。同名多篇是歧义：提示候选、不猜也不创建。
    /// 未解析的目标在链接所在目录创建 `<target>.md` 并打开，toast 提供撤销（见 [`Self::undo_created_note`]）。
    fn on_link_clicked(
        &mut self,
        target: String,
        fragment: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.open.as_ref() else { return };
        let editor = open.editor.clone();
        if target.trim().is_empty() {
            // `[[#标题]]` / `[[#^id]]`：同篇锚点
            if let (Some(fragment), Some(editor)) = (fragment, editor) {
                if let Err(error) = editor.update(cx, |e, cx| e.reveal_fragment(&fragment, cx)) {
                    window.push_notification(Notification::info(error).title("Aero"), cx);
                }
            }
            return;
        }
        // 相对路径按链接所在笔记的目录解析；撤销时回到来源笔记
        let source_dir = editor.as_ref().map(|e| e.read(cx).path().parent().to_owned()).unwrap_or_default();
        let source_path = editor.as_ref().map(|e| e.read(cx).path().clone());
        let vault = open.vault.clone();
        let resolver = open.search.read(cx).link_resolver(vault.clone(), cx);
        cx.spawn_in(window, async move |this, cx| {
            let action = cx
                .background_spawn(async move {
                    match resolver.await.resolve(&target, fragment.as_deref(), &source_dir) {
                        aero_core::links::Resolution::Resolved { path, fragment } => {
                            LinkAction::Open { path, fragment }
                        }
                        aero_core::links::Resolution::Ambiguous { candidates } => {
                            LinkAction::Ambiguous { target, candidates }
                        }
                        aero_core::links::Resolution::Unresolved => {
                            if fragment.as_deref().is_some_and(|f| f.starts_with('^')) {
                                return LinkAction::Failed(format!("找不到块引用的目标笔记：{target}"));
                            }
                            let Some(path) = aero_core::links::creation_path(&source_dir, &target) else {
                                return LinkAction::Failed(format!("无效的链接目标：{target}"));
                            };
                            if vault.exists(&path) {
                                // 解析后、创建前被并发新建：直接打开
                                return LinkAction::Open { path, fragment: None };
                            }
                            let body = format!("# {}\n\n", path.stem());
                            match vault.create_bytes(&path, body.as_bytes()) {
                                Ok(_) => LinkAction::Created { path, body },
                                Err(err) => LinkAction::Failed(format!("无法新建笔记：{err}")),
                            }
                        }
                    }
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| match action {
                LinkAction::Open { path, fragment } => {
                    this.open_note(path.clone(), window, cx);
                    // 打开可能被否决（上一篇保存失败）或失败：只在真的切到目标后才定位标题
                    let editor = this.open.as_ref().and_then(|o| o.editor.clone());
                    if let (Some(fragment), Some(editor)) = (fragment, editor)
                        && editor.read(cx).path() == &path
                        && let Err(error) = editor.update(cx, |e, cx| e.reveal_fragment(&fragment, cx))
                    {
                        window.push_notification(
                            Notification::info(format!("{}：{error}", path.as_str())).title("Aero"),
                            cx,
                        );
                    }
                }
                LinkAction::Ambiguous { target, candidates } => {
                    let list = candidates.iter().map(|p| p.as_str()).collect::<Vec<_>>().join("、");
                    window.push_notification(
                        Notification::warning(format!(
                            "「{target}」有 {} 篇同名笔记：{list}。请在链接里写明路径。",
                            candidates.len()
                        ))
                        .title("Aero"),
                        cx,
                    );
                }
                LinkAction::Created { path, body } => {
                    this.open_created_note(path, source_path, body, window, cx)
                }
                LinkAction::Failed(msg) => {
                    window.push_notification(Notification::error(msg).title("Aero"), cx)
                }
            });
        })
        .detach();
    }

    /// 打开刚创建（或并发已存在）的笔记：刷新树、进入编辑态，toast 提供撤销。
    fn open_created_note(
        &mut self,
        path: VaultPath,
        source: Option<VaultPath>,
        body: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(open) = self.open.as_ref() {
            open.tree.update(cx, |t, cx| {
                t.rescan();
                cx.notify();
            });
        }
        self.open_note(path.clone(), window, cx);
        if let Some(editor) = self.open.as_ref().and_then(|o| o.editor.clone()) {
            editor.update(cx, |e, cx| e.start_writing(window, cx));
        }
        let this = cx.weak_entity();
        window.push_notification(
            Notification::info(format!("已创建：{}", path.as_str())).title("Aero").action(move |_, _, _| {
                let this = this.clone();
                let path = path.clone();
                let source = source.clone();
                let body = body.clone();
                Button::new("wikilink-undo").label("撤销").on_click(move |_, window, cx| {
                    let Some(this) = this.upgrade() else { return };
                    this.update(cx, |w, cx| w.undo_created_note(&path, &body, source.as_ref(), window, cx));
                })
            }),
            cx,
        );
    }

    /// 撤销刚创建的笔记：内容未被编辑才移入废纸篓并回到来源笔记；已编辑则拒绝。
    fn undo_created_note(
        &mut self,
        path: &VaultPath,
        original: &str,
        source: Option<&VaultPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.open.as_mut() else { return };
        let showing = open.editor.as_ref().is_some_and(|e| e.read(cx).path() == path);
        let untouched = match open.editor.as_ref().filter(|_| showing) {
            Some(editor) => {
                // 文本没变还不够：有未落盘 / 正在落盘的改动时，移走文件后保存会把它写回来
                let e = editor.read(cx);
                e.index_snapshot().1 == original
                    && !matches!(e.save_state(), SaveState::Dirty | SaveState::Saving)
            }
            None => open.vault.read(path).is_ok_and(|(doc, _)| doc.to_string() == original),
        };
        if !untouched {
            window.push_notification(Notification::warning("笔记已被编辑，撤销已取消。").title("Aero"), cx);
            return;
        }
        // 先移文件、成功了再收编辑器：废纸篓失败时文件还在，编辑器不能先没了
        let fs_path = open.vault.fs_path(path);
        if let Err(err) = crate::trash::move_to_trash(&fs_path) {
            window.push_notification(Notification::error(format!("无法撤销：{err}")).title("Aero"), cx);
            return;
        }
        if showing {
            open.editor = None;
            open.editor_sub = None;
            open.backlinks.update(cx, |v, cx| v.set_note(None, cx));
        }
        open.tree.update(cx, |t, cx| {
            t.rescan();
            if t.selected() == Some(path) {
                t.select(None, cx);
            }
            cx.notify();
        });
        let p = path.clone();
        open.search.update(cx, |s, cx| s.note_removed(p, cx));
        window.push_notification(
            Notification::info(format!("已撤销：{} 移入废纸篓", path.as_str())).title("Aero"),
            cx,
        );
        match source {
            Some(source) => self.open_note(source.clone(), window, cx),
            None => self.focus_handle.focus(window, cx),
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
            self.state.save(&self.data_dir);
        }
        open.search_view.update(cx, |v, cx| v.focus(window, cx));
        cx.notify();
    }

    fn on_open_files(&mut self, _: &OpenFiles, _window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_tab = SidebarTab::Files;
        if !self.sidebar_visible {
            self.sidebar_visible = true;
            self.state.sidebar_visible = Some(true);
            self.state.save(&self.data_dir);
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
        if open.tree.read(cx).selected_folder().is_some() {
            return;
        }
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
            open.editor_sub = None;
            open.backlinks.update(cx, |v, cx| v.set_note(None, cx));
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

    fn move_entry(&mut self, from: VaultPath, to: VaultPath, window: &mut Window, cx: &mut Context<Self>) {
        if from != to {
            self.change_name(from, to, None, window, cx);
        }
    }

    fn retitle_current(&mut self, title: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.open.as_ref().and_then(|o| o.editor.as_ref()) else { return };
        let path = editor.read(cx).path().clone();
        self.change_name(path.clone(), path, Some(title), window, cx);
    }

    fn rename_current(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.open.as_ref().and_then(|o| o.editor.as_ref()) else { return };
        let old = editor.read(cx).path().clone();
        match note_path_in(old.parent(), &name) {
            Ok(target) => self.move_entry(old, target, window, cx),
            Err(msg) => window.push_notification(Notification::error(msg).title("重命名"), cx),
        }
    }

    fn change_name(
        &mut self,
        old: VaultPath,
        target: VaultPath,
        title: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.flush_before_leaving(window, cx) {
            return;
        }
        let Some(open) = self.open.as_ref() else { return };
        let editor = open.editor.clone();
        let snapshot = editor.as_ref().map(|e| e.read(cx).index_snapshot());
        let vault = open.vault.clone();
        let vault_id = vault.id().to_owned();
        let directory = vault.fs_path(&old).is_dir();
        let description =
            title.as_ref().map_or_else(|| format!("{old} → {target}"), |title| format!("标题 → {title}"));
        self.rename_pending = true;
        let (from, to) = (old.clone(), target.clone());
        let prepare = cx.background_spawn(async move {
            match title {
                Some(title) => aero_core::rename::RenamePlan::prepare_title(&vault, from, title),
                None => aero_core::rename::RenamePlan::prepare(&vault, from, to),
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let plan = match prepare.await {
                Ok(plan) => plan,
                Err(error) => {
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.rename_pending = false;
                        window.push_notification(
                            Notification::error(format!("无法重命名：{error}")).title("重命名"),
                            cx,
                        );
                        cx.notify();
                    });
                    return;
                }
            };
            // 产品交互约定：超过 20 个其他文件的批量修改先确认实际影响数量。
            if plan.other_files_changed() > 20 {
                let Ok(answer) = this.update_in(cx, |_, window, cx| {
                    window.prompt(
                        PromptLevel::Warning,
                        "重命名并更新链接？",
                        Some(&format!(
                            "将 {} 重命名为 {}，并更新 {} 个其他文件中的链接。",
                            old,
                            target,
                            plan.other_files_changed()
                        )),
                        &["取消", "重命名并更新"],
                        cx,
                    )
                }) else {
                    return;
                };
                if answer.await.ok() != Some(1) {
                    let _ = this.update(cx, |this, cx| {
                        this.rename_pending = false;
                        cx.notify();
                    });
                    return;
                }
            }
            let _ = this.update_in(cx, |this, window, cx| {
                this.rename_pending = false;
                let Some(open) = this.open.as_mut() else { return };
                if open.vault.id() != vault_id
                    || open.editor != editor
                    || editor.as_ref().zip(snapshot.as_ref()).is_some_and(|(editor, (path, _, stamp))| {
                        let editor = editor.read(cx);
                        editor.path() != path
                            || editor.index_snapshot().2.hash != stamp.hash
                            || !matches!(editor.save_state(), SaveState::Clean | SaveState::Saved(_))
                    })
                {
                    window.push_notification(
                        Notification::warning("笔记在准备期间已变化，重命名已取消，请保存后重试。"),
                        cx,
                    );
                    return;
                }
                // 提交与编辑器换路径在同一次 UI 回调内完成，文件监听/自动保存不会在两者之间插入。
                match plan.execute() {
                    Ok(report) => {
                        let message = format!("已完成 {description}，更新 {} 处链接。", report.links_changed);
                        open.search.update(cx, |s, cx| s.notes_renamed(report, cx));
                        if let Some(editor) = &editor {
                            editor.update(cx, |e, cx| {
                                let mapped = moved_path(e.path(), &old, &target, directory);
                                if e.path() != &mapped {
                                    e.set_path(mapped, cx);
                                }
                                e.check_external(cx);
                            });
                        }
                        for recent in &mut open.recent {
                            *recent = moved_path(recent, &old, &target, directory);
                        }
                        window.push_notification(Notification::info(message).title("重命名"), cx);
                    }
                    Err(error) => {
                        // 回滚被外部改动阻止时仍跟随存活的文件，错误中带有恢复副本路径。
                        if let Some(editor) = &editor {
                            editor.update(cx, |e, cx| {
                                let mapped = moved_path(e.path(), &old, &target, directory);
                                if !open.vault.exists(e.path()) && open.vault.exists(&mapped) {
                                    e.set_path(mapped, cx);
                                }
                                e.check_external(cx);
                            });
                        }
                        open.search.update(cx, |s, cx| s.resync(cx));
                        window.push_notification(Notification::error(error.to_string()).title("重命名"), cx);
                    }
                }
                let current = editor.as_ref().map(|e| e.read(cx).path().clone());
                open.backlinks.update(cx, |v, cx| v.set_note(current.clone(), cx));
                if let Some(editor) = &editor {
                    editor.update(cx, |e, cx| e.connect_backlinks(open.backlinks.clone(), cx));
                }
                open.tree.update(cx, |t, cx| {
                    t.rescan();
                    t.select(current, cx);
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// 离开当前 vault（退出 / 关闭 / 切换）前落盘。这里没有窗口可以提示，冲突副本只进日志。
    /// 索引同步 flush 并**关掉**：引擎句柄不能留给后台任务在退出途中释放（见 `SearchService::close_blocking`）。
    fn flush_all(&mut self, cx: &mut Context<Self>) {
        if let Some(editor) = self.open.as_ref().and_then(|o| o.editor.clone()) {
            match editor.update(cx, |e, _| e.flush_sync()) {
                Ok(Some(copy)) => log::warn!("conflict copy written on quit: {}", copy.as_str()),
                Ok(None) => {}
                Err(err) => log::error!("flush: {err}"),
            }
        }
        if let Some(open) = self.open.as_ref() {
            open.search.read(cx).close_blocking();
        }
    }

    fn flush_before_leaving(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.rename_pending {
            window.push_notification(Notification::warning("正在准备重命名，请稍后再试。").title("Aero"), cx);
            return false;
        }
        let Some(editor) = self.open.as_ref().and_then(|o| o.editor.clone()) else { return true };
        if editor.read(cx).save_state() == &SaveState::Saving {
            window.push_notification(Notification::warning("正在保存，请稍后再试。").title("Aero"), cx);
            return false;
        }
        let result = editor.update(cx, |e, _| e.flush_sync());
        let saved = result.is_ok();
        if saved {
            self.index_editor(&editor, cx);
        }
        self.report_flush(result, window, cx);
        saved
    }

    fn on_quit(&mut self, _: &Quit, window: &mut Window, cx: &mut Context<Self>) {
        if self.flush_before_leaving(window, cx) {
            cx.quit();
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

    /// 切换笔记时上一篇落盘留下了冲突副本：那个编辑器已经不在了，横幅没处显示，用通知代替。
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

    /// 编辑器在自己路径之外动了文件（冲突副本、另存为）：文件树重扫并跟住编辑器当前路径，索引对齐。
    fn on_editor_touched_paths(
        &mut self,
        editor: &Entity<Editor>,
        paths: Vec<VaultPath>,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.open.as_mut() else { return };
        let current = editor.read(cx).path().clone();
        open.tree.update(cx, |t, cx| {
            t.rescan();
            if t.selected() != Some(&current) {
                t.select(Some(current.clone()), cx);
            }
            cx.notify();
        });
        open.recent.retain(|p| p != &current);
        open.recent.insert(0, current);
        open.search.update(cx, |s, cx| s.notes_touched(paths, cx));
        cx.notify();
    }

    /// 打开中的笔记在外部被删：编辑器已显示横幅并停止保存；这里让文件树与索引跟上。
    fn on_editor_deleted(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let path = editor.read(cx).path().clone();
        open.tree.update(cx, |t, cx| {
            t.rescan();
            if t.selected() == Some(&path) {
                t.select(None, cx);
            }
            cx.notify();
        });
        open.search.update(cx, |s, cx| s.note_removed(path, cx));
        cx.notify();
    }

    /// 横幅"关闭"：丢掉编辑器，回到空编辑区。不落盘（文件已删除时正是为了不把它变回来）。
    fn close_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else { return };
        open.editor = None;
        open.editor_sub = None;
        open.backlinks.update(cx, |v, cx| v.set_note(None, cx));
        open.tree.update(cx, |t, cx| t.select(None, cx));
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    /// vault 目录有动静（文件监听）：文件树重扫；打开中的那篇立刻检查外部修改；其余按指纹对齐索引。
    fn on_vault_changed(&mut self, batch: &Batch, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let editor = open.editor.clone();
        let current = editor.as_ref().map(|e| e.read(cx).path().clone());
        open.tree.update(cx, |t, cx| {
            t.rescan();
            cx.notify();
        });
        let mine = |p: &VaultPath| current.as_ref() == Some(p);
        if let Some(editor) = &editor
            && (batch.changed.iter().any(mine) || batch.removed.iter().any(mine))
        {
            // 编辑器自己处理：重载 / 冲突副本 / 已删除，并通过事件把索引带上
            editor.update(cx, |e, cx| e.check_external(cx));
        }
        // 附件变化只影响渲染缓存：外部换了图，打开中的笔记下一帧就重新渲染
        open.images.update(cx, |s, cx| s.invalidate(&batch.attachments, cx));
        // 被嵌入的笔记变了：丢掉嵌入缓存重读，嵌入块跟着更新
        if let Some(editor) = &editor {
            let changed: Vec<VaultPath> = batch.changed.iter().chain(&batch.removed).cloned().collect();
            editor.update(cx, |e, cx| e.invalidate_embeds(&changed, cx));
        }
        let touched: Vec<VaultPath> =
            batch.changed.iter().chain(&batch.removed).filter(|p| !mine(p)).cloned().collect();
        open.search.update(cx, |s, cx| {
            s.notes_touched(touched, cx);
            if batch.rescan {
                s.resync(cx);
            }
        });
        cx.notify();
    }

    fn on_toggle_sidebar(&mut self, _: &ToggleSidebar, _window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_visible = !self.sidebar_visible;
        self.state.sidebar_visible = Some(self.sidebar_visible);
        self.state.save(&self.data_dir);
        cx.notify();
    }

    fn on_toggle_panel(&mut self, _: &TogglePanel, _window: &mut Window, cx: &mut Context<Self>) {
        self.panel_visible = !self.panel_visible;
        self.state.panel_visible = Some(self.panel_visible);
        self.state.save(&self.data_dir);
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
                        "一个 vault 就是一个普通文件夹，里面的 Markdown 文件就是你的笔记。选择一个开始，或先看看示例。",
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
                        )
                        .child(
                            div().debug_selector(|| "first-run-demo-vault".into()).child(
                                Button::new("demo-vault")
                                    .outline()
                                    .icon(IconName::Lightbulb)
                                    .label("查看示例库")
                                    .tooltip("六篇内置示例笔记，随程序更新")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_demo_vault(window, cx)
                                    })),
                            ),
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

    fn render_context(&self, open: &VaultSession, cx: &mut Context<Self>) -> impl IntoElement {
        let (background, border) = (cx.theme().sidebar, cx.theme().sidebar_border);
        let body = match self.context_tab {
            ContextTab::Backlinks => open.backlinks.clone().into_any_element(),
            ContextTab::Outline => self.render_outline(open, cx).into_any_element(),
        };
        v_flex()
            .size_full()
            .bg(background)
            .border_l_1()
            .border_color(border)
            .child(
                div().px_2().py_2().child(
                    TabBar::new("context-tabs")
                        .segmented()
                        .w_full()
                        .xsmall()
                        .selected_index(if self.context_tab == ContextTab::Backlinks { 0 } else { 1 })
                        .children(["反链", "大纲"])
                        .on_click(cx.listener(|this, ix, _, cx| {
                            this.context_tab =
                                if *ix == 0 { ContextTab::Backlinks } else { ContextTab::Outline };
                            cx.notify();
                        })),
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
        v_flex().size_full().child(
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                                .child(self.render_context(open, cx)),
                        ),
                )
                .child(self.render_status_bar(open, cx))
                .into_any_element(),
        };

        v_flex()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_quit))
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
            .children(gpui_kit::component::Root::render_notification_layer(window, cx))
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
fn moved_path(path: &VaultPath, from: &VaultPath, to: &VaultPath, directory: bool) -> VaultPath {
    if path == from {
        return to.clone();
    }
    if directory && let Some(rest) = path.as_str().strip_prefix(&format!("{from}/")) {
        return VaultPath::new(&format!("{to}/{rest}")).expect("mapped vault path");
    }
    path.clone()
}

fn note_path_in(dir: &str, name: &str) -> Result<VaultPath, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("名字不能为空".into());
    }
    if name.contains(['/', '\\']) {
        return Err("名字里不能有 / 或 \\".into());
    }
    if name.chars().any(char::is_control) {
        return Err("名字里不能有换行或控制字符".into());
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
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext};

    use super::{ContextTab, SidebarTab, VaultSession, Workspace};
    use crate::backlinks::BacklinksView;
    use crate::editor::Editor;
    use crate::file_tree::FileTree;
    use crate::paths::PersistedState;
    use crate::search::SearchService;
    use crate::search_view::SearchView;
    use crate::watch::VaultWatcher;

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
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            let open = vault.map(|vault| {
                let path = VaultPath::new("a.md").unwrap();
                let (doc, stamp) = Editor::load(vault.clone(), path.clone()).unwrap();
                let editor = cx.new(|cx| Editor::new(vault.clone(), path, doc, stamp, cx));
                let tree = cx.new(|cx| FileTree::new(vault.clone(), cx));
                let search = cx.new(|cx| SearchService::new(vault.clone(), data.path().into(), cx));
                let search_view = cx.new(|cx| SearchView::new(search.clone(), window, cx));
                let backlinks = cx.new(|cx| BacklinksView::new(search.clone(), cx));
                backlinks.update(cx, |v, cx| v.set_note(Some(VaultPath::new("a.md").unwrap()), cx));
                let images =
                    cx.new(|_| crate::render::RenderService::new(vault.clone(), data.path().join("cache")));
                editor.update(cx, |e, cx| {
                    e.connect_link_preview(search.clone(), cx);
                    e.connect_backlinks(backlinks.clone(), cx);
                    e.connect_images(images.clone(), cx);
                });
                let watcher = cx.new(|cx| VaultWatcher::new(vault.clone(), cx));
                VaultSession {
                    vault,
                    tree,
                    editor: Some(editor),
                    editor_sub: None,
                    search,
                    search_view,
                    backlinks,
                    images,
                    _watcher: watcher,
                    recent: vec![],
                    _subscriptions: vec![],
                }
            });
            let focus_handle = cx.focus_handle();
            focus_handle.focus(window, cx);
            Workspace {
                data_dir: data.path().to_path_buf(),
                state: PersistedState::default(),
                open,
                quick_open: None,
                picker_focus: None,
                recent_commands: vec![],
                sidebar_tab: SidebarTab::Files,
                context_tab: ContextTab::Backlinks,
                rename_pending: false,
                sidebar_visible: true,
                panel_visible: true,
                resizable: cx.new(|_| Default::default()),
                focus_handle,
                error: None,
                _quit_subscription: cx.on_app_quit(|_, _| async {}),
                _activation_subscription: cx.observe_window_activation(window, |_, _, _| {}),
            }
        });
        // 编辑器事件订阅与 `open_note` 一致
        workspace.update_in(cx, |w, window, cx| {
            let editor = w.open.as_ref().and_then(|o| o.editor.clone());
            if let Some(editor) = editor {
                let sub = w.subscribe_editor(&editor, window, cx);
                w.open.as_mut().unwrap().editor_sub = Some(sub);
            }
            if let Some(tree) = w.open.as_ref().map(|o| o.tree.clone()) {
                let sub = w.subscribe_tree(&tree, window, cx);
                w.open.as_mut().unwrap()._subscriptions.push(sub);
            }
            if let Some(view) = w.open.as_ref().map(|o| o.backlinks.clone()) {
                let sub = w.subscribe_backlinks(&view, window, cx);
                w.open.as_mut().unwrap()._subscriptions.push(sub);
            }
        });
        (dir, data, workspace)
    }

    /// 索引线程是真线程：真实等待直到条件成立（最多 20 s）。
    fn wait_until(cx: &mut VisualTestContext, mut ok: impl FnMut(&mut VisualTestContext) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            if ok(cx) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "condition not met within 20 s");
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    }

    /// 没有可恢复的库时停在库选择页，示例库要用户自己点；点了才准备并停在欢迎页。
    #[gpui_kit::test]
    fn startup_stays_on_the_library_chooser_until_the_demo_vault_is_picked(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::actions::init(cx);
        });
        let data = tempfile::tempdir().unwrap();
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::new(None, data.path().to_path_buf(), window, cx));
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(workspace.read(cx).open.is_none(), "启动不打开任何库");
            window.draw(cx).clear(cx);
        });
        assert!(!data.path().join("demo-vault").exists(), "没点就不写示例库");

        let button = cx.debug_bounds("first-run-demo-vault").expect("库选择页有示例库入口");
        cx.simulate_click(button.center(), gpui_kit::Modifiers::default());
        cx.run_until_parked();
        let demo = data.path().join("demo-vault").canonicalize().unwrap();
        cx.update(|_, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.error, None);
            let open = this.open.as_ref().unwrap();
            assert_eq!(open.vault.root(), demo);
            assert_eq!(open.vault.notes().unwrap().len(), 6);
            assert_eq!(open.editor.as_ref().unwrap().read(cx).path().as_str(), crate::demo_vault::WELCOME);
            open.search.read(cx).close_blocking();
        });
        // 示例库进了最近列表：「关闭 Vault」后从那里回来也走示例库入口，能拿到更新过的示例。
        assert_eq!(PersistedState::load(data.path()).recent_vaults, vec![demo.clone()]);
        cx.update(|_, cx| assert!(workspace.read(cx).is_demo_vault(&demo)));
    }

    /// 无参数启动恢复最近打开且还在的库；路径已经不在的条目跳过，不弹错误。
    #[gpui_kit::test]
    fn startup_restores_the_most_recent_vault_and_skips_stale_entries(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::actions::init(cx);
        });
        let data = tempfile::tempdir().unwrap();
        let mine = tempfile::tempdir().unwrap();
        std::fs::write(mine.path().join("mine.md"), "我的笔记\n").unwrap();
        let recents = vec![data.path().join("已经删掉的库"), mine.path().to_path_buf()];
        PersistedState { recent_vaults: recents, ..Default::default() }.save(data.path());
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::new(None, data.path().to_path_buf(), window, cx));
        cx.run_until_parked();
        cx.update(|_, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.error, None);
            let open = this.open.as_ref().unwrap();
            assert_eq!(open.vault.root(), mine.path().canonicalize().unwrap());
            assert_eq!(open.editor.as_ref().unwrap().read(cx).path().as_str(), "mine.md");
            open.search.read(cx).close_blocking();
        });
        assert!(!data.path().join("demo-vault").exists());
    }

    /// 最近打开的是示例库时同样走示例库入口：会交付这一版的示例，并停在欢迎页而不是最近改过的那篇。
    #[gpui_kit::test]
    fn startup_restoring_the_demo_vault_goes_through_the_demo_entry(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::actions::init(cx);
        });
        let data = tempfile::tempdir().unwrap();
        let root = crate::demo_vault::prepare(data.path()).unwrap();
        std::fs::write(root.join("项目/项目清单.md"), "我改过的清单\n").unwrap();
        PersistedState { recent_vaults: vec![root.clone()], ..Default::default() }.save(data.path());
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::new(None, data.path().to_path_buf(), window, cx));
        cx.run_until_parked();
        cx.update(|_, cx| {
            let open = workspace.read(cx).open.as_ref().unwrap();
            assert_eq!(open.vault.root(), root.canonicalize().unwrap());
            assert_eq!(open.editor.as_ref().unwrap().read(cx).path().as_str(), crate::demo_vault::WELCOME);
            open.search.read(cx).close_blocking();
        });
        assert_eq!(std::fs::read_to_string(root.join("项目/项目清单.md")).unwrap(), "我改过的清单\n");
    }

    /// 命令行给了路径就不碰示例库，也不去恢复最近打开的库。
    #[gpui_kit::test]
    fn startup_with_an_explicit_path_skips_the_demo_vault(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::actions::init(cx);
        });
        let data = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let recent = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mine.md"), "我的笔记\n").unwrap();
        let recents = vec![recent.path().to_path_buf()];
        PersistedState { recent_vaults: recents, ..Default::default() }.save(data.path());
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new(Some(dir.path().to_path_buf()), data.path().to_path_buf(), window, cx)
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            let open = workspace.read(cx).open.as_ref().unwrap();
            assert_eq!(open.vault.root(), dir.path().canonicalize().unwrap());
            assert_eq!(open.editor.as_ref().unwrap().read(cx).path().as_str(), "mine.md");
            open.search.read(cx).close_blocking();
        });
        assert!(!data.path().join("demo-vault").exists());
    }

    /// 示例库准备失败不是静默降级：库选择页给出原因，占位的文件保持原样。
    #[gpui_kit::test]
    fn picking_an_unusable_demo_vault_reports_it_on_the_chooser(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::actions::init(cx);
        });
        let data = tempfile::tempdir().unwrap();
        std::fs::write(data.path().join("demo-vault"), "not a directory").unwrap();
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::new(None, data.path().to_path_buf(), window, cx));
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
            window.draw(cx).clear(cx);
        });
        let button = cx.debug_bounds("first-run-demo-vault").unwrap();
        cx.simulate_click(button.center(), gpui_kit::Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            let this = workspace.read(cx);
            assert!(this.open.is_none());
            assert!(this.error.as_deref().unwrap().starts_with("无法准备示例库"), "{:?}", this.error);
        });
        assert_eq!(std::fs::read_to_string(data.path().join("demo-vault")).unwrap(), "not a directory");
    }

    #[gpui_kit::test]
    fn failed_save_blocks_note_and_vault_switching(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("base\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
        });
        let (editor, vault) = cx.update(|_, cx| {
            let session = workspace.read(cx).open.as_ref().unwrap();
            (session.editor.clone().unwrap(), session.vault.clone())
        });
        let long_path = VaultPath::new(&format!("{}.md", "a".repeat(190))).unwrap();
        vault.rename(&VaultPath::new("a.md").unwrap(), &long_path).unwrap();
        cx.update(|window, cx| {
            editor.update(cx, |e, cx| {
                e.set_path(long_path.clone(), cx);
                e.start_writing(window, cx);
            })
        });
        cx.simulate_input("!");
        let local_text = cx.update(|_, cx| editor.read(cx).index_snapshot().1);
        std::fs::write(vault.fs_path(&long_path), "theirs from disk\n").unwrap();
        std::fs::write(dir.path().join("b.md"), "other\n").unwrap();
        let other_vault = tempfile::tempdir().unwrap();
        cx.update(|window, cx| {
            workspace.update(cx, |w, cx| {
                w.open_note(VaultPath::new("b.md").unwrap(), window, cx);
                assert_eq!(w.open.as_ref().unwrap().editor.as_ref(), Some(&editor));
                w.rename_current("should-not-rename".into(), window, cx);
                assert_eq!(editor.read(cx).path(), &long_path);
                assert!(!w.rename_pending, "failed save must prevent rename preparation");
                w.on_close_vault(&crate::actions::CloseVault, window, cx);
                assert!(w.open.is_some());
                w.open_vault(other_vault.path(), window, cx);
                assert_eq!(w.open.as_ref().unwrap().vault.root(), vault.root());
                assert!(!w.flush_before_leaving(window, cx), "close/quit must be vetoed");
            })
        });
        assert_eq!(std::fs::read_to_string(vault.fs_path(&long_path)).unwrap(), "theirs from disk\n");
        cx.update(|_, cx| assert_eq!(editor.read(cx).index_snapshot().1, local_text));
    }

    fn hit_paths(cx: &mut VisualTestContext, search: &Entity<SearchService>, query: &str) -> Vec<String> {
        let result: std::rc::Rc<std::cell::RefCell<Option<Vec<String>>>> = Default::default();
        let sink = result.clone();
        cx.update(|_, cx| {
            let task = search.read(cx).search(query.to_owned(), 20, cx);
            cx.spawn(async move |_| {
                let hits = task.await.unwrap();
                *sink.borrow_mut() = Some(hits.iter().map(|h| h.path.as_str().to_owned()).collect());
            })
            .detach();
        });
        cx.run_until_parked();
        result.borrow().clone().expect("search task finished")
    }

    /// 文件监听的一批变化：文件树重扫、打开中的那篇立刻重载 / 进入已删除态、其余按指纹对齐索引。
    #[gpui_kit::test]
    fn vault_changes_refresh_tree_editor_and_index(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("alpha anchor\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let (tree, editor, search) = cx.update(|_, cx| {
            let open = workspace.read(cx).open.as_ref().unwrap();
            (open.tree.clone(), open.editor.clone().unwrap(), open.search.clone())
        });
        wait_until(cx, |cx| cx.update(|_, cx| search.read(cx).phase() == crate::search::Phase::Idle));
        assert_eq!(hit_paths(cx, &search, "anchor"), ["a.md"]);

        // 外部新建 b.md
        std::fs::write(dir.path().join("b.md"), "bravo anchor\n").unwrap();
        let b = VaultPath::new("b.md").unwrap();
        let batch = crate::watch::Batch {
            changed: vec![b.clone()],
            removed: vec![],
            attachments: vec![],
            rescan: false,
        };
        workspace.update_in(cx, |w, window, cx| w.on_vault_changed(&batch, window, cx));
        cx.update(|_, cx| assert!(tree.read(cx).contains(&b)));
        wait_until(cx, |cx| hit_paths(cx, &search, "anchor").len() == 2);

        // 外部改了打开中的 a.md（干净）：立刻重载，不用等 2 s 轮询
        let a = VaultPath::new("a.md").unwrap();
        let file = dir.path().join("a.md");
        std::fs::write(&file, "alpha rewritten\n").unwrap();
        std::fs::File::open(&file)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(5))
            .unwrap();
        let batch = crate::watch::Batch {
            changed: vec![a.clone()],
            removed: vec![],
            attachments: vec![],
            rescan: false,
        };
        workspace.update_in(cx, |w, window, cx| w.on_vault_changed(&batch, window, cx));
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(editor.read(cx).index_snapshot().1, "alpha rewritten\n");
            assert_eq!(editor.read(cx).banner(), Some(&crate::editor::Banner::Reloaded));
        });
        // 重载事件让索引跟上
        wait_until(cx, |cx| hit_paths(cx, &search, "rewritten") == ["a.md"]);

        // 外部删了打开中的 a.md：编辑器进入已删除态，树里没了，索引里也没了
        std::fs::remove_file(&file).unwrap();
        let batch = crate::watch::Batch {
            changed: vec![],
            removed: vec![a.clone()],
            attachments: vec![],
            rescan: false,
        };
        workspace.update_in(cx, |w, window, cx| w.on_vault_changed(&batch, window, cx));
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert!(editor.read(cx).is_deleted());
            assert!(!tree.read(cx).contains(&a));
            assert_eq!(tree.read(cx).selected(), None);
        });
        wait_until(cx, |cx| hit_paths(cx, &search, "anchor") == ["b.md"]);

        // 横幅"关闭"：编辑器被丢掉
        editor.update(cx, |_, cx| cx.emit(crate::editor::EditorEvent::CloseRequested));
        cx.run_until_parked();
        cx.update(|_, cx| assert!(workspace.read(cx).open.as_ref().unwrap().editor.is_none()));
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
        cx.update(|_, cx| workspace.read(cx).open.as_ref().unwrap().search.read(cx).close_blocking());
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
        cx.update(|_, cx| workspace.read(cx).open.as_ref().unwrap().search.read(cx).close_blocking());
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
        // 临时目录早于 GPUI 实体释放，先停止索引线程，避免在已删除的目录中继续初始化 RocksDB。
        cx.update(|_, cx| workspace.read(cx).open.as_ref().unwrap().search.read(cx).close_blocking());
    }

    #[gpui_kit::test]
    fn wiki_completion_uses_workspace_catalog_when_index_is_unavailable(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("# Source\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let editor = cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
            let open = workspace.read(cx).open.as_ref().unwrap();
            // 停掉索引后再创建文件，验证从磁盘补全标题，而非依赖同步时机。
            open.search.read(cx).close_blocking();
            open.editor.clone().unwrap()
        });
        std::fs::write(dir.path().join("new.md"), "---\ntitle: 尚未索引\n---\n\nbody\n").unwrap();
        cx.update(|window, cx| {
            editor.update(cx, |e, cx| {
                let id = e.headings()[0].2;
                e.focus_block(id, window, cx);
            })
        });
        cx.run_until_parked();
        cx.simulate_input(" [[尚未索引");
        wait_until(cx, |cx| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.debug_bounds("wiki-row-0").is_some()
        });
        cx.simulate_keystrokes("enter");
        cx.update(|_, cx| {
            assert_eq!(editor.read(cx).index_snapshot().1, "# Source [[new]]\n");
        });
    }

    #[gpui_kit::test]
    fn heading_completion_inserts_and_clicks_fragment_without_index(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("# Source\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let editor = cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
            let open = workspace.read(cx).open.as_ref().unwrap();
            open.search.read(cx).close_blocking();
            open.editor.clone().unwrap()
        });
        std::fs::write(dir.path().join("b.md"), "---\ntitle: 尚未索引\n---\n\n# Intro\n\n**中文**\n----\n")
            .unwrap();
        cx.update(|window, cx| editor.update(cx, |e, cx| e.focus_block(e.headings()[0].2, window, cx)));
        cx.run_until_parked();
        cx.simulate_input(" [[尚未索引#文");
        wait_until(cx, |cx| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.debug_bounds("wiki-row-0").is_some()
        });
        cx.simulate_keystrokes("enter");
        cx.update(|_, cx| {
            assert_eq!(editor.read(cx).index_snapshot().1, "# Source [[b#中文]]\n");
            let link = aero_core::links::scan(&editor.read(cx).index_snapshot().1).remove(0);
            editor.update(cx, |_, cx| {
                cx.emit(crate::editor::EditorEvent::LinkClicked {
                    target: link.target,
                    fragment: link.fragment,
                })
            });
        });
        wait_until(cx, |cx| {
            cx.update(|_, cx| {
                let open = workspace.read(cx).open.as_ref().unwrap();
                let e = open.editor.as_ref().unwrap().read(cx);
                e.path().as_str() == "b.md"
                    && e.headings()
                        .iter()
                        .any(|(_, text, id)| text == "中文" && e.highlighted_block() == Some(*id))
            })
        });
        assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), "# Source [[b#中文]]\n");
    }

    #[gpui_kit::test]
    fn block_ref_completion_assigns_indexes_and_clicks_target(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("# Source\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let (editor, search, backlinks) = cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
            let open = workspace.read(cx).open.as_ref().unwrap();
            (open.editor.clone().unwrap(), open.search.clone(), open.backlinks.clone())
        });
        wait_until(cx, |cx| cx.update(|_, cx| search.read(cx).phase() == crate::search::Phase::Idle));
        let original = "---\ntitle: 尚未索引\n---\n\nneedle 正文\n";
        std::fs::write(dir.path().join("b.md"), original).unwrap();
        cx.update(|window, cx| editor.update(cx, |e, cx| e.focus_block(e.headings()[0].2, window, cx)));
        cx.run_until_parked();
        cx.simulate_input(" [[尚未索引#^needle");
        wait_until(cx, |cx| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.debug_bounds("wiki-row-0").is_some()
        });
        assert_eq!(std::fs::read_to_string(dir.path().join("b.md")).unwrap(), original);
        cx.simulate_keystrokes("enter");
        let source = cx.update(|_, cx| editor.read(cx).index_snapshot().1);
        let link = aero_core::links::scan(&source).remove(0);
        assert_eq!(link.target, "b");
        let fragment = link.fragment.clone().unwrap();
        assert!(fragment.starts_with('^'));
        let target = std::fs::read_to_string(dir.path().join("b.md")).unwrap();
        assert!(target.contains(&format!("needle 正文 {fragment}")));
        cx.update(|_, cx| {
            editor.update(cx, |_, cx| {
                cx.emit(crate::editor::EditorEvent::LinkClicked {
                    target: link.target,
                    fragment: link.fragment,
                })
            })
        });
        wait_until(cx, |cx| {
            cx.update(|_, cx| {
                let open = workspace.read(cx).open.as_ref().unwrap();
                let e = open.editor.as_ref().unwrap().read(cx);
                e.path().as_str() == "b.md" && e.highlighted_block().is_some()
            })
        });
        assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), source);
        wait_until(cx, |cx| cx.update(|_, cx| backlinks.read(cx).hits().len() == 1));
        assert_eq!(hit_paths(cx, &search, "needle"), ["b.md"]);
        cx.update(|_, cx| search.read(cx).close_blocking());
    }

    #[gpui_kit::test]
    fn auto_pairs_workspace_completion_save_and_undo(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("# Source\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let editor = cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
            let open = workspace.read(cx).open.as_ref().unwrap();
            open.search.read(cx).close_blocking();
            let editor = open.editor.clone().unwrap();
            editor.update(cx, |e, cx| e.focus_block(e.headings()[0].2, window, cx));
            editor
        });
        std::fs::write(dir.path().join("b.md"), "---\ntitle: Target\n---\n正文\n").unwrap();
        cx.run_until_parked();
        cx.simulate_input(" (**中文**) [[Target");
        wait_until(cx, |cx| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.debug_bounds("wiki-row-0").is_some()
        });
        cx.simulate_keystrokes("enter");
        cx.update(|_, cx| editor.update(cx, |e, cx| e.save_now(cx)));
        wait_until(cx, |cx| {
            cx.update(|_, cx| matches!(editor.read(cx).save_state(), crate::editor::SaveState::Saved(_)))
        });
        assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), "# Source (**中文**) [[b]]\n");
        cx.simulate_keystrokes("cmd-z");
        cx.update(|_, cx| {
            assert_eq!(editor.read(cx).index_snapshot().1, "# Source (**中文**) [[Target]]\n");
            editor.update(cx, |e, cx| e.save_now(cx));
        });
        wait_until(cx, |cx| {
            cx.update(|_, cx| matches!(editor.read(cx).save_state(), crate::editor::SaveState::Saved(_)))
        });
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.md")).unwrap(),
            "# Source (**中文**) [[Target]]\n"
        );
    }

    #[gpui_kit::test]
    fn inline_format_command_palette_restores_selection_and_saves(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("# Source\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let editor = cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
            let open = workspace.read(cx).open.as_ref().unwrap();
            open.search.read(cx).close_blocking();
            let editor = open.editor.clone().unwrap();
            editor.update(cx, |e, cx| e.focus_block(e.headings()[0].2, window, cx));
            editor
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("alt-shift-left");
        cx.simulate_keystrokes("cmd-shift-p");
        cx.simulate_input("粗体");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let row = cx.debug_bounds("picker-row-0").unwrap();
        cx.simulate_click(row.center(), gpui_kit::Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
            assert!(workspace.read(cx).quick_open.is_none());
            assert_eq!(workspace.read(cx).recent_commands.first().copied(), Some("format-bold"));
            assert_eq!(editor.read(cx).index_snapshot().1, "# **Source**\n");
        });
        // 命令面板返回后保持正文选区，可以直接替换，再取消格式。
        cx.simulate_input("中文");
        cx.simulate_keystrokes("shift-left shift-left");
        cx.simulate_keystrokes("cmd-b");
        cx.update(|_, cx| editor.update(cx, |e, cx| e.save_now(cx)));
        wait_until(cx, |cx| {
            cx.update(|_, cx| matches!(editor.read(cx).save_state(), crate::editor::SaveState::Saved(_)))
        });
        assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), "# 中文\n");
    }

    #[gpui_kit::test]
    fn inline_link_command_palette_preserves_selection_and_saves_with_note_completion(
        cx: &mut TestAppContext,
    ) {
        let (dir, _data, workspace) = open(cx, Some("# Source\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let editor = cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
            let open = workspace.read(cx).open.as_ref().unwrap();
            open.search.read(cx).close_blocking();
            let editor = open.editor.clone().unwrap();
            editor.update(cx, |e, cx| e.focus_block(e.headings()[0].2, window, cx));
            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string("https://example.com".into()));
            editor
        });
        std::fs::write(dir.path().join("b.md"), "---\ntitle: Target\n---\n正文\n").unwrap();
        cx.run_until_parked();
        cx.simulate_keystrokes("alt-shift-left");
        cx.simulate_keystrokes("cmd-shift-p");
        cx.simulate_input("插入或编辑链接");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let row = cx.debug_bounds("picker-row-0").unwrap();
        cx.simulate_click(row.center(), gpui_kit::Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
            assert!(workspace.read(cx).quick_open.is_none());
            assert_eq!(workspace.read(cx).recent_commands.first().copied(), Some("edit-link"));
            assert_eq!(editor.read(cx).index_snapshot().1, "# [Source](https://example.com/)\n");
        });
        // 命令面板恢复原选区；新地址选区留在正文，可直接替换。
        cx.simulate_input("https://new.example");
        cx.simulate_keystrokes("cmd-right");
        cx.simulate_input(" ");
        cx.simulate_keystrokes("cmd-k");
        cx.simulate_input("Target");
        wait_until(cx, |cx| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.debug_bounds("wiki-row-0").is_some()
        });
        cx.simulate_keystrokes("enter");
        cx.update(|_, cx| editor.update(cx, |e, cx| e.save_now(cx)));
        wait_until(cx, |cx| {
            cx.update(|_, cx| matches!(editor.read(cx).save_state(), crate::editor::SaveState::Saved(_)))
        });
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.md")).unwrap(),
            "# [Source](https://new.example) [[b]]\n"
        );
    }

    #[gpui_kit::test]
    fn embed_completion_loads_assets_and_note_fragments_without_index(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("# Source\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let editor = cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
            let open = workspace.read(cx).open.as_ref().unwrap();
            open.search.read(cx).close_blocking();
            open.editor.clone().unwrap()
        });
        std::fs::create_dir(dir.path().join("a.assets")).unwrap();
        std::fs::write(dir.path().join("a.assets/图.png"), b"image unchanged").unwrap();
        std::fs::write(dir.path().join("b.md"), "---\ntitle: 嵌入笔记\n---\n\nneedle 正文\n").unwrap();
        cx.update(|window, cx| editor.update(cx, |e, cx| e.focus_block(e.headings()[0].2, window, cx)));
        cx.run_until_parked();
        for query in [" ![[图", " ![[嵌入笔记", " ![[b#^needle"] {
            cx.simulate_input(query);
            wait_until(cx, |cx| {
                cx.update(|window, cx| window.draw(cx).clear(cx));
                cx.debug_bounds("wiki-row-0").is_some()
            });
            cx.simulate_keystrokes("enter");
        }
        let source = cx.update(|_, cx| editor.read(cx).index_snapshot().1);
        assert!(source.starts_with("# Source ![[图.png]] ![[b]] ![[b#^"));
        let embeds = aero_core::links::scan_embeds(&source);
        assert_eq!(embeds.len(), 3);
        let fragment = embeds[2].fragment.as_deref().unwrap();
        assert!(
            std::fs::read_to_string(dir.path().join("b.md"))
                .unwrap()
                .contains(&format!("needle 正文 {fragment}"))
        );
        assert_eq!(std::fs::read(dir.path().join("a.assets/图.png")).unwrap(), b"image unchanged");
        assert!(aero_core::links::scan(&source).is_empty());
        cx.update(|_, cx| editor.update(cx, |e, cx| e.save_now(cx)));
        wait_until(cx, |cx| {
            cx.update(|_, cx| matches!(editor.read(cx).save_state(), crate::editor::SaveState::Saved(_)))
        });
        assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), source);
    }

    #[gpui_kit::test]
    fn tag_completion_workspace_loads_front_matter_and_inserts(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("body\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let editor = cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
            let open = workspace.read(cx).open.as_ref().unwrap();
            open.search.read(cx).close_blocking();
            open.editor.clone().unwrap()
        });
        std::fs::write(dir.path().join("tags.md"), "---\ntags: [rust]\n---\n").unwrap();
        cx.update(|window, cx| editor.update(cx, |e, cx| e.start_writing(window, cx)));
        cx.run_until_parked();
        cx.simulate_input("next #Ru");
        wait_until(cx, |cx| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.debug_bounds("tag-row-0").is_some()
        });
        cx.simulate_keystrokes("enter");
        cx.update(|_, cx| assert!(editor.read(cx).index_snapshot().1.trim_end().ends_with("next #rust")));
    }

    #[gpui_kit::test]
    fn rename_updates_links_editor_tree_search_and_backlinks(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("# Target\n\n自己 [[a]]\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
        });
        let (search, view, editor, tree) = cx.update(|_, cx| {
            let open = workspace.read(cx).open.as_ref().unwrap();
            (open.search.clone(), open.backlinks.clone(), open.editor.clone().unwrap(), open.tree.clone())
        });
        wait_until(cx, |cx| cx.update(|_, cx| search.read(cx).phase() == crate::search::Phase::Idle));
        std::fs::write(dir.path().join("b.md"), "引用 [[a#Target|别名]]\n").unwrap();
        // b 尚未索引；重命名不能依赖索引齐全。
        workspace.update_in(cx, |w, window, cx| w.rename_current("新名字".into(), window, cx));
        wait_until(cx, |cx| cx.update(|_, cx| !workspace.read(cx).rename_pending));
        let new = VaultPath::new("新名字.md").unwrap();
        cx.update(|_, cx| {
            assert_eq!(editor.read(cx).path(), &new);
            assert_eq!(editor.read(cx).index_snapshot().1, "# Target\n\n自己 [[新名字]]\n");
            assert!(tree.read(cx).contains(&new));
            assert!(!tree.read(cx).contains(&VaultPath::new("a.md").unwrap()));
        });
        assert!(!dir.path().join("a.md").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.md")).unwrap(),
            "引用 [[新名字#Target|别名]]\n"
        );
        wait_until(cx, |cx| cx.update(|_, cx| view.read(cx).hits().len() == 1));
        assert_eq!(hit_paths(cx, &search, "Target"), ["新名字.md"]);
        // 后续输入/保存必须写新路径，不得复活旧文件。
        editor.update_in(cx, |e, window, cx| e.start_writing(window, cx));
        cx.simulate_input("追加正文");
        editor.update_in(cx, |e, _, cx| e.save_now(cx));
        wait_until(cx, |cx| {
            cx.update(|_, cx| matches!(editor.read(cx).save_state(), crate::editor::SaveState::Saved(_)))
        });
        assert!(!dir.path().join("a.md").exists());
        assert!(std::fs::read_to_string(dir.path().join("新名字.md")).unwrap().contains("追加正文"));
        cx.update(|_, cx| search.read(cx).close_blocking());
    }

    #[gpui_kit::test]
    fn rename_cancels_when_editing_continues_during_preparation(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("# Target\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
        });
        workspace.update_in(cx, |w, window, cx| {
            w.rename_current("new".into(), window, cx);
            let editor = w.open.as_ref().unwrap().editor.clone().unwrap();
            editor.update(cx, |e, cx| e.start_writing(window, cx));
        });
        cx.simulate_input("继续编辑");
        wait_until(cx, |cx| cx.update(|_, cx| !workspace.read(cx).rename_pending));
        assert!(dir.path().join("a.md").exists());
        assert!(!dir.path().join("new.md").exists());
        cx.update(|_, cx| {
            let e = workspace.read(cx).open.as_ref().unwrap().editor.as_ref().unwrap().read(cx);
            assert_eq!(e.path().as_str(), "a.md");
            assert!(e.index_snapshot().1.contains("继续编辑"));
        });
        cx.update(|_, cx| workspace.read(cx).open.as_ref().unwrap().search.read(cx).close_blocking());
    }

    #[gpui_kit::test]
    fn rename_large_batch_can_be_cancelled_before_any_file_changes(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("# Target\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
        });
        for i in 0..21 {
            std::fs::write(dir.path().join(format!("source-{i}.md")), "[[a]]\n").unwrap();
        }
        workspace.update_in(cx, |w, window, cx| w.rename_current("new".into(), window, cx));
        wait_until(cx, |cx| cx.has_pending_prompt());
        assert!(cx.pending_prompt().unwrap().1.contains("21"));
        assert!(dir.path().join("a.md").exists());
        assert!(!dir.path().join("new.md").exists());
        cx.simulate_prompt_answer("取消");
        wait_until(cx, |cx| cx.update(|_, cx| !workspace.read(cx).rename_pending));
        for i in 0..21 {
            assert_eq!(
                std::fs::read_to_string(dir.path().join(format!("source-{i}.md"))).unwrap(),
                "[[a]]\n"
            );
        }
        assert!(dir.path().join("a.md").exists());
        assert!(!dir.path().join("new.md").exists());
        // 再次发起并接受，同一批引用全部更新。
        workspace.update_in(cx, |w, window, cx| w.rename_current("new".into(), window, cx));
        wait_until(cx, |cx| cx.has_pending_prompt());
        cx.simulate_prompt_answer("重命名并更新");
        wait_until(cx, |cx| cx.update(|_, cx| !workspace.read(cx).rename_pending));
        assert!(!dir.path().join("a.md").exists());
        assert!(dir.path().join("new.md").exists());
        for i in 0..21 {
            assert_eq!(
                std::fs::read_to_string(dir.path().join(format!("source-{i}.md"))).unwrap(),
                "[[new]]\n"
            );
        }
        cx.update(|_, cx| workspace.read(cx).open.as_ref().unwrap().search.read(cx).close_blocking());
    }

    #[gpui_kit::test]
    fn move_directory_updates_open_editor_links_index_and_future_saves(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("[target](group/target.md)\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
        });
        wait_until(cx, |cx| {
            cx.update(|_, cx| {
                workspace.read(cx).open.as_ref().unwrap().search.read(cx).phase()
                    == crate::search::Phase::Idle
            })
        });
        std::fs::create_dir_all(dir.path().join("group/empty")).unwrap();
        std::fs::create_dir(dir.path().join("dest")).unwrap();
        std::fs::write(dir.path().join("group/target.md"), "movingunique\n\n[back](../a.md)\n").unwrap();
        workspace.update_in(cx, |w, window, cx| {
            w.open_note(VaultPath::new("group/target.md").unwrap(), window, cx)
        });
        workspace.update_in(cx, |w, window, cx| {
            w.move_entry(VaultPath::new("group").unwrap(), VaultPath::new("dest/moved").unwrap(), window, cx)
        });
        wait_until(cx, |cx| cx.update(|_, cx| !workspace.read(cx).rename_pending));
        let (editor, search) = cx.update(|_, cx| {
            let open = workspace.read(cx).open.as_ref().unwrap();
            (open.editor.clone().unwrap(), open.search.clone())
        });
        assert_eq!(editor.read_with(cx, |e, _| e.path().as_str().to_owned()), "dest/moved/target.md");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.md")).unwrap(),
            "[target](dest/moved/target.md)\n"
        );
        assert!(dir.path().join("dest/moved/empty").is_dir());
        editor.update_in(cx, |e, window, cx| e.start_writing(window, cx));
        cx.run_until_parked();
        cx.simulate_input("added");
        cx.simulate_keystrokes("cmd-s");
        wait_until(cx, |cx| {
            cx.update(|_, cx| !matches!(editor.read(cx).save_state(), crate::editor::SaveState::Saving))
        });
        assert!(!dir.path().join("group/target.md").exists());
        wait_until(cx, |cx| cx.update(|_, cx| matches!(search.read(cx).phase(), crate::search::Phase::Idle)));
        let task = cx.update(|_, cx| search.read(cx).search("movingunique".into(), 10, cx));
        let hits = cx.foreground_executor().block_on(task).unwrap();
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|hit| hit.path.as_str() == "dest/moved/target.md"));
        assert!(std::fs::read_to_string(dir.path().join("dest/moved/target.md")).unwrap().contains("added"));
        cx.update(|_, cx| search.read(cx).close_blocking());
    }

    #[gpui_kit::test]
    fn file_tree_drag_and_f2_move_files_through_workspace(cx: &mut TestAppContext) {
        use gpui_kit::{Modifiers, MouseButton, point, px};
        let (dir, _data, workspace) = open(cx, Some("body\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        wait_until(cx, |cx| {
            cx.update(|_, cx| {
                workspace.read(cx).open.as_ref().unwrap().search.read(cx).phase()
                    == crate::search::Phase::Idle
            })
        });
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
            let tree = workspace.read(cx).open.as_ref().unwrap().tree.clone();
            tree.update(cx, |t, cx| {
                t.rescan();
                cx.notify();
            });
            window.draw(cx).clear(cx);
        });
        let from = cx.debug_bounds("file-tree-note:a.md").unwrap().center();
        let to = cx.debug_bounds("file-tree-folder:empty").unwrap().center();
        cx.simulate_mouse_move(from, None, Modifiers::default());
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(from + point(px(10.), px(0.)), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
        wait_until(cx, |cx| cx.update(|_, cx| !workspace.read(cx).rename_pending));
        assert!(dir.path().join("empty/a.md").is_file());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let note = cx.debug_bounds("file-tree-note:empty/a.md").unwrap();
        cx.simulate_click(note.center(), Modifiers::default());
        cx.run_until_parked();
        cx.simulate_keystrokes("f2");
        cx.run_until_parked();
        cx.simulate_input("renamed");
        cx.simulate_keystrokes("enter");
        wait_until(cx, |cx| cx.update(|_, cx| !workspace.read(cx).rename_pending));
        assert!(dir.path().join("empty/renamed.md").is_file());
        assert!(!dir.path().join("empty/a.md").exists());
        cx.update(|_, cx| workspace.read(cx).open.as_ref().unwrap().search.read(cx).close_blocking());
    }

    #[gpui_kit::test]
    fn retitle_transaction_updates_incoming_links_without_renaming_file(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("---\ntitle: Old title\n---\nbody\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
        });
        wait_until(cx, |cx| {
            cx.update(|_, cx| {
                workspace.read(cx).open.as_ref().unwrap().search.read(cx).phase()
                    == crate::search::Phase::Idle
            })
        });
        std::fs::write(dir.path().join("b.md"), "[[Old title]] [path](a.md)\n").unwrap();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let title = cx.debug_bounds("editor-title").unwrap();
        cx.simulate_click(title.center(), gpui_kit::Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.simulate_input("New title");
        cx.simulate_keystrokes("enter");
        wait_until(cx, |cx| cx.update(|_, cx| !workspace.read(cx).rename_pending));
        cx.update(|_, cx| {
            let open = workspace.read(cx).open.as_ref().unwrap();
            let editor = open.editor.as_ref().unwrap().read(cx);
            assert_eq!(editor.title(), "New title");
            assert_eq!(editor.path().as_str(), "a.md");
            open.search.read(cx).close_blocking();
        });
        assert_eq!(std::fs::read_to_string(dir.path().join("b.md")).unwrap(), "[[New title]] [path](a.md)\n");
        assert!(!dir.path().join("New title.md").exists());
    }

    #[gpui_kit::test]
    fn backlink_count_and_selected_block_filter_use_full_index(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("first ^one\n\nsecond ^two\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
        });
        let (search, view) = cx.update(|_, cx| {
            let open = workspace.read(cx).open.as_ref().unwrap();
            (open.search.clone(), open.backlinks.clone())
        });
        wait_until(cx, |cx| cx.update(|_, cx| search.read(cx).phase() == crate::search::Phase::Idle));
        std::fs::write(dir.path().join("b.md"), format!("{}\n[[a#^one]] [[a#^two]]\n", "[[a]] ".repeat(205)))
            .unwrap();
        cx.update(|_, cx| search.update(cx, |s, cx| s.note_changed(VaultPath::new("b.md").unwrap(), cx)));
        wait_until(cx, |cx| cx.update(|_, cx| view.read(cx).total() == Some(207)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let unit = cx.debug_bounds("unit-1").unwrap();
        cx.simulate_click(unit.center(), gpui_kit::Modifiers::default());
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let filter = cx.debug_bounds("backlinks-block").expect("selected ^id block exposes its references");
        cx.simulate_click(filter.center(), gpui_kit::Modifiers::default());
        wait_until(cx, |cx| cx.update(|_, cx| view.read(cx).hits().len() == 1));
        cx.update(|_, cx| {
            assert_eq!(view.read(cx).total(), Some(207));
            assert_eq!(view.read(cx).hits()[0].label, "a#^one");
        });
        workspace.update_in(cx, |w, _, cx| {
            w.panel_visible = false;
            cx.notify();
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let count = cx.debug_bounds("backlink-count").unwrap();
        cx.simulate_click(count.center(), gpui_kit::Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, cx| assert!(workspace.read(cx).panel_visible));
        workspace.update_in(cx, |w, window, cx| w.rename_current("renamed".into(), window, cx));
        wait_until(cx, |cx| {
            cx.update(|_, cx| {
                let editor = workspace.read(cx).open.as_ref().unwrap().editor.as_ref().unwrap();
                editor.read(cx).path().as_str() == "renamed.md" && view.read(cx).total() == Some(207)
            })
        });
        // The toast occludes the filter until its virtual-time transitions finish.
        for duration in [400, 5000, 250] {
            cx.executor().advance_clock(std::time::Duration::from_millis(duration));
            cx.run_until_parked();
        }
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let unit = cx.debug_bounds("unit-2").unwrap();
        cx.simulate_click(unit.center(), gpui_kit::Modifiers::default());
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let filter = cx.debug_bounds("backlinks-block").expect("renamed editor still tracks block selection");
        cx.update(|_, cx| {
            let editor = workspace.read(cx).open.as_ref().unwrap().editor.as_ref().unwrap();
            assert_eq!(editor.read(cx).selected_reference(cx).as_deref(), Some("two"));
        });
        let indexed = cx.update(|_, cx| {
            search.read(cx).backlinks(VaultPath::new("renamed.md").unwrap(), Some("^two".into()), 200, cx)
        });
        let indexed = cx.foreground_executor().block_on(indexed).unwrap();
        assert_eq!(indexed.matched, 1);
        assert_eq!(indexed.hits.len(), 1);
        cx.simulate_click(filter.center(), gpui_kit::Modifiers::default());
        wait_until(cx, |cx| cx.update(|_, cx| view.read(cx).hits().len() == 1));
        cx.update(|_, cx| assert_eq!(view.read(cx).hits()[0].label, "renamed#^two"));
        workspace.update_in(cx, |w, window, cx| w.open_note(VaultPath::new("b.md").unwrap(), window, cx));
        wait_until(cx, |cx| cx.update(|_, cx| view.read(cx).total() == Some(0)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("backlinks-block").is_none());
        cx.update(|_, cx| search.read(cx).close_blocking());
    }

    #[gpui_kit::test]
    fn backlinks_refresh_navigate_and_reject_stale_positions(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("# Target\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
        });
        let (search, view) = cx.update(|_, cx| {
            let open = workspace.read(cx).open.as_ref().unwrap();
            (open.search.clone(), open.backlinks.clone())
        });
        wait_until(cx, |cx| cx.update(|_, cx| search.read(cx).phase() == crate::search::Phase::Idle));
        let b = VaultPath::new("b.md").unwrap();
        std::fs::write(dir.path().join("b.md"), "# 引用 [[a]]\n\n> 再次 [[a|目标]]\n").unwrap();
        cx.update(|_, cx| search.update(cx, |s, cx| s.note_changed(b.clone(), cx)));
        wait_until(cx, |cx| cx.update(|_, cx| view.read(cx).hits().len() == 2));

        // 从真实面板行点击到来源标题块。
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let row = cx.debug_bounds("backlink-0").expect("反链行已渲染");
        cx.simulate_click(row.center(), gpui_kit::Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, cx| {
            let editor = workspace.read(cx).open.as_ref().unwrap().editor.as_ref().unwrap().read(cx);
            assert_eq!(editor.path(), &b);
            assert_eq!(editor.highlighted_block(), editor.headings().first().map(|(_, _, id)| *id));
        });

        // 切回目标；保存 / 外部更新后已有结果也需要刷新。
        workspace.update_in(cx, |w, window, cx| w.open_note(VaultPath::new("a.md").unwrap(), window, cx));
        wait_until(cx, |cx| cx.update(|_, cx| view.read(cx).hits().len() == 2));
        std::fs::write(dir.path().join("b.md"), "没有链接了\n").unwrap();
        cx.update(|_, cx| search.update(cx, |s, cx| s.note_changed(b.clone(), cx)));
        wait_until(cx, |cx| cx.update(|_, cx| view.read(cx).hits().is_empty()));

        std::fs::write(dir.path().join("b.md"), "# 来源\n\n[[a]]\n").unwrap();
        cx.update(|_, cx| search.update(cx, |s, cx| s.note_changed(b.clone(), cx)));
        wait_until(cx, |cx| cx.update(|_, cx| view.read(cx).hits().len() == 1));
        // 查询完成后、监听接到事件前内容改变：仍打开来源，但不闪烁错误块。
        std::fs::write(dir.path().join("b.md"), "# 已改动\n\n前面多了一段\n\n[[a]]\n").unwrap();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let row = cx.debug_bounds("backlink-0").unwrap();
        cx.simulate_click(row.center(), gpui_kit::Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, cx| {
            let editor = workspace.read(cx).open.as_ref().unwrap().editor.as_ref().unwrap().read(cx);
            assert_eq!(editor.path(), &b);
            assert!(editor.highlighted_block().is_none(), "stale position must not highlight another block");
        });

        // 连续切换及关闭不能被在途查询的旧结果覆盖。
        workspace.update_in(cx, |w, window, cx| {
            w.open_note(VaultPath::new("a.md").unwrap(), window, cx);
            w.open_note(b, window, cx);
            w.close_editor(window, cx);
        });
        cx.run_until_parked();
        cx.update(|_, cx| assert!(view.read(cx).hits().is_empty()));
    }

    /// 真实点击预览里的 wikilink（mouse down、一帧、mouse up——真机上两者之间必有重绘）：
    /// 不能被"点块进编辑态"吃掉，要经 TextView → 编辑器事件 → 工作区解析，打开目标并按标题片段定位。
    #[gpui_kit::test]
    fn clicking_a_wikilink_in_the_preview_opens_target_and_reveals_heading(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("[[B#**标题**]] 见\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
        });
        std::fs::write(dir.path().join("b.md"), "# 甲\n\n正文\n\n## 标题\n\n更多\n").unwrap();
        let search = cx.update(|_, cx| workspace.read(cx).open.as_ref().unwrap().search.clone());
        wait_until(cx, |cx| cx.update(|_, cx| search.read(cx).phase() == crate::search::Phase::Idle));
        let source = cx.update(|_, cx| workspace.read(cx).open.as_ref().unwrap().editor.clone().unwrap());

        cx.update(|window, cx| window.draw(cx).clear(cx));
        let block = cx.debug_bounds("unit-1").expect("段落已渲染");
        let at = block.origin + gpui_kit::point(gpui_kit::px(6.), gpui_kit::px(10.));
        cx.simulate_mouse_down(at, gpui_kit::MouseButton::Left, gpui_kit::Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.simulate_mouse_up(at, gpui_kit::MouseButton::Left, gpui_kit::Modifiers::default());

        wait_until(cx, |cx| {
            cx.update(|_, cx| {
                let open = workspace.read(cx).open.as_ref().unwrap();
                let Some(editor) = open.editor.as_ref() else { return false };
                let e = editor.read(cx);
                e.path().as_str() == "b.md" && e.highlighted_block().is_some()
            })
        });
        cx.update(|_, cx| {
            assert!(!source.read(cx).is_editing(), "点链接不该把来源块切进编辑态");
            let e = workspace.read(cx).open.as_ref().unwrap().editor.as_ref().unwrap().read(cx);
            // 目标大小写不同（[[B]] → b.md）、片段带行内标记（**标题** → ## 标题）也要命中
            let second = e.headings().get(1).map(|(_, _, id)| *id);
            assert_eq!(e.highlighted_block(), second, "带标题片段的链接应定位到该标题块");
        });
    }

    /// 同名多篇（歧义）：不猜、不创建，编辑器留在原地。
    #[gpui_kit::test]
    fn wikilink_click_on_ambiguous_target_creates_nothing(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("见 [[cargo]]\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
        });
        for dir_name in ["x", "y"] {
            std::fs::create_dir_all(dir.path().join(dir_name)).unwrap();
            std::fs::write(dir.path().join(dir_name).join("cargo.md"), "c\n").unwrap();
        }
        let search = cx.update(|_, cx| workspace.read(cx).open.as_ref().unwrap().search.clone());
        wait_until(cx, |cx| cx.update(|_, cx| search.read(cx).phase() == crate::search::Phase::Idle));
        let editor = cx.update(|_, cx| workspace.read(cx).open.as_ref().unwrap().editor.clone().unwrap());
        cx.update(|_, cx| {
            editor.update(cx, |_, cx| {
                cx.emit(crate::editor::EditorEvent::LinkClicked { target: "cargo".into(), fragment: None })
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!dir.path().join("cargo.md").exists(), "歧义目标不能在源目录新建同名文件");
        cx.update(|_, cx| {
            let open = workspace.read(cx).open.as_ref().unwrap();
            assert_eq!(open.editor.as_ref().unwrap().read(cx).path().as_str(), "a.md");
        });
    }

    /// 未解析链接点击 → 在链接所在目录创建 `<target>.md`（`# 名字` 约定）并打开；
    /// toast 撤销按钮把未改动的文件移入废纸篓并回到来源笔记。
    #[gpui_kit::test]
    fn wikilink_click_creates_missing_note_and_undo_trashes_it(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("见 [[新笔记]]\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
        });
        let editor = cx.update(|_, cx| workspace.read(cx).open.as_ref().unwrap().editor.clone().unwrap());
        cx.update(|_, cx| {
            editor.update(cx, |_, cx| {
                cx.emit(crate::editor::EditorEvent::LinkClicked {
                    target: "新笔记".into(), fragment: None
                })
            });
        });
        let created = dir.path().join("新笔记.md");
        wait_until(cx, |cx| {
            created.exists()
                && cx.update(|_, cx| {
                    let open = workspace.read(cx).open.as_ref().unwrap();
                    let Some(editor) = open.editor.as_ref() else { return false };
                    let e = editor.read(cx);
                    e.path().as_str() == "新笔记.md" && e.index_snapshot().1 == "# 新笔记\n\n"
                })
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        // toast 入场动画是 400ms 墙钟动画：等它结束再点撤销
        std::thread::sleep(std::time::Duration::from_millis(450));
        cx.update(|window, cx| window.click("wikilink-undo", cx));
        wait_until(cx, |cx| {
            !created.exists()
                && cx.update(|_, cx| {
                    workspace
                        .read(cx)
                        .open
                        .as_ref()
                        .and_then(|o| o.editor.as_ref())
                        .is_some_and(|e| e.read(cx).path().as_str() == "a.md")
                })
        });
    }

    /// 新建笔记被编辑过后点撤销：拒绝移动，文件保留、编辑器不动。
    #[gpui_kit::test]
    fn wikilink_undo_refuses_when_created_note_was_edited(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("见 [[新笔记]]\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
        });
        let editor = cx.update(|_, cx| workspace.read(cx).open.as_ref().unwrap().editor.clone().unwrap());
        cx.update(|_, cx| {
            editor.update(cx, |_, cx| {
                cx.emit(crate::editor::EditorEvent::LinkClicked {
                    target: "新笔记".into(), fragment: None
                })
            });
        });
        let created = dir.path().join("新笔记.md");
        wait_until(cx, |cx| {
            created.exists()
                && cx.update(|_, cx| {
                    workspace
                        .read(cx)
                        .open
                        .as_ref()
                        .and_then(|o| o.editor.as_ref())
                        .is_some_and(|e| e.read(cx).path().as_str() == "新笔记.md")
                })
        });
        cx.simulate_input("X");
        cx.executor().advance_clock(std::time::Duration::from_millis(600));
        wait_until(cx, |_| std::fs::read_to_string(&created).is_ok_and(|s| s.contains('X')));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        // toast 入场动画是 400ms 墙钟动画：等它结束再点撤销
        std::thread::sleep(std::time::Duration::from_millis(450));
        cx.update(|window, cx| window.click("wikilink-undo", cx));
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert!(created.exists(), "编辑过的笔记不应被撤销删除");
            let open = workspace.read(cx).open.as_ref().unwrap();
            let e = open.editor.as_ref().unwrap().read(cx);
            assert_eq!(e.path().as_str(), "新笔记.md", "拒绝撤销后应留在新建笔记上");
        });
    }
}
