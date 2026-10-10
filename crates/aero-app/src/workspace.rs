//! 主窗口：标题栏 + 三栏 + 状态栏（`ui_design.md` 第 2 节）、首次运行页（第 11 节）。右侧面板先空着。
//!
//! Workspace 持有当前 vault、文件树与各标签页的编辑器，并处理应用级动作（打开 / 新建 vault、新建笔记、
//! 折叠面板、保存）。编辑器与文件树之间不直接通信，都经由这里转发。
//! 标签页见 `tabs`，标题栏、三栏布局与分割线、路径行见 `chrome`。

mod chrome;
#[cfg(test)]
mod layout_tests;
mod tabs;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use aero_core::{Vault, VaultPath};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Icon, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::actions::QuickOpen as QuickOpen_;
use crate::actions::*;
use crate::backlinks::{BacklinksView, ReferenceOpened};
use crate::branding;
use crate::editor::{Editor, EditorEvent, SaveState};
use crate::file_tree::{FileTree, FileTreeEvent};
use crate::paths::{LinkPreviewMode, PersistedState, display_path};
use crate::quick_open::{QuickOpen, QuickOpenEvent};
use crate::search::{Phase, SearchService};
use crate::search_view::{SearchView, SearchViewEvent};
use crate::tags_view::{TagsView, TagsViewEvent};
use crate::watch::{Batch, VaultWatcher, WatchEvent};
use chrome::{PANEL_DEFAULT, SIDEBAR_DEFAULT, SplitDrag};
use tabs::{OpenIn, Tab};

/// 一个已打开的 vault 及其视图。
struct VaultSession {
    vault: Arc<Vault>,
    tree: Entity<FileTree>,
    /// 标签页，从左到右；至少有一页（可能是空白的「新标签页」）
    tabs: Vec<Tab>,
    active: usize,
    /// 最近关闭的笔记，最新的在后（⌘⇧T 依次重开）
    closed: Vec<VaultPath>,
    /// 全文索引（后台）；状态栏显示进度，搜索视图查询它
    search: Entity<SearchService>,
    semantic: Entity<crate::semantic::SemanticService>,
    search_view: Entity<SearchView>,
    tags_view: Entity<TagsView>,
    backlinks: Entity<BacklinksView>,
    /// 重内容渲染服务（9.9）：图片等按需渲染，缓存落在应用数据目录
    images: Entity<crate::render::RenderService>,
    /// vault 目录的文件监听；起不来时编辑器退回轮询、索引只在启动时对齐
    _watcher: Entity<VaultWatcher>,
    /// 本次会话里打开过的笔记，最近的在前（快速打开的"最近优先"）
    recent: Vec<VaultPath>,
    _subscriptions: Vec<Subscription>,
}

/// 左侧栏当前视图（`ui_design.md` 2.1：文件 / 搜索 / 标签）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarTab {
    Files,
    Search,
    Tags,
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
    rename_pending: bool,
    rename_changes: Batch,
    pending_operation: Option<PendingOperation>,
    session_closing: bool,
    io_task: Option<Task<()>>,
    sidebar_visible: bool,
    panel_visible: bool,
    /// 两侧拖出来的宽度；实际画多宽还看窗口宽度（`chrome::column_widths`）
    sidebar_width: Pixels,
    panel_width: Pixels,
    /// 正在拖的分割线
    split: Option<SplitDrag>,
    /// 后台算附件引用计数（`refresh_attachment_references`）；算的时候又来请求就置 `references_again`
    references_task: Option<Task<()>>,
    references_again: bool,
    next_tab_id: u64,
    tab_scroll: ScrollHandle,
    /// 标签页上的重命名 / 修改标题输入框
    tab_name_edit: Option<tabs::TabNameEdit>,
    /// 上次交给系统的窗口标题（窗口菜单、调度中心里显示）
    window_title: String,
    focus_handle: FocusHandle,
    /// 首次运行页上的错误（打开失败等）
    error: Option<String>,
    _quit_subscription: Subscription,
    _activation_subscription: Subscription,
}

enum PendingOperation {
    OpenVault(PathBuf),
    CloseVault,
    CloseWindow,
    Quit,
    Rename { old: VaultPath, target: VaultPath, title: Option<String> },
}

/// 预览里点 wikilink 后的动作：打开已存在的笔记，或提示同名候选，或失败原因。不创建笔记。
enum LinkAction {
    Open { path: VaultPath, fragment: Option<String> },
    Ambiguous { target: String, candidates: Vec<VaultPath> },
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
            workspace
                .update(cx, |this, cx| {
                    if this.open.is_none() && !this.session_busy() {
                        return true;
                    }
                    this.request_operation(PendingOperation::CloseWindow, window, cx);
                    false
                })
                .unwrap_or(true)
        });
        let quit = cx.on_app_quit(|this, cx| this.shutdown_on_quit(cx));
        // 从别的程序切回来时立刻看一眼磁盘（方案 12）；平时靠编辑器自己的轮询
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if !this.session_busy()
                && window.is_window_active()
                && let Some(open) = this.open.as_ref()
            {
                for editor in open.editors().cloned().collect::<Vec<_>>() {
                    editor.update(cx, |e, cx| e.check_external(cx));
                }
            }
        });
        let preview = state.link_preview;
        crate::editor::set_preview_mode(preview, cx);
        crate::editor::set_focus_shows_source(state.focus_shows_source, cx);
        crate::actions::sync_menus(
            crate::actions::MenuState {
                preview,
                focus_shows_source: state.focus_shows_source,
                note_source: false,
            },
            cx,
        );
        let mut this = Workspace {
            data_dir,
            quick_open: None,
            picker_focus: None,
            recent_commands: Vec::new(),
            sidebar_tab: SidebarTab::Files,
            rename_pending: false,
            rename_changes: Batch::default(),
            pending_operation: None,
            session_closing: false,
            io_task: None,
            sidebar_visible: state.sidebar_visible.unwrap_or(true),
            panel_visible: state.panel_visible.unwrap_or(true),
            sidebar_width: px(state.sidebar_width.unwrap_or(SIDEBAR_DEFAULT)),
            panel_width: px(state.panel_width.unwrap_or(PANEL_DEFAULT)),
            split: None,
            references_task: None,
            references_again: false,
            next_tab_id: 0,
            tab_scroll: ScrollHandle::new(),
            tab_name_edit: None,
            window_title: String::new(),
            state,
            open: None,
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
        if self.open.is_some() {
            // Validate first: an unusable destination must not destroy the current session.
            if let Err(error) = Vault::open(root) {
                window.push_notification(
                    Notification::error(format!("无法打开 {}：{error}", display_path(root))),
                    cx,
                );
                return;
            }
            self.request_operation(PendingOperation::OpenVault(root.to_owned()), window, cx);
            return;
        }
        match Vault::open(root) {
            Ok(vault) => {
                let vault = Arc::new(vault);
                let tree = cx.new(|cx| FileTree::new(vault.clone(), cx));
                let sub = self.subscribe_tree(&tree, window, cx);
                // Any previous session has already completed asynchronous shutdown.
                let index_dir = self.data_dir.join("index");
                let search = cx.new(|cx| SearchService::new(vault.clone(), index_dir, cx));
                let search_sub = cx.observe(&search, |_, _, cx| cx.notify());
                let progress_sub = cx.subscribe(&search, |_, _, _: &crate::search::SearchEvent, cx| {
                    cx.notify();
                });
                let semantic = cx.new(|cx| {
                    crate::semantic::SemanticService::new(vault.clone(), self.data_dir.clone(), cx)
                });
                let search_view = cx.new(|cx| {
                    let mut view = SearchView::new(search.clone(), window, cx);
                    view.connect_semantic(semantic.clone(), cx);
                    view
                });
                let tags_view = cx.new(|cx| TagsView::new(search.clone(), vault.clone(), cx));
                let backlinks = cx.new(|cx| BacklinksView::new(search.clone(), cx));
                let backlinks_sub = self.subscribe_backlinks(&backlinks, window, cx);
                let view_sub =
                    cx.subscribe_in(&search_view, window, |this, _, event, window, cx| match event {
                        SearchViewEvent::Open { path, block_ord, preview } => {
                            this.open_hit(path.clone(), *block_ord, *preview, window, cx)
                        }
                        SearchViewEvent::Settings => this.on_search_settings(&OpenSearchSettings, window, cx),
                        SearchViewEvent::Dismiss => {
                            if let Some(editor) = this.open.as_ref().and_then(|o| o.editor().cloned()) {
                                editor.update(cx, |e, cx| e.focus_browse(window, cx));
                            } else {
                                this.focus_handle.focus(window, cx);
                            }
                        }
                    });
                let tags_sub =
                    cx.subscribe_in(&tags_view, window, |this, _, event, window, cx| match event {
                        TagsViewEvent::Search(tag) => this.search_tag(tag, window, cx),
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
                let tabs = self.first_tabs();
                self.open = Some(VaultSession {
                    vault: vault.clone(),
                    tree,
                    tabs,
                    active: 0,
                    closed: Vec::new(),
                    search,
                    semantic,
                    search_view,
                    tags_view,
                    backlinks,
                    images,
                    _watcher: watcher,
                    recent: Vec::new(),
                    _subscriptions: vec![
                        sub,
                        search_sub,
                        progress_sub,
                        view_sub,
                        tags_sub,
                        watch_sub,
                        backlinks_sub,
                        images_sub,
                    ],
                });
                self.error = None;
                self.refresh_attachment_references(cx);
                self.state.touch_vault(vault.root());
                self.state.save(&self.data_dir);
                let favorites = self.favorites();
                if let Some(open) = self.open.as_ref() {
                    open.tree.update(cx, |t, cx| t.set_favorites(favorites, cx));
                }
                // 恢复上次的标签页；没有记录就打开最近修改的一篇，还没有笔记就停在空白页
                self.restore_tabs(window, cx);
                if self.open.as_ref().is_some_and(|open| open.editor().is_none()) {
                    self.focus_handle.focus(window, cx);
                }
            }
            Err(err) => {
                self.error = Some(format!("无法打开 {}：{err}", display_path(root)));
                self.state.forget_vault(root);
                self.state.save(&self.data_dir);
            }
        }
        cx.notify();
    }

    /// 后台扫一遍附件引用（读每篇笔记的正文），算完把报告交给文件树标"未引用"。
    /// 打开 vault 与附件 / 目录变化后各算一次；只在笔记里加减引用不会立刻刷新计数。
    /// 同时最多一个在算，算的时候又来的请求合并成算完后的一次补算。
    fn refresh_attachment_references(&mut self, cx: &mut Context<Self>) {
        if self.references_task.is_some() {
            self.references_again = true;
            return;
        }
        let Some(open) = self.open.as_ref() else { return };
        let indexer = open.search.read(cx).share_indexer();
        let (vault, tree) = (open.vault.clone(), open.tree.clone());
        self.references_task = Some(cx.spawn(async move |this, cx| {
            let report = cx
                .background_spawn(async move {
                    let targets = indexer
                        .lock()
                        .ok()
                        .and_then(|guard| guard.as_ref().and_then(|index| index.attachment_targets().ok()));
                    if let Some(targets) = targets {
                        return aero_core::attachments::Catalog::read(&vault)
                            .map(|catalog| {
                                aero_core::attachments::References::from_targets(&catalog, targets)
                            })
                            .map_err(|error| log::warn!("attachment references: {error}"))
                            .ok();
                    }
                    aero_core::attachments::References::read(&vault)
                        .map_err(|error| log::warn!("attachment references: {error}"))
                        .ok()
                })
                .await;
            if let Some(report) = report {
                let _ = tree.update(cx, |tree, cx| tree.set_references(report, cx));
            }
            let _ = this.update(cx, |this, cx| {
                this.references_task = None;
                if std::mem::take(&mut this.references_again) {
                    this.refresh_attachment_references(cx);
                }
            });
        }));
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
        self.request_operation(PendingOperation::CloseVault, window, cx);
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

    /// 按标签页的默认规则打开（`tabs` 模块注释）：已打开就切过去，否则放进空白页 / 预览页或右边新开一页。
    /// 别的标签页里的编辑器不受影响，不必先落盘。
    fn open_note(&mut self, path: VaultPath, window: &mut Window, cx: &mut Context<Self>) {
        self.open_note_in(path, OpenIn::Auto, window, cx);
    }

    /// 反链点击 → 打开来源；切换被保存保护否决时不改动当前编辑器。
    fn subscribe_backlinks(
        &mut self,
        view: &Entity<BacklinksView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(view, window, |this, _, event: &ReferenceOpened, window, cx| match event {
            ReferenceOpened::Backlink(hit) => {
                this.open_note(hit.path.clone(), window, cx);
                this.navigate_note(&hit.path, tabs::Navigation::Backlink(hit.clone()), window, cx);
            }
            ReferenceOpened::Outgoing(url) => {
                let path = url.split('#').next().unwrap_or(url);
                let note_url = if !path.contains(':')
                    && (path.to_ascii_lowercase().ends_with(".md") || url.starts_with('#'))
                {
                    format!("{}{}", aero_core::links::NOTE_URL_PREFIX, url.strip_prefix("./").unwrap_or(url))
                } else {
                    url.clone()
                };
                if let Some((target, fragment)) = aero_core::links::decode_note_url(&note_url) {
                    this.on_link_clicked(target, fragment, window, cx);
                } else if let Some(editor) = this.open.as_ref().and_then(|open| open.editor().cloned()) {
                    editor.update(cx, |editor, cx| editor.open_external_link(url, window, cx));
                }
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
            FileTreeEvent::Open(path) | FileTreeEvent::OpenInNewTab(path) => {
                let mode =
                    if matches!(event, FileTreeEvent::Open(_)) { OpenIn::Auto } else { OpenIn::NewTab };
                this.open_note_in(path.clone(), mode, window, cx);
                if let Some(open) = &this.open {
                    open.tree.update(cx, |t, cx| t.focus(window, cx));
                }
            }
            FileTreeEvent::ToggleFavorite(path) => this.toggle_favorite(path.clone(), cx),
            FileTreeEvent::Move { from, to } => this.move_entry(from.clone(), to.clone(), window, cx),
            FileTreeEvent::Touched(paths) => {
                if !paths.is_empty() {
                    let search = this.open.as_ref().map(|open| open.search.clone());
                    if let Some(search) = search {
                        search.update(cx, |search, cx| search.notes_touched(paths.clone(), cx));
                    }
                }
                this.refresh_attachment_references(cx);
            }
        })
    }

    fn subscribe_editor(
        &mut self,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(editor, window, |this, editor, event, window, cx| match event {
            EditorEvent::Changed => {
                if let Some(open) = &this.open {
                    open.semantic.read(cx).touch_activity();
                }
                this.keep_preview_if_edited(editor, cx);
                this.continue_tab_closes(window, cx);
                this.continue_operation(window, cx);
                cx.notify()
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
            EditorEvent::OpenRequested(path) => this.open_note(path.clone(), window, cx),
            EditorEvent::LinkClicked { target, fragment } => {
                this.on_link_clicked(target.clone(), fragment.clone(), window, cx)
            }
            EditorEvent::VaultLink { path, fragment } => {
                this.on_vault_link(path.clone(), fragment.clone(), window, cx)
            }
            EditorEvent::CloseRequested => this.close_editor(editor, window, cx),
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

    /// `[[显示名]](相对路径)`：`.md` 打开笔记并定位，其他已存在的文件在访达中显示。缺失时不创建。
    fn on_vault_link(
        &mut self,
        relative: String,
        fragment: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.open.as_ref() else { return };
        let Some(editor) = open.editor() else { return };
        let note_dir = editor.read(cx).path().parent().to_owned();
        let Some(full) = aero_core::links::resolve_relative(&note_dir, &relative) else {
            window
                .push_notification(Notification::error(format!("无效的路径：{relative}")).title("Aero"), cx);
            return;
        };
        let Ok(path) = VaultPath::new(&full) else {
            window
                .push_notification(Notification::error(format!("无效的路径：{relative}")).title("Aero"), cx);
            return;
        };
        let fs_path = open.vault.fs_path(&path);
        if !fs_path.is_file() {
            window.push_notification(
                Notification::info(format!("找不到：{}", path.as_str())).title("Aero"),
                cx,
            );
            return;
        }
        if path.as_str().to_ascii_lowercase().ends_with(".md") {
            self.open_note(path.clone(), window, cx);
            if let Some(fragment) = fragment {
                self.navigate_note(&path, tabs::Navigation::Fragment(fragment), window, cx);
            }
        } else {
            cx.reveal_path(&fs_path);
        }
    }

    /// 预览里点 wikilink：后台解析（已索引笔记 + 磁盘文件，见 `SearchService::link_resolver`）→ 打开目标；
    /// 带标题片段则定位到对应标题。同名多篇是歧义：提示候选、不猜也不创建。
    /// 找不到的目标只提示，不新建笔记。
    fn on_link_clicked(
        &mut self,
        target: String,
        fragment: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.open.as_ref() else { return };
        let editor = open.editor().cloned();
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
                                LinkAction::Failed(format!("找不到块引用的目标笔记：{target}"))
                            } else {
                                LinkAction::Failed(format!("找不到笔记：{target}"))
                            }
                        }
                    }
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.session_busy() || !this.open.as_ref().is_some_and(|o| Arc::ptr_eq(&o.vault, &vault)) {
                    return;
                }
                match action {
                    LinkAction::Open { path, fragment } => {
                        this.open_note(path.clone(), window, cx);
                        if let Some(fragment) = fragment {
                            this.navigate_note(&path, tabs::Navigation::Fragment(fragment), window, cx);
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
                    LinkAction::Failed(msg) => {
                        window.push_notification(Notification::error(msg).title("Aero"), cx)
                    }
                }
            });
        })
        .detach();
    }

    /// 搜索命中：打开笔记（已打开则复用）并定位到块。不抢焦点——↑↓ 预览时焦点留在搜索框。
    /// ↑↓ 预览放进预览页，⏎ / 点击才占一个普通标签页。
    fn open_hit(
        &mut self,
        path: VaultPath,
        block_ord: u32,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let already = self.open.as_ref().and_then(|o| o.editor()).is_some_and(|e| e.read(cx).path() == &path);
        if !already {
            let focused = window.focused(cx);
            self.open_note_in(path.clone(), if preview { OpenIn::Preview } else { OpenIn::Auto }, window, cx);
            if let Some(f) = focused {
                window.focus(&f, cx);
            }
        } else if !preview {
            self.keep_active_preview();
        }
        self.navigate_note(&path, tabs::Navigation::Block(block_ord), window, cx);
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
        let editor = self.open.as_ref().and_then(|o| o.editor());
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
        if let Some(editor) = self.open.as_ref().and_then(|o| o.editor().cloned()) {
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
            } else if let Some(editor) = self.open.as_ref().and_then(|o| o.editor().cloned()) {
                editor.update(cx, |e, cx| e.focus_browse(window, cx));
            } else {
                self.focus_handle.focus(window, cx);
            }
            if let Some(editor) = self.open.as_ref().and_then(|o| o.editor().cloned()) {
                editor.update(cx, |e, cx| e.set_picker_open(false, window, cx));
            }
            cx.notify();
        }
    }

    /// 快速打开里"新建 X"：在 vault 根（3.5 的默认位置）新建并进入编辑。
    fn create_note_named(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_busy() {
            return;
        }
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
                t.rescan_in_background(cx);
                cx.notify();
            });
        }
        self.open_note(path.clone(), window, cx);
        self.navigate_note(&path, tabs::Navigation::Write, window, cx);
    }

    fn on_open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        crate::settings::open(cx.entity(), window, cx);
    }

    fn on_about(&mut self, _: &About, window: &mut Window, cx: &mut Context<Self>) {
        crate::settings::open_about(window, cx);
    }

    fn on_check_updates(&mut self, _: &CheckForUpdates, window: &mut Window, cx: &mut Context<Self>) {
        crate::settings::open_updates(window, cx);
    }

    fn on_search_settings(&mut self, _: &OpenSearchSettings, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(open) = &self.open {
            crate::search_settings::open(open.semantic.clone(), window, cx);
        } else {
            window.push_notification(Notification::info("请先打开一个知识库，再设置搜索与隐私。"), cx);
        }
    }

    fn on_open_search(&mut self, _: &OpenSearch, window: &mut Window, cx: &mut Context<Self>) {
        if self.open.is_none() {
            return;
        }
        self.show_sidebar(SidebarTab::Search, cx);
        if let Some(open) = self.open.as_ref() {
            open.search_view.update(cx, |v, cx| v.focus(window, cx));
        }
    }

    /// ⌘⇧E：切到文件视图，并在树里定位当前笔记（展开到它、滚到它那一行）。
    fn on_open_files(&mut self, _: &OpenFiles, _window: &mut Window, cx: &mut Context<Self>) {
        self.show_sidebar(SidebarTab::Files, cx);
        if let Some(open) = self.open.as_ref() {
            let current = open.editor().map(|e| e.read(cx).path().clone());
            if current.is_some() {
                open.tree.update(cx, |t, cx| t.select(current, cx));
            }
        }
    }

    pub(crate) fn focus_shows_source(&self) -> bool {
        self.state.focus_shows_source
    }

    pub(crate) fn link_preview(&self) -> LinkPreviewMode {
        self.state.link_preview
    }

    pub(crate) fn semantic_service(&self) -> Option<Entity<crate::semantic::SemanticService>> {
        self.open.as_ref().map(|open| open.semantic.clone())
    }

    pub(crate) fn set_focus_shows_source(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.state.focus_shows_source == on {
            return;
        }
        self.state.focus_shows_source = on;
        self.state.save(&self.data_dir);
        crate::editor::set_focus_shows_source(on, cx);
        for editor in self.open.iter().flat_map(|o| o.editors().cloned()).collect::<Vec<_>>() {
            editor.update(cx, |editor, cx| editor.sync_presentation(cx));
        }
        self.sync_editor_menus(cx);
    }

    pub(crate) fn set_link_preview(
        &mut self,
        mode: LinkPreviewMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state.link_preview == mode {
            return;
        }
        self.state.link_preview = mode;
        self.state.save(&self.data_dir);
        crate::editor::set_preview_mode(mode, cx);
        self.sync_editor_menus(cx);
        for editor in self.open.iter().flat_map(|o| o.editors().cloned()).collect::<Vec<_>>() {
            editor.update(cx, |editor, cx| editor.preview_mode_changed(window, cx));
        }
        cx.notify();
    }

    fn sync_editor_menus(&mut self, cx: &mut Context<Self>) {
        let note_source = self
            .open
            .as_ref()
            .and_then(|open| open.editor())
            .is_some_and(|editor| editor.read(cx).note_source());
        crate::actions::sync_menus(
            crate::actions::MenuState {
                preview: self.state.link_preview,
                focus_shows_source: self.state.focus_shows_source,
                note_source,
            },
            cx,
        );
    }

    fn on_toggle_note_source(
        &mut self,
        _: &crate::actions::ToggleNoteSource,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = self.open.as_ref().and_then(|open| open.editor().cloned()) {
            editor.update(cx, |editor, cx| editor.toggle_note_source(cx));
        }
        self.sync_editor_menus(cx);
    }

    fn on_toggle_focus_source(
        &mut self,
        _: &crate::actions::ToggleFocusSource,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_focus_shows_source(!self.state.focus_shows_source, cx);
    }

    fn on_link_preview_hover(&mut self, _: &LinkPreviewHover, window: &mut Window, cx: &mut Context<Self>) {
        self.set_link_preview(LinkPreviewMode::Hover, window, cx);
    }

    fn on_link_preview_command(
        &mut self,
        _: &LinkPreviewCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_link_preview(LinkPreviewMode::Command, window, cx);
    }

    fn on_link_preview_off(&mut self, _: &LinkPreviewOff, window: &mut Window, cx: &mut Context<Self>) {
        self.set_link_preview(LinkPreviewMode::Off, window, cx);
    }

    fn on_open_tags(&mut self, _: &OpenTags, _window: &mut Window, cx: &mut Context<Self>) {
        self.show_sidebar(SidebarTab::Tags, cx);
    }

    fn search_tag(&mut self, tag: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.open.is_none() {
            return;
        }
        self.show_sidebar(SidebarTab::Search, cx);
        if let Some(open) = self.open.as_ref() {
            open.search_view.update(cx, |v, cx| v.search_tag(tag, window, cx));
        }
    }

    fn on_new_note(&mut self, _: &NewNote, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_busy() {
            return;
        }
        let Some(open) = self.open.as_ref() else { return };
        let vault = open.vault.clone();
        // 3.5：新建位置 = 当前选中的文件夹；否则 vault 根
        let dir = open.tree.read(cx).selected_dir().unwrap_or_default();
        let dir = if dir == "daily" { String::new() } else { dir };
        let path = open.tree.read(cx).available_note_path(&dir, "未命名");
        match vault.create_bytes(&path, b"") {
            Ok(_) => {
                open.tree.update(cx, |t, cx| {
                    t.rescan_in_background(cx);
                    cx.notify();
                });
                self.open_note(path.clone(), window, cx);
                self.navigate_note(&path, tabs::Navigation::Write, window, cx);
            }
            Err(err) => window.push_notification(Notification::error(format!("无法新建笔记：{err}")), cx),
        }
    }

    /// ⌘⇧D：今日日记 `daily/YYYY-MM-DD.md`，不存在则按模板创建（3.4）。
    fn on_open_daily(&mut self, _: &OpenDaily, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_busy() {
            return;
        }
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
                t.rescan_in_background(cx);
                cx.notify();
            });
            true
        };
        self.open_note(path.clone(), window, cx);
        if created {
            self.navigate_note(&path, tabs::Navigation::Write, window, cx);
        }
    }

    fn on_save_now(&mut self, _: &SaveNow, _window: &mut Window, cx: &mut Context<Self>) {
        if self.session_busy() {
            return;
        }
        if let Some(editor) = self.open.as_ref().and_then(|o| o.editor().cloned()) {
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
            .or_else(|| open.editor().map(|e| e.read(cx).path().clone()));
        if let Some(path) = target {
            self.trash_note(&path, window, cx);
        }
    }

    fn on_trash_note(&mut self, action: &TrashNote, window: &mut Window, cx: &mut Context<Self>) {
        if let Ok(path) = VaultPath::new(&action.0) {
            self.trash_note(&path, window, cx);
        }
    }

    /// 移到系统废纸篓。显示这篇的标签页先拿掉、不落盘——它排队中的自动保存会把文件写回来。
    fn trash_note(&mut self, path: &VaultPath, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_busy() {
            return;
        }
        let Some(open) = self.open.as_ref() else { return };
        let fs_path = open.vault.fs_path(path);
        let tab = self.drop_tab_showing(path, window, cx);
        let Some(open) = self.open.as_ref() else { return };
        match crate::trash::move_to_trash(&fs_path) {
            Ok(()) => {
                open.tree.update(cx, |t, cx| {
                    t.rescan_in_background(cx);
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
                let favorites = self.favorites();
                if favorites.contains(path) {
                    self.set_favorites(favorites.into_iter().filter(|p| p != path).collect(), cx);
                }
            }
            Err(err) => {
                window.push_notification(
                    Notification::error(format!("无法移到废纸篓：{err}")).title("Aero"),
                    cx,
                );
                // 文件还在：重新打开
                if tab.is_some() {
                    self.open_note_in(path.clone(), OpenIn::NewTab, window, cx);
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
        let Some(editor) = self.open.as_ref().and_then(|o| o.editor()) else { return };
        let path = editor.read(cx).path().clone();
        self.change_name(path.clone(), path, Some(title), window, cx);
    }

    fn rename_current(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.open.as_ref().and_then(|o| o.editor()) else { return };
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
        self.request_operation(PendingOperation::Rename { old, target, title }, window, cx);
    }

    fn prepare_change_name(
        &mut self,
        old: VaultPath,
        target: VaultPath,
        title: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.open.as_ref() else { return };
        // 各标签页都已落盘；提交前核对它们在准备期间没被改过
        let snapshots: Vec<_> = open
            .editors()
            .map(|e| {
                let (path, _, stamp) = e.read(cx).index_snapshot();
                (e.clone(), path, stamp)
            })
            .collect();
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
                    || snapshots.iter().any(|(editor, path, stamp)| {
                        // 准备期间关掉的标签页不算
                        open.tab_of_editor(editor).is_some() && {
                            let editor = editor.read(cx);
                            editor.path() != path
                                || editor.index_snapshot().2.hash != stamp.hash
                                || editor.has_pending_export()
                                || !matches!(editor.save_state(), SaveState::Clean | SaveState::Saved(_))
                        }
                    })
                {
                    window.push_notification(
                        Notification::warning("笔记在准备期间已变化，重命名已取消，请保存后重试。"),
                        cx,
                    );
                    return;
                }
                this.execute_change_name(plan, old, target, directory, description, window, cx);
            });
        })
        .detach();
    }

    fn execute_change_name(
        &mut self,
        plan: aero_core::rename::RenamePlan,
        old: VaultPath,
        target: VaultPath,
        directory: bool,
        description: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.open.as_ref() else {
            self.rename_pending = false;
            return;
        };
        let editors: Vec<_> = open.editors().cloned().collect();
        // Include tabs opened after preparation. Either all editors are frozen, or none are.
        for editor in &editors {
            if !editor.update(cx, |e, cx| e.suspend_io(cx)) {
                for editor in &editors {
                    editor.update(cx, |e, cx| e.resume_io(cx));
                }
                self.rename_pending = false;
                window.push_notification(
                    Notification::warning("笔记在准备期间已变化，重命名已取消，请保存后重试。"),
                    cx,
                );
                cx.notify();
                return;
            }
        }
        let paths = editors.iter().map(|e| e.read(cx).path().clone()).collect::<Vec<_>>();
        let vault = open.vault.clone();
        self.rename_pending = true;
        self.session_closing = true;
        self.quick_open = None;
        self.picker_focus = None;
        self.focus_handle.focus(window, cx);
        self.io_task = Some(cx.spawn_in(window, async move |this, cx| {
            let (from, to) = (old.clone(), target.clone());
            let (result, loaded) = cx
                .background_spawn(async move {
                    let result = plan.execute();
                    // Read and parse the resulting documents before re-enabling input.
                    // On a failed rollback, follow whichever path actually survived.
                    let loaded = paths
                        .into_iter()
                        .map(|path| {
                            let mapped = moved_path(&path, &from, &to, directory);
                            let path = if result.is_ok() || (!vault.exists(&path) && vault.exists(&mapped)) {
                                mapped
                            } else {
                                path
                            };
                            let document = vault.read(&path);
                            (path, document)
                        })
                        .collect::<Vec<_>>();
                    (result, loaded)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.rename_pending = false;
                this.session_closing = false;
                this.io_task = None;
                let Some(open) = this.open.as_mut() else { return };
                let mut renamed = false;
                match result {
                    Ok(report) => {
                        renamed = true;
                        let message = format!("已完成 {description}，更新 {} 处链接。", report.links_changed);
                        open.search.update(cx, |s, cx| s.notes_renamed(report, cx));
                        open.semantic.update(cx, |s, cx| s.rescan(cx));
                        let map = |p: &VaultPath| moved_path(p, &old, &target, directory);
                        for tab in &mut open.tabs {
                            tab.retarget(&map);
                        }
                        for path in open.recent.iter_mut().chain(open.closed.iter_mut()) {
                            *path = map(path);
                        }
                        window.push_notification(Notification::info(message).title("重命名"), cx);
                    }
                    Err(error) => {
                        open.search.update(cx, |s, cx| s.resync(cx));
                        window.push_notification(Notification::error(error.to_string()).title("重命名"), cx);
                    }
                }
                for (editor, (path, document)) in editors.iter().zip(loaded) {
                    editor.update(cx, |e, cx| e.finish_file_operation(path, document, cx));
                }
                let current = open.editor().map(|e| e.read(cx).path().clone());
                open.backlinks.update(cx, |v, cx| v.set_note(current.clone(), cx));
                if let Some(editor) = open.editor().cloned() {
                    editor.update(cx, |e, cx| e.connect_backlinks(open.backlinks.clone(), cx));
                }
                open.tree.update(cx, |t, cx| {
                    t.rescan_in_background(cx);
                    t.select(current, cx);
                });
                if renamed {
                    let favorites =
                        this.favorites().iter().map(|p| moved_path(p, &old, &target, directory)).collect();
                    this.set_favorites(favorites, cx);
                }
                this.tabs_changed(cx);
                if let Some(active) = this.open.as_ref().map(|o| o.active) {
                    this.load_editor(active, window, cx);
                }
                this.refresh_active_tab(true, window, cx);
                let changes = std::mem::take(&mut this.rename_changes);
                if !changes.is_empty() {
                    this.on_vault_changed(&changes, window, cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// GPUI's irrevocable native quit hook has a timeout and removes all windows before
    /// polling its futures. User actions use `request_operation` BEFORE reaching here.
    /// Keep raw shutdown guards: if GPUI times out, their Drop still joins/destroys C++.
    fn shutdown_on_quit(&mut self, cx: &mut Context<Self>) -> impl std::future::Future<Output = ()> + use<> {
        self.session_closing = true;
        // OS-initiated termination cannot be vetoed in GPUI 0.3.5. This is the final
        // safety fallback; asynchronous user saves must finish before calling quit.
        let saves = self
            .open
            .iter()
            .flat_map(|o| o.editors().cloned())
            .collect::<Vec<_>>()
            .into_iter()
            .map(|editor| editor.update(cx, |e, cx| e.native_save_barrier(cx)))
            .collect::<Vec<_>>();
        let open = self.open.take();
        let shutdown = open.as_ref().map(|open| {
            (
                open.search.read(cx).shutdown_guard(),
                open.semantic.read(cx).shutdown_guard(),
                open.search.read(cx).close_async(cx),
                open.semantic.read(cx).close_async(cx),
            )
        });
        async move {
            for (guard, wait) in saves {
                wait.await;
                drop(guard);
            }
            if let Some((search_guard, semantic_guard, search, semantic)) = shutdown {
                search.await;
                semantic.await;
                drop((search_guard, semantic_guard));
            }
            drop(open);
        }
    }

    /// Synchronous test/Drop fallback, never used for ordinary user actions.
    #[cfg(test)]
    fn flush_all(&mut self, cx: &mut Context<Self>) {
        for editor in self.open.iter().flat_map(|o| o.editors().cloned()).collect::<Vec<_>>() {
            match editor.update(cx, |e, _| e.flush_sync()) {
                Ok(Some(copy)) => log::warn!("conflict copy written on quit: {}", copy.as_str()),
                Ok(None) => {}
                Err(err) => log::error!("flush: {err}"),
            }
        }
        if let Some(open) = self.open.as_ref() {
            open.semantic.read(cx).close_blocking();
            open.search.read(cx).close_blocking();
        }
    }

    fn session_busy(&self) -> bool {
        self.session_closing
    }

    fn request_operation(
        &mut self,
        operation: PendingOperation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.rename_pending || self.pending_operation.is_some() || self.session_busy() {
            window
                .push_notification(Notification::warning("正在完成文件操作，请稍后再试。").title("Aero"), cx);
            return;
        }
        self.pending_operation = Some(operation);
        self.continue_operation(window, cx);
    }

    /// Save completion drives this continuation; newer edits cause another save, never a discard.
    fn continue_operation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_operation.is_none() || self.session_busy() {
            return;
        }
        let editors: Vec<_> = self.open.iter().flat_map(|o| o.editors().cloned()).collect();
        if editors.iter().any(|e| {
            let e = e.read(cx);
            e.has_pending_export() || e.is_deleted() || matches!(e.save_state(), SaveState::Failed(_))
        }) {
            self.pending_operation = None;
            window.push_notification(
                Notification::warning("笔记尚未安全保存，请处理保存错误或导出后重试。").title("Aero"),
                cx,
            );
            return;
        }
        let mut waiting = false;
        for editor in editors {
            match editor.read(cx).save_state() {
                SaveState::Dirty => {
                    waiting = true;
                    editor.update(cx, |e, cx| e.save_now(cx));
                }
                SaveState::Saving => waiting = true,
                _ => {}
            }
        }
        if waiting {
            return;
        }
        let operation = self.pending_operation.take().unwrap();
        if let PendingOperation::Rename { old, target, title } = operation {
            self.prepare_change_name(old, target, title, window, cx);
            return;
        }
        for editor in self.open.iter().flat_map(|open| open.editors()) {
            editor.update(cx, |e, cx| {
                e.suspend_io(cx);
            });
        }
        self.session_closing = true;
        self.quick_open = None;
        self.picker_focus = None;
        self.focus_handle.focus(window, cx);
        let tasks = self
            .open
            .as_ref()
            .map(|open| (open.search.read(cx).close_async(cx), open.semantic.read(cx).close_async(cx)));
        // The session stays owned by Workspace, including its editors and service Drop guards.
        // Native close is vetoed until this continuation removes the window.
        self.io_task = Some(cx.spawn_in(window, async move |this, cx| {
            let started = Instant::now();
            if let Some((search, semantic)) = tasks {
                search.await;
                semantic.await;
            }
            log::debug!("workspace close: services stopped after {:?}", started.elapsed());
            let _ = this.update_in(cx, |this, window, cx| {
                this.open = None;
                log::debug!("workspace close: session released after {:?}", started.elapsed());
                this.session_closing = false;
                this.io_task = None;
                this.references_task = None;
                this.references_again = false;
                this.sync_editor_menus(cx);
                match operation {
                    PendingOperation::OpenVault(root) => {
                        this.open_vault(&root, window, cx);
                        if this.is_demo_vault(&root) {
                            this.open_note(VaultPath::new(crate::demo_vault::WELCOME).unwrap(), window, cx);
                        }
                    }
                    PendingOperation::CloseVault => {}
                    PendingOperation::CloseWindow => window.remove_window(),
                    PendingOperation::Quit => cx.quit(),
                    PendingOperation::Rename { .. } => unreachable!(),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn on_quit(&mut self, _: &Quit, window: &mut Window, cx: &mut Context<Self>) {
        self.request_operation(PendingOperation::Quit, window, cx);
    }

    /// 编辑器刚落盘（或重载）：把它的内容送进索引。
    fn index_editor(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let (path, text, stamp) = editor.read(cx).index_snapshot();
        open.semantic.update(cx, |s, cx| s.note_changed(vec![path.clone()], cx));
        open.search.update(cx, |s, cx| s.note_saved(path, text, stamp, cx));
    }

    /// 编辑器改动了其他路径：笔记副本 / 另存为对齐索引与标签页；附件导出只刷新树、引用与渲染缓存。
    fn on_editor_touched_paths(
        &mut self,
        editor: &Entity<Editor>,
        paths: Vec<VaultPath>,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.open.as_mut() else { return };
        // 与 Vault::notes 一致：附件目录里的 .md 也不是笔记，不能送进文本索引。
        let (notes, attachments): (Vec<_>, Vec<_>) = paths
            .into_iter()
            .partition(|p| p.is_note() && !p.parent().split('/').any(|part| part.ends_with(".assets")));
        let notes_touched = !notes.is_empty();
        let current = editor.read(cx).path().clone();
        let active = open.editor() == Some(editor);
        open.tree.update(cx, |t, cx| {
            t.rescan_in_background(cx);
            if notes_touched && active && t.selected() != Some(&current) {
                t.select(Some(current.clone()), cx);
            }
            cx.notify();
        });
        if notes_touched {
            open.recent.retain(|p| p != &current);
            open.recent.insert(0, current);
            open.semantic.update(cx, |s, cx| s.note_changed(notes.clone(), cx));
            open.search.update(cx, |s, cx| s.notes_touched(notes, cx));
        }
        if !attachments.is_empty() {
            open.images.update(cx, |s, cx| s.invalidate(&attachments, cx));
            self.refresh_attachment_references(cx);
        }
        if notes_touched {
            self.tabs_changed(cx);
        }
        cx.notify();
    }

    /// 打开中的笔记在外部被删：编辑器已显示横幅并停止保存；这里让文件树与索引跟上。
    fn on_editor_deleted(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let path = editor.read(cx).path().clone();
        open.tree.update(cx, |t, cx| {
            t.rescan_in_background(cx);
            if t.selected() == Some(&path) {
                t.select(None, cx);
            }
            cx.notify();
        });
        open.semantic.update(cx, |s, cx| s.note_changed(vec![path.clone()], cx));
        open.search.update(cx, |s, cx| s.note_removed(path, cx));
        cx.notify();
    }

    /// vault 目录有动静（文件监听）：文件树重扫；标签页里打开着的笔记立刻检查外部修改；其余按指纹对齐索引。
    fn on_vault_changed(&mut self, batch: &Batch, _window: &mut Window, cx: &mut Context<Self>) {
        if self.session_busy() {
            if self.rename_pending {
                // Do not lose events delivered while the transaction owns the files.
                self.rename_changes.changed.extend(batch.changed.iter().cloned());
                self.rename_changes.removed.extend(batch.removed.iter().cloned());
                self.rename_changes.attachments.extend(batch.attachments.iter().cloned());
                for paths in [
                    &mut self.rename_changes.changed,
                    &mut self.rename_changes.removed,
                    &mut self.rename_changes.attachments,
                ] {
                    paths.sort();
                    paths.dedup();
                }
                self.rename_changes.rescan |= batch.rescan;
            }
            return;
        }
        let Some(open) = self.open.as_ref() else { return };
        let editors: Vec<Entity<Editor>> = open.editors().cloned().collect();
        let shown: Vec<VaultPath> = editors.iter().map(|e| e.read(cx).path().clone()).collect();
        // 只改了已有笔记的内容（包括我们自己的自动保存）不动文件树；增删、目录与附件变化才重扫，而且在后台扫
        let tree_changed = batch.rescan
            || !batch.removed.is_empty()
            || !batch.attachments.is_empty()
            || batch.changed.iter().any(|p| !open.tree.read(cx).contains(p));
        if tree_changed {
            open.tree.update(cx, |t, cx| t.rescan_in_background(cx));
        }
        let mine = |p: &VaultPath| shown.contains(p);
        let changed: Vec<VaultPath> = batch.changed.iter().chain(&batch.removed).cloned().collect();
        open.semantic.update(cx, |s, cx| {
            s.note_changed(changed.clone(), cx);
            if batch.rescan {
                s.rescan(cx);
            }
        });
        for (editor, path) in editors.iter().zip(&shown) {
            if changed.contains(path) {
                // 编辑器自己处理：重载 / 冲突副本 / 已删除，并通过事件把索引带上
                editor.update(cx, |e, cx| e.check_external(cx));
            }
            // 被嵌入的笔记变了：丢掉嵌入缓存重读，嵌入块跟着更新
            editor.update(cx, |e, cx| e.invalidate_embeds(&changed, cx));
        }
        // 附件变化只影响渲染缓存：外部换了图，打开中的笔记下一帧就重新渲染
        open.images.update(cx, |s, cx| s.invalidate(&batch.attachments, cx));
        if !batch.attachments.is_empty() || batch.rescan {
            self.refresh_attachment_references(cx);
        }
        let touched: Vec<VaultPath> = changed.iter().filter(|p| !mine(p)).cloned().collect();
        let Some(open) = self.open.as_ref() else { return };
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
        self.save_layout();
        cx.notify();
    }

    fn on_toggle_panel(&mut self, _: &TogglePanel, _window: &mut Window, cx: &mut Context<Self>) {
        self.panel_visible = !self.panel_visible;
        self.save_layout();
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
                                    .tooltip("内置示例笔记，随程序更新")
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
        let (title, buttons, body): (&'static str, Vec<AnyElement>, AnyElement) = match self.sidebar_tab {
            SidebarTab::Files => ("文件", self.explorer_buttons(cx), open.tree.clone().into_any_element()),
            SidebarTab::Search => ("搜索", Vec::new(), open.search_view.clone().into_any_element()),
            SidebarTab::Tags => ("标签", Vec::new(), open.tags_view.clone().into_any_element()),
        };
        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .child(self.render_pane_header(title, buttons, cx))
            .child(div().flex_1().min_h(px(0.)).child(body))
    }

    fn render_context(&self, _open: &VaultSession, cx: &mut Context<Self>) -> impl IntoElement {
        let (background, border) = (cx.theme().sidebar, cx.theme().sidebar_border);
        // 大纲先拿掉，这块先空着。
        div()
            .id("context-blank")
            .debug_selector(|| "context-blank".into())
            .size_full()
            .bg(background)
            .border_l_1()
            .border_color(border)
    }

    fn render_editor_area(&self, open: &VaultSession, cx: &mut Context<Self>) -> impl IntoElement {
        let background = cx.theme().background;
        let body = match open.editor() {
            Some(editor) => editor.clone().into_any_element(),
            None if open.tabs[open.active].path(cx).is_some() => div()
                .debug_selector(|| "note-loading".into())
                .p_6()
                .text_color(cx.theme().muted_foreground)
                .child("正在读取笔记…")
                .into_any_element(),
            None => self.render_empty_tab(open, cx).into_any_element(),
        };
        v_flex()
            .size_full()
            .bg(background)
            .children(self.render_breadcrumb(open, cx))
            .child(div().flex_1().min_h(px(0.)).child(body))
    }

    /// 状态栏（2.1）：vault 名；右边索引进度、保存状态、字数。面板开关在标题栏上。
    fn render_status_bar(&self, open: &VaultSession, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let root = open.vault.root().to_path_buf();
        let path_hint = format!("{}\n点击复制路径；⌘点击在文件管理器中打开", root.display());
        let right: Vec<AnyElement> = match open.editor() {
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
                vec![
                    div().text_color(color).child(label).into_any_element(),
                    div().child(words).into_any_element(),
                ]
            }
            None => vec![],
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
            .debug_selector(|| "status-bar".into())
            .flex_none()
            .h(px(24.))
            .w_full()
            .px_3()
            .gap_3()
            .items_center()
            .justify_between()
            .text_xs()
            .text_color(muted)
            .bg(theme.sidebar)
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .id("status-vault-path")
                    .debug_selector(|| "status-vault-path".into())
                    .min_w_0()
                    .truncate()
                    .cursor_pointer()
                    .hover(|style| style.text_color(theme.foreground))
                    .child(display_path(&root))
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(path_hint.clone()).build(window, cx)
                    })
                    .on_click(move |event: &ClickEvent, _, cx| {
                        if event.modifiers().platform {
                            cx.open_with_system(&root);
                        } else {
                            cx.write_to_clipboard(ClipboardItem::new_string(root.display().to_string()));
                        }
                    }),
            )
            .child(h_flex().flex_none().gap_3().items_center().children(index_item).children(right))
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (background, foreground) = (cx.theme().background, cx.theme().foreground);
        // 标签页上已经有标题；系统的窗口菜单与调度中心仍用窗口标题
        let title = match &self.open {
            Some(open) => match open.editor() {
                Some(e) => format!("{} — Aero", e.read(cx).title()),
                None => format!("{} — Aero", display_path(open.vault.root())),
            },
            None => "Aero".to_owned(),
        };
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }

        let body: AnyElement = match &self.open {
            None => self.render_first_run(cx).into_any_element(),
            Some(open) => v_flex()
                .flex_1()
                .min_h(px(0.))
                .child(self.render_columns(open, window, cx))
                .child(self.render_status_bar(open, cx))
                .into_any_element(),
        };

        v_flex()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_quit))
            .when(!self.session_busy(), |el| {
                el.on_action(cx.listener(Self::on_open_vault))
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
                    .on_action(cx.listener(Self::on_open_settings))
                    .on_action(cx.listener(Self::on_about))
                    .on_action(cx.listener(Self::on_check_updates))
                    .on_action(cx.listener(Self::on_search_settings))
                    .on_action(cx.listener(Self::on_open_files))
                    .on_action(cx.listener(Self::on_open_tags))
                    .on_action(cx.listener(Self::on_show_backlinks))
                    .on_action(cx.listener(Self::on_show_outline))
                    .on_action(cx.listener(Self::on_toggle_note_source))
                    .on_action(cx.listener(Self::on_toggle_focus_source))
                    .on_action(cx.listener(Self::on_link_preview_hover))
                    .on_action(cx.listener(Self::on_link_preview_command))
                    .on_action(cx.listener(Self::on_link_preview_off))
                    .on_action(cx.listener(Self::on_quick_open))
                    .on_action(cx.listener(Self::on_command_palette))
                    .on_action(cx.listener(Self::on_toggle_sidebar))
                    .on_action(cx.listener(Self::on_toggle_panel))
                    .on_action(cx.listener(Self::on_new_tab))
                    .on_action(cx.listener(Self::on_close_tab))
                    .on_action(cx.listener(Self::on_close_other_tabs))
                    .on_action(cx.listener(Self::on_rename_active_tab))
                    .on_action(cx.listener(Self::on_retitle_active_tab))
                    .on_action(cx.listener(Self::on_reopen_closed_tab))
                    .on_action(cx.listener(Self::on_next_tab))
                    .on_action(cx.listener(Self::on_prev_tab))
                    .on_action(cx.listener(Self::on_activate_tab))
                    .on_action(cx.listener(Self::on_toggle_favorite))
                    .on_action(cx.listener(Self::on_toggle_favorite_note))
                    .on_action(cx.listener(Self::on_open_in_new_tab))
            })
            .size_full()
            .bg(background)
            .text_color(foreground)
            .child(self.render_title_bar(window, cx))
            .child(body)
            .children(gpui_kit::component::Root::render_dialog_layer(window, cx))
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
            .when(self.session_busy(), |el| {
                // Keep the note and window chrome in place while cleanup runs.
                // Editors are suspended and focus is on Workspace; block pointer
                // and keyboard input so visible controls cannot start more work.
                el.capture_key_down(|_, _, cx| cx.stop_propagation()).child(deferred(
                    div()
                        .id("workspace-busy")
                        .debug_selector(|| "workspace-busy".into())
                        .absolute()
                        .inset_0()
                        .occlude()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .px_4()
                                .py_2()
                                .rounded_md()
                                .bg(background)
                                .border_1()
                                .border_color(cx.theme().border)
                                .shadow_md()
                                .text_sm()
                                .child(if self.rename_pending {
                                    "正在完成文件操作…"
                                } else {
                                    "正在关闭笔记库…"
                                }),
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
    use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext};

    use super::{SidebarTab, Tab, VaultSession, Workspace};
    use crate::actions::NewTab;
    use crate::backlinks::BacklinksView;
    use crate::editor::{Editor, SaveState};
    use crate::file_tree::FileTree;
    use crate::paths::PersistedState;
    use crate::search::SearchService;
    use crate::search_view::SearchView;
    use crate::tags_view::TagsView;
    use crate::watch::{Batch, VaultWatcher};
    use gpui_kit::px;

    #[gpui_kit::test]
    fn settings_dialog_opens_editor_search_and_about(cx: &mut TestAppContext) {
        use gpui_kit::Modifiers;
        let (_dir, _data, workspace) = open(cx, Some("body\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
        });
        workspace
            .update_in(cx, |w, window, cx| w.on_open_settings(&crate::actions::OpenSettings, window, cx));
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("settings-panel").is_some());
        assert!(cx.debug_bounds("settings-editor").is_some());
        let about = cx.debug_bounds("settings-nav-about").unwrap();
        cx.simulate_click(about.center(), Modifiers::default());
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("settings-about").is_some());
        let search = cx.debug_bounds("settings-nav-search").unwrap();
        cx.simulate_click(search.center(), Modifiers::default());
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("settings-search").is_some());
        assert!(cx.debug_bounds("search-settings-panel").is_some(), "search preferences sit inside settings");
        cx.update(|_, cx| {
            let open = workspace.read(cx).open.as_ref().unwrap();
            open.semantic.read(cx).close_blocking();
            open.search.read(cx).close_blocking();
        });
    }

    #[gpui_kit::test]
    fn search_settings_dialog_is_visible_from_workspace(cx: &mut TestAppContext) {
        let (_dir, _data, workspace) = open(cx, Some("body\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
        });
        workspace.update_in(cx, |w, window, cx| {
            w.on_search_settings(&crate::actions::OpenSearchSettings, window, cx)
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        let visible = cx.debug_bounds("search-settings-panel");
        cx.update(|_, cx| {
            let open = workspace.read(cx).open.as_ref().unwrap();
            open.semantic.read(cx).close_blocking();
            open.search.read(cx).close_blocking();
        });
        assert!(
            visible.is_some_and(|b| b.size.width > px(300.) && b.size.height > px(100.)),
            "settings must render in the real workspace"
        );
    }

    pub(super) fn open(
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
                let semantic =
                    cx.new(|cx| crate::semantic::SemanticService::new(vault.clone(), data.path().into(), cx));
                let search_view = cx.new(|cx| SearchView::new(search.clone(), window, cx));
                let tags_view = cx.new(|cx| TagsView::new(search.clone(), vault.clone(), cx));
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
                    tabs: vec![Tab::loaded(1, editor)],
                    active: 0,
                    closed: vec![],
                    search,
                    semantic,
                    search_view,
                    tags_view,
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
                rename_pending: false,
                rename_changes: Batch::default(),
                pending_operation: None,
                session_closing: false,
                io_task: None,
                sidebar_visible: true,
                panel_visible: true,
                sidebar_width: px(super::SIDEBAR_DEFAULT),
                panel_width: px(super::PANEL_DEFAULT),
                split: None,
                references_task: None,
                references_again: false,
                next_tab_id: 1,
                tab_scroll: gpui_kit::ScrollHandle::new(),
                tab_name_edit: None,
                window_title: String::new(),
                focus_handle,
                error: None,
                _quit_subscription: cx.on_app_quit(|_, _| async {}),
                _activation_subscription: cx.observe_window_activation(window, |_, _, _| {}),
            }
        });
        // 编辑器事件订阅与 `open_note` 一致
        workspace.update_in(cx, |w, window, cx| {
            let editor = w.open.as_ref().and_then(|o| o.editor().cloned());
            if let Some(editor) = editor {
                let sub = w.subscribe_editor(&editor, window, cx);
                w.open.as_mut().unwrap().tabs[0].subscribe(sub);
            }
            if let Some(tree) = w.open.as_ref().map(|o| o.tree.clone()) {
                let sub = w.subscribe_tree(&tree, window, cx);
                w.open.as_mut().unwrap()._subscriptions.push(sub);
            }
            if let Some(view) = w.open.as_ref().map(|o| o.backlinks.clone()) {
                let sub = w.subscribe_backlinks(&view, window, cx);
                w.open.as_mut().unwrap()._subscriptions.push(sub);
            }
            if let Some(view) = w.open.as_ref().map(|o| o.tags_view.clone()) {
                let sub = cx.subscribe_in(&view, window, |this, _, event, window, cx| match event {
                    crate::tags_view::TagsViewEvent::Search(tag) => this.search_tag(tag, window, cx),
                });
                w.open.as_mut().unwrap()._subscriptions.push(sub);
            }
        });
        (dir, data, workspace)
    }

    /// 标签侧栏（4.3）：分级树带计数，点击切到搜索并执行 `tag:#…`，父标签含子标签。
    #[gpui_kit::test]
    fn tag_sidebar_nests_and_clicks_through_to_search(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("hello #rust\n"));
        std::fs::write(dir.path().join("b.md"), "tools #rust/cli\n").unwrap();
        std::fs::write(dir.path().join("c.md"), "笔记 #中文\n").unwrap();
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let (search, tags) = cx.update(|_, cx| {
            let open = workspace.read(cx).open.as_ref().unwrap();
            (open.search.clone(), open.tags_view.clone())
        });
        wait_until(cx, |cx| cx.update(|_, cx| search.read(cx).phase() == crate::search::Phase::Idle));
        cx.update(|_, cx| {
            search.update(cx, |s, cx| {
                s.note_changed(VaultPath::new("b.md").unwrap(), cx);
                s.note_changed(VaultPath::new("c.md").unwrap(), cx);
            });
        });
        wait_until(cx, |cx| {
            cx.update(|_, cx| {
                let labels = tags.read(cx).labels();
                labels.iter().any(|(p, c, _)| p == "rust" && *c >= 2)
                    && labels.iter().any(|(p, _, d)| p == "rust/cli" && *d == 1)
                    && labels.iter().any(|(p, _, _)| p == "中文")
            })
        });
        workspace.update_in(cx, |w, window, cx| w.on_open_tags(&crate::actions::OpenTags, window, cx));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("tags-tree").is_some());
        assert!(cx.debug_bounds("tag-row:rust/cli").is_some());
        let row = cx.debug_bounds("tag-row:rust/cli").unwrap();
        cx.simulate_click(row.center(), gpui_kit::Modifiers::default());
        cx.run_until_parked();
        cx.executor().advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        cx.update(|_, cx| {
            let open = workspace.read(cx).open.as_ref().unwrap();
            assert_eq!(workspace.read(cx).sidebar_tab, SidebarTab::Search);
            let view = open.search_view.read(cx);
            assert_eq!(view.query(), "tag:#rust/cli");
            assert_eq!(view.hits().len(), 1);
            assert_eq!(view.hits()[0].path.as_str(), "b.md");
        });
        workspace.update_in(cx, |w, window, cx| w.on_open_tags(&crate::actions::OpenTags, window, cx));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let parent = cx.debug_bounds("tag-row:rust").unwrap();
        cx.simulate_click(parent.center(), gpui_kit::Modifiers::default());
        cx.run_until_parked();
        cx.executor().advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        cx.update(|_, cx| {
            let view = workspace.read(cx).open.as_ref().unwrap().search_view.read(cx);
            assert_eq!(view.query(), "tag:#rust");
            let mut paths: Vec<_> = view.hits().iter().map(|h| h.path.as_str()).collect();
            paths.sort();
            assert_eq!(paths, ["a.md", "b.md"]);
        });
        cx.update(|_, cx| search.read(cx).close_blocking());
    }

    /// 索引线程是真线程：真实等待直到条件成立（最多 20 s）。
    pub(super) fn wait_until(cx: &mut VisualTestContext, mut ok: impl FnMut(&mut VisualTestContext) -> bool) {
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
            assert_eq!(open.vault.notes().unwrap().len(), crate::demo_vault::note_count());
            assert_eq!(open.editor().unwrap().read(cx).path().as_str(), crate::demo_vault::WELCOME);
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
            assert_eq!(open.editor().unwrap().read(cx).path().as_str(), "mine.md");
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
            assert_eq!(open.editor().unwrap().read(cx).path().as_str(), crate::demo_vault::WELCOME);
            open.search.read(cx).close_blocking();
        });
        assert_eq!(std::fs::read_to_string(root.join("项目/项目清单.md")).unwrap(), "我改过的清单\n");
    }

    /// 命令行给了路径就不碰示例库，也不去恢复最近打开的库。
    #[gpui_kit::test]
    fn startup_finds_recent_note_in_background_without_replacing_user_choice(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::actions::init(cx);
        });
        let data = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "recent\n").unwrap();
        std::fs::write(dir.path().join("b.md"), "chosen\n").unwrap();
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            let mut w = Workspace::new(Some(dir.path().to_path_buf()), data.path().into(), window, cx);
            let open = w.open.as_ref().unwrap();
            assert!(open.editor().is_none());
            assert!(open.tabs[0].path(cx).is_none(), "constructing the window must not walk the vault");
            w.open_note(VaultPath::new("b.md").unwrap(), window, cx);
            w
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            let open = workspace.read(cx).open.as_ref().unwrap();
            assert_eq!(open.tabs.len(), 1);
            assert_eq!(open.editor().unwrap().read(cx).path().as_str(), "b.md");
            open.search.read(cx).close_blocking();
        });
    }

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
            assert_eq!(open.editor().unwrap().read(cx).path().as_str(), "mine.md");
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

    /// 保存失败的那页不会被丢掉：打开别的笔记另开一页，它原样留着；
    /// 重命名、关闭 / 切换 vault、退出都要求每页落盘成功，所以被否决。
    #[gpui_kit::test]
    fn failed_save_keeps_its_tab_and_blocks_rename_and_vault_switching(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("base\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
        });
        let (editor, vault) = cx.update(|_, cx| {
            let session = workspace.read(cx).open.as_ref().unwrap();
            (session.editor().cloned().unwrap(), session.vault.clone())
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
        workspace.update_in(cx, |w, window, cx| w.open_note(VaultPath::new("b.md").unwrap(), window, cx));
        cx.run_until_parked();
        cx.update(|window, cx| {
            workspace.update(cx, |w, cx| {
                let open = w.open.as_ref().unwrap();
                assert_eq!(open.tabs.len(), 2);
                assert_eq!(open.tabs[0].editor.as_ref(), Some(&editor), "保存失败的那页还在");
                assert_eq!(open.editor().unwrap().read(cx).path().as_str(), "b.md");
                w.rename_current("should-not-rename".into(), window, cx);
                assert_eq!(w.open.as_ref().unwrap().editor().unwrap().read(cx).path().as_str(), "b.md");
                assert_eq!(editor.read(cx).path(), &long_path);
                assert!(!w.rename_pending, "failed save must prevent rename preparation");
                w.on_close_vault(&crate::actions::CloseVault, window, cx);
                assert!(w.open.is_some());
                w.open_vault(other_vault.path(), window, cx);
                assert_eq!(w.open.as_ref().unwrap().vault.root(), vault.root());
                w.on_quit(&crate::actions::Quit, window, cx);
                assert!(!w.session_closing && w.pending_operation.is_none(), "close/quit must be vetoed");
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

    fn catalog_paths(cx: &mut VisualTestContext, search: &Entity<SearchService>) -> Vec<String> {
        let result = std::rc::Rc::new(std::cell::RefCell::new(None));
        let sink = result.clone();
        cx.update(|_, cx| {
            let task = search.read(cx).note_summaries(cx);
            cx.spawn(async move |_| {
                let mut paths: Vec<_> = task.await.into_iter().map(|note| note.path.to_string()).collect();
                paths.sort();
                *sink.borrow_mut() = Some(paths);
            })
            .detach();
        });
        cx.run_until_parked();
        result.borrow_mut().take().expect("catalog task finished")
    }

    fn check_export_paths_touched(cx: &mut TestAppContext, png: bool) {
        let (dir, _data, workspace) = open(cx, Some("anchor\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let (editor, search) = cx.update(|_, cx| {
            let open = workspace.read(cx).open.as_ref().unwrap();
            (open.editor().cloned().unwrap(), open.search.clone())
        });
        wait_until(cx, |cx| cx.update(|_, cx| search.read(cx).phase() == crate::search::Phase::Idle));
        let bytes = cx.update(|_, cx| {
            crate::render::export_diagram(
                "graph LR\n A[exportneedle] --> B[Done]",
                false,
                "Helvetica",
                cx.text_system().clone(),
                png,
            )
            .unwrap()
        });
        assert_eq!(std::str::from_utf8(&bytes).is_ok(), !png);
        let path = VaultPath::new(if png { "a.assets/diagram.png" } else { "a.assets/diagram.svg" }).unwrap();
        std::fs::create_dir_all(dir.path().join("a.assets")).unwrap();
        std::fs::write(dir.path().join(path.as_str()), bytes).unwrap();

        // Use the real editor subscription, without a watcher event or a direct handler call.
        editor.update(cx, |_, cx| cx.emit(crate::editor::EditorEvent::PathsTouched(vec![path])));
        cx.run_until_parked();
        assert_eq!(catalog_paths(cx, &search), ["a.md"], "UTF-8 SVG is still an attachment, not a note");
        cx.update(|_, cx| {
            // Failed PNG reads count as writes too: an unchanged catalog alone would miss the bug.
            assert_eq!(search.read(cx).writes_since_optimize(), 0, "exports must not attempt text indexing");
            let w = workspace.read(cx);
            let open = w.open.as_ref().unwrap();
            assert!(open.recent.is_empty(), "export does not change recent notes");
            assert_eq!(open.tree.read(cx).selected(), None, "export does not reselect the note");
            assert!(w.state.vaults.is_empty(), "export does not persist tab state");
            assert_eq!(open.tabs.len(), 1);
            assert_eq!(open.editor(), Some(&editor));
        });
        assert!(hit_paths(cx, &search, "exportneedle").is_empty());
        assert_eq!(hit_paths(cx, &search, "anchor"), ["a.md"]);
        cx.update(|_, cx| search.read(cx).close_blocking());
    }

    #[gpui_kit::test]
    fn paths_touched_png_export_does_not_index_attachment(cx: &mut TestAppContext) {
        check_export_paths_touched(cx, true);
    }

    #[gpui_kit::test]
    fn paths_touched_svg_export_does_not_index_attachment(cx: &mut TestAppContext) {
        check_export_paths_touched(cx, false);
    }

    #[gpui_kit::test]
    fn paths_touched_mixed_event_indexes_only_notes(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("anchor\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let (editor, search, tree) = cx.update(|_, cx| {
            let open = workspace.read(cx).open.as_ref().unwrap();
            (open.editor().cloned().unwrap(), open.search.clone(), open.tree.clone())
        });
        wait_until(cx, |cx| cx.update(|_, cx| search.read(cx).phase() == crate::search::Phase::Idle));
        std::fs::create_dir_all(dir.path().join("a.assets")).unwrap();
        image::RgbaImage::new(2, 2).save(dir.path().join("a.assets/diagram.png")).unwrap();
        std::fs::write(dir.path().join("a.assets/diagram.svg"), "<svg>exportneedle</svg>").unwrap();
        std::fs::write(dir.path().join("a.assets/source.md"), "exportneedle\n").unwrap();
        std::fs::write(dir.path().join("copy.md"), "mixedanchor\n").unwrap();
        let copy = VaultPath::new("copy.md").unwrap();
        editor.update(cx, |e, cx| {
            e.set_path(copy.clone(), cx);
            cx.emit(crate::editor::EditorEvent::PathsTouched(vec![
                VaultPath::new("a.assets/diagram.png").unwrap(),
                copy.clone(),
                VaultPath::new("a.assets/diagram.svg").unwrap(),
                VaultPath::new("a.assets/source.md").unwrap(),
            ]));
        });
        cx.run_until_parked();
        assert_eq!(catalog_paths(cx, &search), ["a.md", "copy.md"]);
        assert_eq!(hit_paths(cx, &search, "mixedanchor"), ["copy.md"]);
        assert!(hit_paths(cx, &search, "exportneedle").is_empty());
        cx.update(|_, cx| {
            assert!(tree.read(cx).contains(&copy));
            assert_eq!(tree.read(cx).selected(), Some(&copy));
            let w = workspace.read(cx);
            let open = w.open.as_ref().unwrap();
            assert_eq!(open.recent.first(), Some(&copy));
            assert_eq!(w.state.vaults[open.vault.id()].tabs, ["copy.md"]);
            search.read(cx).close_blocking();
        });
    }

    #[gpui_kit::test]
    fn paths_touched_exports_refresh_tree_references_and_render_cache(cx: &mut TestAppContext) {
        use crate::render::{Attachment, State};

        let (dir, _data, workspace) = open(cx, Some("anchor\n\n![[used.png]]\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let (editor, search, images) = cx.update(|_, cx| {
            let open = workspace.read(cx).open.as_ref().unwrap();
            (open.editor().cloned().unwrap(), open.search.clone(), open.images.clone())
        });
        wait_until(cx, |cx| cx.update(|_, cx| search.read(cx).phase() == crate::search::Phase::Idle));
        let note = VaultPath::new("a.md").unwrap();
        let used = VaultPath::new("a.assets/used.png").unwrap();
        let orphan = VaultPath::new("a.assets/orphan.svg").unwrap();
        std::fs::create_dir_all(dir.path().join("a.assets")).unwrap();
        image::RgbaImage::new(8, 4).save(dir.path().join(used.as_str())).unwrap();
        wait_until(cx, |cx| {
            cx.update(|_, cx| {
                images.update(cx, |s, cx| {
                    let state = s.image(used.clone(), 100, 1., cx);
                    matches!(state, State::Ready { pixels, .. } if pixels.width == 8)
                })
            })
        });
        wait_until(cx, |cx| {
            cx.update(|_, cx| images.update(cx, |s, cx| s.attachment("orphan.svg", &note, cx)))
                == Attachment::Missing
        });
        // Cache a missing render as well as an existing image before export writes the files.
        wait_until(cx, |cx| {
            cx.update(|_, cx| {
                images.update(cx, |s, cx| matches!(s.image(orphan.clone(), 100, 1., cx), State::Failed(_)))
            })
        });
        image::RgbaImage::new(16, 6).save(dir.path().join(used.as_str())).unwrap();
        std::fs::write(
            dir.path().join(orphan.as_str()),
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="12" height="6"><rect width="12" height="6"/></svg>"#,
        )
        .unwrap();
        editor.update(cx, |_, cx| {
            cx.emit(crate::editor::EditorEvent::PathsTouched(vec![used.clone(), orphan.clone()]));
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                images.update(cx, |s, cx| s.attachment("orphan.svg", &note, cx)),
                Attachment::Found(orphan.clone()),
                "export must refresh the cached attachment catalog"
            );
        });
        wait_until(cx, |cx| {
            cx.update(|_, cx| {
                images.update(cx, |s, cx| {
                    let used_state = s.image(used.clone(), 100, 1., cx);
                    let orphan_state = s.image(orphan.clone(), 100, 1., cx);
                    matches!(used_state, State::Ready { pixels, .. } if pixels.width == 16)
                        && matches!(orphan_state, State::Ready { .. })
                })
            })
        });
        wait_until(cx, |cx| cx.update(|_, cx| workspace.read(cx).references_task.is_none()));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let clip = cx.debug_bounds("file-tree-clip:a.md").expect("export adds the attachment count badge");
        cx.simulate_click(clip.center(), gpui_kit::Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        for selector in ["file-tree-attachment:a.assets/used.png", "file-tree-attachment:a.assets/orphan.svg"]
        {
            assert!(cx.debug_bounds(selector).is_some(), "{selector} appears in tree");
        }
        assert!(cx.debug_bounds("file-tree-folder:a.assets").is_none());
        assert!(
            cx.debug_bounds("file-tree-orphan:a.assets/orphan.svg").is_some(),
            "new export has no references"
        );
        assert!(
            cx.debug_bounds("file-tree-orphan:a.assets/used.png").is_none(),
            "existing embed remains referenced"
        );
        cx.update(|_, cx| search.read(cx).close_blocking());
    }

    /// 文件监听的一批变化：文件树重扫、打开中的那篇立刻重载 / 进入已删除态、其余按指纹对齐索引。
    #[gpui_kit::test]
    fn vault_changes_refresh_tree_editor_and_index(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("alpha anchor\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let (tree, editor, search) = cx.update(|_, cx| {
            let open = workspace.read(cx).open.as_ref().unwrap();
            (open.tree.clone(), open.editor().cloned().unwrap(), open.search.clone())
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
        // 文件树在后台重扫
        cx.run_until_parked();
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
        cx.update(|_, cx| assert!(workspace.read(cx).open.as_ref().unwrap().editor().is_none()));
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
                ["settings", "about", "check-updates", "open-vault", "new-vault", "quit"]
            );
            assert!(
                commands.iter().find(|command| command.id == "open-vault").unwrap().shortcut.contains('O')
            );
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
            let editor = workspace.read(cx).open.as_ref().unwrap().editor().cloned().unwrap();
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
            workspace.open_hit(VaultPath::new("a.md").unwrap(), 0, true, window, cx);
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
            let editor = workspace.read(cx).open.as_ref().unwrap().editor().cloned().unwrap();
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
        cx.update(|_, cx| assert_eq!(editor.read(cx).index_snapshot().1, "# XHeading\n"));
        cx.update(|_, cx| workspace.read(cx).open.as_ref().unwrap().search.read(cx).close_blocking());
    }

    #[gpui_kit::test]
    fn command_palette_restores_caret_and_executes_editor_action(cx: &mut TestAppContext) {
        let (_dir, _data, workspace) = open(cx, Some("# Heading\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.run_until_parked();
        let editor = cx.update(|window, cx| {
            let editor = workspace.read(cx).open.as_ref().unwrap().editor().cloned().unwrap();
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
        cx.update(|_, cx| assert_eq!(editor.read(cx).index_snapshot().1, "# XHeading!\n"));
        cx.simulate_keystrokes("cmd-shift-p");
        cx.run_until_parked();
        cx.simulate_input("heading 2");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
            assert!(workspace.read(cx).quick_open.is_none());
            assert_eq!(editor.read(cx).index_snapshot().1, "## XHeading!\n");
            assert!(editor.read(cx).command_available(&crate::actions::ConvertBlock(1)));
        });
        cx.simulate_keystrokes("cmd-shift-p");
        cx.run_until_parked();
        cx.simulate_input("undo");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
            assert_eq!(editor.read(cx).index_snapshot().1, "# XHeading!\n");
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
            assert_eq!(editor.read(cx).index_snapshot().1, "## XHeading!\n");
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
                "# XHeading!\n",
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
            open.editor().cloned().unwrap()
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
            open.editor().cloned().unwrap()
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
                let e = open.editor().unwrap().read(cx);
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
            (open.editor().cloned().unwrap(), open.search.clone(), open.backlinks.clone())
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
                let e = open.editor().unwrap().read(cx);
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
            let editor = open.editor().cloned().unwrap();
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
            let editor = open.editor().cloned().unwrap();
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
            let editor = open.editor().cloned().unwrap();
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
            open.editor().cloned().unwrap()
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
            open.editor().cloned().unwrap()
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
            (open.search.clone(), open.backlinks.clone(), open.editor().cloned().unwrap(), open.tree.clone())
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
    fn rename_submission_returns_before_disk_change_and_blocks_hidden_input(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("# base\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
        });
        let editor = cx.update(|_, cx| workspace.read(cx).open.as_ref().unwrap().editor().unwrap().clone());
        editor.update_in(cx, |e, window, cx| e.focus_block(e.headings()[0].2, window, cx));
        workspace.update_in(cx, |w, window, cx| {
            let open = w.open.as_ref().unwrap();
            let old = VaultPath::new("a.md").unwrap();
            let target = VaultPath::new("new.md").unwrap();
            let plan =
                aero_core::rename::RenamePlan::prepare(&open.vault, old.clone(), target.clone()).unwrap();
            w.execute_change_name(plan, old, target, false, "test rename".into(), window, cx);
            assert!(w.session_busy());
            assert!(w.open.is_some(), "session ownership remains until completion");
            assert!(w.focus_handle.is_focused(window));
            assert!(dir.path().join("a.md").exists(), "submission must not execute disk changes inline");
            assert!(!dir.path().join("new.md").exists());
            w.on_new_tab(&NewTab, window, cx);
            w.close_editor(&editor, window, cx);
            assert_eq!(w.open.as_ref().unwrap().tabs.len(), 1);
        });
        cx.simulate_input("must not edit hidden text");
        wait_until(cx, |cx| cx.update(|_, cx| !workspace.read(cx).session_busy()));
        assert!(!dir.path().join("a.md").exists());
        assert_eq!(std::fs::read_to_string(dir.path().join("new.md")).unwrap(), "# base\n");
        cx.update(|_, cx| assert_eq!(editor.read(cx).index_snapshot().1, "# base\n"));
    }

    #[gpui_kit::test]
    fn rename_failure_resumes_editors_without_resurrecting_old_paths(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("base\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
        });
        let editor = cx.update(|_, cx| workspace.read(cx).open.as_ref().unwrap().editor().unwrap().clone());
        workspace.update_in(cx, |w, window, cx| {
            let vault = w.open.as_ref().unwrap().vault.clone();
            let old = VaultPath::new("a.md").unwrap();
            let target = VaultPath::new("new.md").unwrap();
            let plan = aero_core::rename::RenamePlan::prepare(&vault, old.clone(), target.clone()).unwrap();
            w.execute_change_name(plan, old.clone(), target.clone(), false, "test rename".into(), window, cx);
            // An external move makes execute fail; watcher/restore/save must not recreate the source.
            vault.rename(&old, &target).unwrap();
            editor.update(cx, |e, cx| {
                e.check_external(cx);
                e.restore_deleted(cx);
                e.save_now(cx);
            });
            assert!(!dir.path().join("a.md").exists());
        });
        wait_until(cx, |cx| cx.update(|_, cx| !workspace.read(cx).session_busy()));
        editor.update_in(cx, |e, window, cx| {
            assert_eq!(e.path().as_str(), "new.md");
            e.start_writing(window, cx);
        });
        cx.simulate_input("after failure");
        editor.update(cx, |e, cx| e.save_now(cx));
        wait_until(cx, |cx| cx.update(|_, cx| matches!(editor.read(cx).save_state(), SaveState::Saved(_))));
        assert!(!dir.path().join("a.md").exists());
        assert!(std::fs::read_to_string(dir.path().join("new.md")).unwrap().contains("after failure"));
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
            let editor = w.open.as_ref().unwrap().editor().cloned().unwrap();
            editor.update(cx, |e, cx| e.start_writing(window, cx));
        });
        cx.simulate_input("继续编辑");
        wait_until(cx, |cx| cx.update(|_, cx| !workspace.read(cx).rename_pending));
        assert!(dir.path().join("a.md").exists());
        assert!(!dir.path().join("new.md").exists());
        cx.update(|_, cx| {
            let e = workspace.read(cx).open.as_ref().unwrap().editor().unwrap().read(cx);
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
            (open.editor().cloned().unwrap(), open.search.clone())
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
    fn tab_menu_renames_the_file_without_showing_it_as_a_heading(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("# 安全\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("editor-title").is_none(), "正文从一级标题开始，不另画文件名");
        cx.update(|window, cx| {
            window.dispatch_action(Box::new(crate::actions::RenameActiveTab), cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("tab-rename").is_some());
        cx.simulate_input("笔记");
        cx.simulate_keystrokes("enter");
        wait_until(cx, |cx| cx.update(|_, cx| !workspace.read(cx).rename_pending));
        assert!(dir.path().join("笔记.md").is_file());
        assert!(!dir.path().join("a.md").exists());
        assert_eq!(std::fs::read_to_string(dir.path().join("笔记.md")).unwrap(), "# 安全\n");
        cx.update(|_, cx| {
            let editor = workspace.read(cx).open.as_ref().unwrap().editor().unwrap().read(cx);
            assert_eq!(editor.title(), "笔记");
            workspace.read(cx).open.as_ref().unwrap().search.read(cx).close_blocking();
        });
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
        assert!(cx.debug_bounds("editor-title").is_none(), "正文不显示文件名或 front matter 标题");
        cx.update(|window, cx| {
            window.dispatch_action(Box::new(crate::actions::RetitleActiveTab), cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("tab-rename").is_some(), "修改标题在标签页上输入");
        cx.simulate_input("New title");
        cx.simulate_keystrokes("enter");
        wait_until(cx, |cx| cx.update(|_, cx| !workspace.read(cx).rename_pending));
        cx.update(|_, cx| {
            let open = workspace.read(cx).open.as_ref().unwrap();
            let editor = open.editor().unwrap().read(cx);
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
        assert!(cx.debug_bounds("backlink-count").is_none());
        workspace
            .update_in(cx, |w, window, cx| w.on_show_backlinks(&crate::actions::ShowBacklinks, window, cx));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|_, cx| {
            assert!(!workspace.read(cx).panel_visible, "references no longer open a side panel")
        });
        let footer = cx.debug_bounds("document-references").unwrap();
        assert!(footer.top() < cx.debug_bounds("editor").unwrap().bottom());
        workspace.update_in(cx, |w, window, cx| w.rename_current("renamed".into(), window, cx));
        wait_until(cx, |cx| {
            cx.update(|_, cx| {
                let editor = workspace.read(cx).open.as_ref().unwrap().editor().unwrap();
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
            let editor = workspace.read(cx).open.as_ref().unwrap().editor().unwrap();
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
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("backlink-0").is_some(), "反链列表显示来源文档");
        assert!(cx.debug_bounds("backlink-rich-0").is_none(), "列表不再展开正文摘要");

        // 从真实面板行点击到来源标题块。
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let row = cx.debug_bounds("backlink-0").expect("反链行已渲染");
        cx.simulate_click(row.center(), gpui_kit::Modifiers::default());
        cx.run_until_parked();
        cx.update(|_, cx| {
            let editor = workspace.read(cx).open.as_ref().unwrap().editor().unwrap().read(cx);
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
            let editor = workspace.read(cx).open.as_ref().unwrap().editor().unwrap().read(cx);
            assert_eq!(editor.path(), &b);
            assert!(editor.highlighted_block().is_none(), "stale position must not highlight another block");
        });

        // 连续切换及关闭不能被在途查询的旧结果覆盖：两页都关掉后停在空白页，反链面板是空的。
        workspace.update_in(cx, |w, window, cx| {
            w.open_note(VaultPath::new("a.md").unwrap(), window, cx);
            w.open_note(b, window, cx);
            for _ in 0..2 {
                let editor = w.open.as_ref().unwrap().editor().cloned().unwrap();
                w.close_editor(&editor, window, cx);
            }
            assert!(w.open.as_ref().unwrap().editor().is_none());
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
        let source = cx.update(|_, cx| workspace.read(cx).open.as_ref().unwrap().editor().cloned().unwrap());

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
                let Some(editor) = open.editor() else { return false };
                let e = editor.read(cx);
                e.path().as_str() == "b.md" && e.highlighted_block().is_some()
            })
        });
        cx.update(|_, cx| {
            assert!(!source.read(cx).is_editing(), "点链接不该把来源块切进编辑态");
            let e = workspace.read(cx).open.as_ref().unwrap().editor().unwrap().read(cx);
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
        let editor = cx.update(|_, cx| workspace.read(cx).open.as_ref().unwrap().editor().cloned().unwrap());
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
            assert_eq!(open.editor().unwrap().read(cx).path().as_str(), "a.md");
        });
    }

    /// 未解析链接点击不创建笔记，编辑器留在原地。
    #[gpui_kit::test]
    fn wikilink_click_does_not_create_a_missing_note(cx: &mut TestAppContext) {
        let (dir, _data, workspace) = open(cx, Some("见 [[新笔记]]\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        cx.update(|window, cx| {
            window
                .replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
        });
        let editor = cx.update(|_, cx| workspace.read(cx).open.as_ref().unwrap().editor().cloned().unwrap());
        cx.update(|_, cx| {
            editor.update(cx, |_, cx| {
                cx.emit(crate::editor::EditorEvent::LinkClicked {
                    target: "新笔记".into(), fragment: None
                })
            });
        });
        for _ in 0..5 {
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!dir.path().join("新笔记.md").exists());
        cx.update(|_, cx| {
            let open = workspace.read(cx).open.as_ref().unwrap();
            assert_eq!(open.editor().unwrap().read(cx).path().as_str(), "a.md");
        });
    }

    #[gpui_kit::test]
    fn dropping_a_tree_note_onto_the_editor_inserts_a_link(cx: &mut TestAppContext) {
        use gpui_kit::{Modifiers, MouseButton, point, px};
        let (_dir, _data, workspace) = open(cx, Some("正文\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        let body = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| {
                workspace.read(cx).open.as_ref().unwrap().editor().unwrap().read(cx).index_snapshot().1
            })
        };
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let from = cx.debug_bounds("file-tree-note:a.md").unwrap().center();
        let onto = cx.debug_bounds("editor").unwrap().center();
        let drag = |cx: &mut VisualTestContext, modifiers: Modifiers| {
            cx.simulate_mouse_move(from, None, Modifiers::default());
            cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
            cx.simulate_mouse_move(from + point(px(16.), px(0.)), MouseButton::Left, Modifiers::default());
            cx.simulate_mouse_move(onto, MouseButton::Left, modifiers);
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.simulate_mouse_up(onto, MouseButton::Left, modifiers);
            cx.run_until_parked();
        };
        drag(cx, Modifiers::default());
        let text = body(cx);
        assert!(text.contains("[[a]]"), "{text}");
        assert!(!text.contains("![[a]]"), "{text}");
        let mut alt = Modifiers::default();
        alt.alt = true;
        drag(cx, alt);
        let text = body(cx);
        assert!(text.contains("![[a]]"), "{text}");
    }

    #[gpui_kit::test]
    fn link_preview_setting_is_saved_and_restored(cx: &mut TestAppContext) {
        use crate::paths::LinkPreviewMode;
        let (_dir, data, workspace) = open(cx, Some("body\n"));
        let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
        workspace.update_in(cx, |w, window, cx| {
            w.on_link_preview_command(&crate::actions::LinkPreviewCommand, window, cx)
        });
        assert_eq!(PersistedState::load(data.path()).link_preview, LinkPreviewMode::Command);
    }
}
