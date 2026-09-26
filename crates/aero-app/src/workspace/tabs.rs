//! 编辑区的标签页（`ui_design.md` 2.3）：每篇文档一个标签页。
//!
//! 打开一篇笔记时，已经在某个标签页里就切过去；当前是空白的「新标签页」或搜索预览页就在原地打开；
//! 否则在当前标签右边新开一个。搜索里 ↑↓ 预览的笔记占一个预览页（标题斜体），继续 ↑↓ 在同一页里换，
//! 编辑、⏎ 或双击标签后它变成普通标签页。
//! 没显示的标签页保留自己的编辑器：撤销历史、滚动位置、自动保存和外部修改检查都照常。
//! 恢复会话时只记路径，切到那一页才加载。

use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex};

/// 标签页标题处的输入：重命名文件，或改 front matter 的 `title`。
pub(super) struct TabNameEdit {
    tab: u64,
    retitle: bool,
    input: Entity<InputState>,
    _subscription: Subscription,
}

use super::*;

pub(super) struct Tab {
    pub(super) id: u64,
    /// 编辑器还没加载时（恢复会话）的路径；加载后以编辑器的路径为准
    path: Option<VaultPath>,
    pub(super) editor: Option<Entity<Editor>>,
    _subscription: Option<Subscription>,
    pub(super) preview: bool,
    loading: bool,
    load_generation: u64,
    closing: bool,
    navigation: Option<Navigation>,
}

/// Stored on the stable tab, not the active editor: loads may finish out of order.
pub(super) enum Navigation {
    Block(u32),
    Fragment(String),
    Backlink(aero_index::Backlink),
    Write,
}

impl Tab {
    fn empty(id: u64) -> Self {
        Tab {
            id,
            path: None,
            editor: None,
            _subscription: None,
            preview: false,
            loading: false,
            load_generation: 0,
            closing: false,
            navigation: None,
        }
    }

    pub(super) fn path<'a>(&'a self, cx: &'a App) -> Option<&'a VaultPath> {
        match &self.editor {
            Some(editor) => Some(editor.read(cx).path()),
            None => self.path.as_ref(),
        }
    }

    /// 重命名 / 移动之后：还没加载的标签页也跟着换路径。
    pub(super) fn retarget(&mut self, map: &impl Fn(&VaultPath) -> VaultPath) {
        if let Some(path) = &self.path {
            self.path = Some(map(path));
        }
        // A same-path title/link rewrite invalidates old bytes too.
        self.load_generation += 1;
        self.loading = false;
    }

    fn title(&self, cx: &App) -> String {
        match (&self.editor, &self.path) {
            (Some(editor), _) => editor.read(cx).title(),
            (None, Some(path)) => path.stem().to_owned(),
            (None, None) => "新标签页".to_owned(),
        }
    }
}

#[cfg(test)]
impl Tab {
    /// 测试直接拼会话时用：一页已经加载好的编辑器。
    pub(super) fn loaded(id: u64, editor: Entity<Editor>) -> Self {
        Tab { editor: Some(editor), ..Self::empty(id) }
    }

    pub(super) fn subscribe(&mut self, subscription: Subscription) {
        self._subscription = Some(subscription);
    }
}

/// 打开笔记放在哪个标签页。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenIn {
    /// 默认规则：已打开就切过去；当前是空白页或预览页就原地打开；否则右边新开
    Auto,
    /// ⌘ 点击 / 中键：不占用当前的预览页
    NewTab,
    /// 搜索里 ↑↓：放进预览页
    Preview,
}

/// 标签条里拖动的标签页
#[derive(Clone)]
struct TabDrag {
    id: u64,
    title: String,
}

impl Render for TabDrag {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_1()
            .rounded_md()
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .text_sm()
            .child(self.title.clone())
    }
}

impl VaultSession {
    /// 当前标签页的编辑器。
    pub(super) fn editor(&self) -> Option<&Entity<Editor>> {
        self.tabs.get(self.active).and_then(|tab| tab.editor.as_ref())
    }

    /// 所有已加载的编辑器（包括没显示的标签页）。
    pub(super) fn editors(&self) -> impl Iterator<Item = &Entity<Editor>> {
        self.tabs.iter().filter_map(|tab| tab.editor.as_ref())
    }

    pub(super) fn tab_of(&self, path: &VaultPath, cx: &App) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.path(cx) == Some(path))
    }

    pub(super) fn tab_of_editor(&self, editor: &Entity<Editor>) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.editor.as_ref() == Some(editor))
    }

    /// 标签页里打开着的笔记。
    pub(super) fn opened(&self, cx: &App) -> std::collections::HashSet<VaultPath> {
        self.tabs.iter().filter_map(|tab| tab.path(cx).cloned()).collect()
    }
}

impl Workspace {
    fn next_tab_id(&mut self) -> u64 {
        self.next_tab_id += 1;
        self.next_tab_id
    }

    /// 新 vault 会话的第一页：空白页，随后由恢复会话或「最近修改的一篇」填上。
    pub(super) fn first_tabs(&mut self) -> Vec<Tab> {
        vec![Tab::empty(self.next_tab_id())]
    }

    /// 打开笔记（默认规则见模块注释）。打开失败时标签页保持原样。
    pub(super) fn open_note_in(
        &mut self,
        path: VaultPath,
        mode: OpenIn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session_busy() {
            return;
        }
        let Some(open) = self.open.as_ref() else { return };
        if let Some(ix) = open.tab_of(&path, cx) {
            if mode != OpenIn::Preview
                && let Some(open) = self.open.as_mut()
            {
                open.tabs[ix].preview = false;
            }
            self.activate_tab(ix, window, cx);
            return;
        }
        let current = &open.tabs[open.active];
        let empty = current.path.is_none() && current.editor.is_none();
        let mut replace = empty || (current.preview && mode != OpenIn::NewTab);
        // A preview with work in flight remains owned until its last save succeeds.
        let close_after_save = replace
            && current.editor.as_ref().is_some_and(|e| {
                let e = e.read(cx);
                e.has_pending_export() || !matches!(e.save_state(), SaveState::Clean | SaveState::Saved(_))
            });
        let old_id = current.id;
        if close_after_save {
            replace = false;
        }
        let id = self.next_tab_id();
        let Some(open) = self.open.as_mut() else { return };
        let tab = Tab { path: Some(path), preview: mode == OpenIn::Preview, ..Tab::empty(id) };
        let ix = if replace {
            open.tabs[open.active] = tab;
            open.active
        } else {
            open.tabs.insert(open.active + 1, tab);
            open.active + 1
        };
        self.activate_tab(ix, window, cx);
        if close_after_save {
            if let Some(ix) = self.open.as_ref().and_then(|o| o.tabs.iter().position(|t| t.id == old_id)) {
                self.close_tab(ix, window, cx);
            }
        }
    }

    /// Reading and parsing happen off-thread. Entity construction/subscriptions stay on GPUI.
    pub(super) fn load_editor(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else { return };
        let tab = &mut open.tabs[ix];
        if tab.loading || tab.editor.is_some() {
            return;
        }
        let Some(path) = tab.path.clone() else { return };
        tab.loading = true;
        tab.load_generation += 1;
        let generation = tab.load_generation;
        let id = tab.id;
        let vault = open.vault.clone();
        let source = vault.clone();
        let load_path = path.clone();
        let load = cx.background_spawn(async move { Editor::load(source, load_path) });
        cx.spawn_in(window, async move |this, cx| {
            let result = load.await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(open) = this.open.as_ref().filter(|o| Arc::ptr_eq(&o.vault, &vault)) else { return };
                let Some(ix) = open
                    .tabs
                    .iter()
                    .position(|t| t.id == id && t.load_generation == generation && t.path(cx) == Some(&path))
                else {
                    return;
                };
                if this.session_busy() {
                    return;
                }
                let (doc, stamp) = match result {
                    Ok(loaded) => loaded,
                    Err(err) => {
                        window.push_notification(Notification::error(format!("无法打开 {path}：{err}")), cx);
                        this.remove_tab(ix, window, cx);
                        return;
                    }
                };
                let (search, images) = (open.search.clone(), open.images.clone());
                let editor = cx.new(|cx| Editor::new(vault, path, doc, stamp, cx));
                editor.update(cx, |e, cx| {
                    e.connect_link_preview(search, cx);
                    e.connect_images(images, cx);
                });
                let subscription = this.subscribe_editor(&editor, window, cx);
                let open = this.open.as_mut().unwrap();
                open.tabs[ix].loading = false;
                open.tabs[ix].editor = Some(editor);
                open.tabs[ix]._subscription = Some(subscription);
                if open.active == ix {
                    let focus = window.focused(cx).as_ref() == Some(&this.focus_handle);
                    this.refresh_active_tab(focus, window, cx);
                }
                this.apply_navigation(ix, window, cx);
                this.tabs_changed(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn navigate_note(
        &mut self,
        path: &VaultPath,
        navigation: Navigation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.open.as_mut() else { return };
        let Some(ix) = open.tab_of(path, cx) else { return };
        open.tabs[ix].navigation = Some(navigation);
        self.apply_navigation(ix, window, cx);
    }

    fn apply_navigation(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else { return };
        let tab = &mut open.tabs[ix];
        let Some(editor) = tab.editor.clone() else { return };
        if matches!(tab.navigation, Some(Navigation::Write)) && ix != open.active {
            return;
        }
        let Some(navigation) = tab.navigation.take() else { return };
        let error = editor.update(cx, |e, cx| match navigation {
            Navigation::Block(ord) => {
                e.reveal_indexed_block(ord, cx);
                None
            }
            Navigation::Fragment(fragment) => e.reveal_fragment(&fragment, cx).err(),
            Navigation::Backlink(hit) => (!e.reveal_backlink(&hit, cx))
                .then(|| "来源笔记已变化，已打开笔记；反链将在索引更新后刷新。".into()),
            Navigation::Write => {
                e.start_writing(window, cx);
                None
            }
        });
        if let Some(error) = error {
            window.push_notification(Notification::info(error).title("Aero"), cx);
        }
    }

    /// 切到第 `ix` 个标签页：按需加载编辑器，文件树、反链、菜单跟上，焦点给编辑器。
    pub(super) fn activate_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_busy() {
            return;
        }
        let Some(open) = self.open.as_mut() else { return };
        let ix = ix.min(open.tabs.len() - 1);
        let leaving = open.editor().cloned().filter(|_| open.active != ix);
        open.active = ix;
        // Save events index the exact completed write. Switching never waits on disk.
        if let Some(editor) = leaving {
            editor.update(cx, |e, cx| e.save_now(cx));
        }
        if let Some(editor) = self.open.as_ref().and_then(|o| o.editor().cloned()) {
            editor.update(cx, |e, cx| e.check_external(cx));
        }
        self.load_editor(ix, window, cx);
        self.refresh_active_tab(true, window, cx);
        self.apply_navigation(ix, window, cx);
    }

    pub(super) fn refresh_active_tab(&mut self, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else { return };
        let ix = open.active;
        let editor = open.editor().cloned();
        let path = open.tabs[ix].path(cx).cloned();
        if let Some(path) = &path {
            open.recent.retain(|p| p != path);
            open.recent.insert(0, path.clone());
            open.recent.truncate(20);
        }
        let backlinks = open.backlinks.clone();
        backlinks.update(cx, |v, cx| v.set_note(path.clone(), cx));
        open.tree.update(cx, |t, cx| t.select(path, cx));
        match &editor {
            Some(editor) => editor.update(cx, |e, cx| {
                e.connect_backlinks(backlinks, cx);
                if focus {
                    e.focus_browse(window, cx);
                }
            }),
            None if focus => self.focus_handle.focus(window, cx),
            None => {}
        }
        self.tab_scroll.scroll_to_item(ix);
        self.tabs_changed(cx);
        self.sync_editor_menus(cx);
        cx.notify();
    }

    /// 标签页增减、切换或换了内容：文件树的「已打开」标记与会话记录跟上。
    pub(super) fn tabs_changed(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let opened = open.opened(cx);
        open.tree.update(cx, |t, cx| t.set_opened(opened, cx));
        let tabs: Vec<String> = open
            .tabs
            .iter()
            .map(|tab| tab.path(cx).map(|p| p.as_str().to_owned()).unwrap_or_default())
            .collect();
        let active = open.active;
        let ui = self.state.vaults.entry(open.vault.id().to_owned()).or_default();
        if ui.tabs != tabs || ui.active_tab != active {
            ui.tabs = tabs;
            ui.active_tab = active;
            self.state.save(&self.data_dir);
        }
    }

    /// 打开 vault 时恢复上次的标签页（只记路径，切到才加载）；没有记录就打开最近修改的一篇。
    pub(super) fn restore_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let saved = self.state.vaults.get(open.vault.id()).cloned().unwrap_or_default();
        let vault = open.vault.clone();
        let mut tabs = Vec::new();
        let mut active = 0;
        for (ix, rel) in saved.tabs.iter().enumerate() {
            let path = match rel.as_str() {
                "" => None,
                rel => match VaultPath::new(rel) {
                    Ok(path) if vault.exists(&path) => Some(path),
                    _ => continue,
                },
            };
            if ix <= saved.active_tab {
                active = tabs.len();
            }
            let mut tab = Tab::empty(self.next_tab_id());
            tab.path = path;
            tabs.push(tab);
        }
        if tabs.is_empty() {
            if let Some(path) = most_recent_note(&vault) {
                self.open_note(path, window, cx);
            } else {
                self.tabs_changed(cx);
            }
            return;
        }
        let Some(open) = self.open.as_mut() else { return };
        open.tabs = tabs;
        self.activate_tab(active, window, cx);
    }

    /// 关闭一页前先落盘；落不了盘就不关，免得丢掉内容。
    pub(super) fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.session_busy() {
            return false;
        }
        let Some(open) = self.open.as_mut() else { return false };
        let Some(tab) = open.tabs.get_mut(ix) else { return false };
        tab.closing = true;
        self.finish_close_tab(ix, window, cx)
    }

    pub(super) fn continue_tab_closes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_busy() {
            return;
        }
        let ids: Vec<_> =
            self.open.iter().flat_map(|o| o.tabs.iter()).filter(|t| t.closing).map(|t| t.id).collect();
        for id in ids {
            if let Some(ix) = self.open.as_ref().and_then(|o| o.tabs.iter().position(|t| t.id == id)) {
                self.finish_close_tab(ix, window, cx);
            }
        }
    }

    fn finish_close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(open) = self.open.as_mut() else { return false };
        if let Some(editor) = open.tabs[ix].editor.clone() {
            let e = editor.read(cx);
            if e.has_pending_export() || e.is_deleted() || matches!(e.save_state(), SaveState::Failed(_)) {
                open.tabs[ix].closing = false;
                window.push_notification(
                    Notification::warning("笔记尚未安全保存，请处理保存错误或导出后重试。").title("Aero"),
                    cx,
                );
                return false;
            }
            match e.save_state() {
                SaveState::Dirty => {
                    editor.update(cx, |e, cx| e.save_now(cx));
                    return false;
                }
                SaveState::Saving => return false,
                _ => {}
            }
            self.index_editor(&editor, cx);
        }
        let Some(open) = self.open.as_mut() else { return false };
        if let Some(path) = open.tabs[ix].path(cx).cloned() {
            open.closed.retain(|p| p != &path);
            open.closed.push(path);
            let excess = open.closed.len().saturating_sub(20);
            open.closed.drain(..excess);
        }
        self.remove_tab(ix, window, cx);
        true
    }

    /// 直接拿掉一页，不落盘（文件已删除 / 刚移进废纸篓时正是为了不把它写回来）。
    /// 关掉最后一页时换成空白页。
    pub(super) fn remove_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.next_tab_id();
        let Some(open) = self.open.as_mut() else { return };
        if ix >= open.tabs.len() {
            return;
        }
        let keep_focus = (ix != open.active).then(|| window.focused(cx)).flatten();
        open.tabs.remove(ix);
        if open.tabs.is_empty() {
            open.tabs.push(Tab::empty(id));
        }
        // 关掉当前页时停在它右边那页（没有就左边）；关别的页不换当前页
        let active = if ix < open.active || open.active >= open.tabs.len() {
            open.active.saturating_sub(1)
        } else {
            open.active
        };
        let active = active.min(open.tabs.len() - 1);
        self.activate_tab(active, window, cx);
        if let Some(focus) = keep_focus {
            window.focus(&focus, cx);
        }
    }

    /// 某篇笔记不能再显示了（移进废纸篓、撤销新建）：拿掉显示它的标签页。返回拿掉的是第几页。
    pub(super) fn drop_tab_showing(
        &mut self,
        path: &VaultPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let ix = self.open.as_ref()?.tab_of(path, cx)?;
        self.remove_tab(ix, window, cx);
        Some(ix)
    }

    /// 编辑器横幅上的「关闭」：拿掉它所在的标签页，不落盘。
    pub(super) fn close_editor(
        &mut self,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self.open.as_ref().and_then(|open| open.tab_of_editor(editor)) {
            self.remove_tab(ix, window, cx);
        }
    }

    /// 编辑过的预览页转为普通标签页。
    pub(super) fn keep_preview_if_edited(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else { return };
        let Some(ix) = open.tab_of_editor(editor) else { return };
        if open.tabs[ix].preview && editor.read(cx).save_state() != &SaveState::Clean {
            open.tabs[ix].preview = false;
        }
    }

    pub(super) fn keep_active_preview(&mut self) {
        if let Some(open) = self.open.as_mut() {
            let active = open.active;
            open.tabs[active].preview = false;
        }
    }

    fn move_tab(&mut self, id: u64, before: u64, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else { return };
        let (Some(from), Some(to)) = (
            open.tabs.iter().position(|tab| tab.id == id),
            open.tabs.iter().position(|tab| tab.id == before),
        ) else {
            return;
        };
        if from == to {
            return;
        }
        let active_id = open.tabs[open.active].id;
        let tab = open.tabs.remove(from);
        let to = if from < to { to - 1 } else { to };
        open.tabs.insert(to, tab);
        open.active = open.tabs.iter().position(|tab| tab.id == active_id).unwrap_or(0);
        self.tabs_changed(cx);
        cx.notify();
    }

    // ---- 动作 ----

    pub(super) fn on_new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.session_busy() {
            return;
        }
        let id = self.next_tab_id();
        let Some(open) = self.open.as_mut() else { return };
        let ix = open.active + 1;
        open.tabs.insert(ix, Tab::empty(id));
        self.activate_tab(ix, window, cx);
    }

    pub(super) fn on_close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        // 只剩一页空白页时 ⌘W 什么也不做
        let tab = &open.tabs[open.active];
        if open.tabs.len() == 1 && tab.path.is_none() && tab.editor.is_none() {
            return;
        }
        self.close_tab(open.active, window, cx);
    }

    pub(super) fn on_rename_active_tab(
        &mut self,
        _: &RenameActiveTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_tab_name_edit(false, window, cx);
    }

    pub(super) fn on_retitle_active_tab(
        &mut self,
        _: &RetitleActiveTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_tab_name_edit(true, window, cx);
    }

    /// `retitle` 时改 front matter 标题，否则重命名文件。输入框画在标签页上。
    pub(super) fn begin_tab_name_edit(&mut self, retitle: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let tab = &open.tabs[open.active];
        let Some(editor) = tab.editor.clone() else { return };
        if retitle && !editor.read(cx).has_front_matter_title() {
            return;
        }
        let id = tab.id;
        let initial =
            if retitle { editor.read(cx).title() } else { editor.read(cx).path().stem().to_owned() };
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |state, cx| {
            state.set_value(initial.clone(), window, cx);
            state.set_selected_range(0..initial.len(), cx);
            state.focus(window, cx);
        });
        let subscription = cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
            InputEvent::PressEnter { .. } => this.commit_tab_name_edit(window, cx),
            InputEvent::Blur => {
                this.tab_name_edit = None;
                cx.notify();
            }
            _ => {}
        });
        self.tab_name_edit = Some(TabNameEdit { tab: id, retitle, input, _subscription: subscription });
        cx.notify();
    }

    fn commit_tab_name_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.tab_name_edit.take() else { return };
        let new = edit.input.read(cx).value().trim().to_owned();
        cx.notify();
        if new.is_empty() {
            return;
        }
        if edit.retitle {
            let current =
                self.open.as_ref().and_then(|open| open.editor()).map(|editor| editor.read(cx).title());
            if current.as_deref() != Some(new.as_str()) {
                self.retitle_current(new, window, cx);
            }
        } else {
            self.rename_current(new, window, cx);
        }
    }

    pub(super) fn on_close_other_tabs(
        &mut self,
        _: &CloseOtherTabs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.open.as_ref() else { return };
        let keep = open.tabs[open.active].id;
        let others: Vec<u64> = open.tabs.iter().map(|tab| tab.id).filter(|id| *id != keep).collect();
        for id in others {
            let Some(ix) = self.open.as_ref().and_then(|o| o.tabs.iter().position(|tab| tab.id == id)) else {
                continue;
            };
            self.close_tab(ix, window, cx);
        }
    }

    pub(super) fn on_reopen_closed_tab(
        &mut self,
        _: &ReopenClosedTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        while let Some(path) = self.open.as_mut().and_then(|open| open.closed.pop()) {
            if self.open.as_ref().is_some_and(|open| open.vault.exists(&path)) {
                self.open_note_in(path, OpenIn::NewTab, window, cx);
                return;
            }
        }
    }

    pub(super) fn on_next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.step_tab(1, window, cx);
    }

    pub(super) fn on_prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        self.step_tab(-1, window, cx);
    }

    fn step_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let n = open.tabs.len() as isize;
        if n > 1 {
            let ix = (open.active as isize + delta).rem_euclid(n) as usize;
            self.activate_tab(ix, window, cx);
        }
    }

    pub(super) fn on_activate_tab(
        &mut self,
        action: &ActivateTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.open.as_ref() else { return };
        let ix = action.0.min(open.tabs.len() - 1);
        if action.0 == usize::MAX || action.0 < open.tabs.len() {
            self.activate_tab(ix, window, cx);
        }
    }

    // ---- 标签条 ----

    pub(super) fn render_tab_strip(&self, open: &VaultSession, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs: Vec<AnyElement> = open
            .tabs
            .iter()
            .enumerate()
            .map(|(ix, tab)| self.render_tab(ix, tab, ix == open.active, cx))
            .collect();
        h_flex()
            .id("tab-strip")
            .debug_selector(|| "tab-strip".into())
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .px_1()
            .gap_1()
            .child(
                h_flex()
                    .id("tabs")
                    .debug_selector(|| "tabs-viewport".into())
                    .min_w_0()
                    .flex_shrink(1.)
                    .h_full()
                    .items_center()
                    .gap_1()
                    .overflow_x_scroll()
                    .track_scroll(&self.tab_scroll)
                    .children(tabs),
            )
            .child(
                stop_title_drag(
                    Button::new("new-tab")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .tooltip_with_action("新建标签页", &NewTab, None)
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(NewTab), cx)),
                )
                .debug_selector(|| "new-tab-button".into()),
            )
    }

    fn render_tab(&self, ix: usize, tab: &Tab, active: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (hover, border, background) = (theme.list_hover, theme.border, theme.background);
        let (foreground, muted, danger, ring) =
            (theme.foreground, theme.muted_foreground, theme.danger, theme.ring);
        let title = tab.title(cx);
        let tooltip = tab.path(cx).map(|p| p.as_str().to_owned()).unwrap_or_else(|| title.clone());
        let marker = tab.editor.as_ref().and_then(|e| match e.read(cx).save_state() {
            SaveState::Dirty | SaveState::Saving => Some(muted),
            SaveState::Failed(_) => Some(danger),
            _ => None,
        });
        let id = tab.id;
        h_flex()
            .id(("tab", id))
            .debug_selector(move || format!("tab:{ix}"))
            .group("tab")
            .flex_shrink(1.)
            .w(px(180.))
            .min_w(px(84.))
            .max_w(px(220.))
            .h(px(26.))
            .pl_2()
            .pr_1()
            .gap_1()
            .items_center()
            .rounded_md()
            .border_1()
            .text_sm()
            .cursor_pointer()
            .map(|el| {
                if active {
                    el.bg(background).border_color(border).text_color(foreground)
                } else {
                    el.border_color(gpui_kit::transparent_black())
                        .text_color(muted)
                        .hover(move |s| s.bg(hover).text_color(foreground))
                }
            })
            .when_some(marker, |el, color| el.child(div().flex_none().size(px(6.)).rounded_full().bg(color)))
            .child(self.render_tab_label(id, &title, tab.preview))
            .child(
                div()
                    .id(("tab-close", id))
                    .debug_selector(move || format!("tab-close:{ix}"))
                    .flex_none()
                    .size(px(18.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .when(!active, |s| s.invisible().group_hover("tab", |s| s.visible()))
                    .hover(move |s| s.bg(hover))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        if let Some(ix) =
                            this.open.as_ref().and_then(|o| o.tabs.iter().position(|t| t.id == id))
                        {
                            this.close_tab(ix, window, cx);
                        }
                    }))
                    .child(Icon::new(IconName::Close).size(px(12.)).text_color(muted)),
            )
            .tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx)
            })
            // 按下就切过去（与浏览器一致），并拦住标题栏的拖动窗口
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let Some(ix) = this.open.as_ref().and_then(|o| o.tabs.iter().position(|t| t.id == id))
                    else {
                        return;
                    };
                    if event.click_count >= 2 {
                        this.keep_active_preview();
                        cx.notify();
                    } else if this.open.as_ref().is_some_and(|o| o.active != ix) {
                        this.activate_tab(ix, window, cx);
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, window, cx| {
                    if let Some(ix) =
                        this.open.as_ref().and_then(|open| open.tabs.iter().position(|tab| tab.id == id))
                        && this.open.as_ref().is_some_and(|open| open.active != ix)
                    {
                        this.activate_tab(ix, window, cx);
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    if let Some(ix) = this.open.as_ref().and_then(|o| o.tabs.iter().position(|t| t.id == id))
                    {
                        this.close_tab(ix, window, cx);
                    }
                }),
            )
            .on_drag(TabDrag { id, title }, |drag, _, _, cx| cx.new(|_| drag.clone()))
            .drag_over::<TabDrag>(
                move |style, drag, _, _| {
                    if drag.id == id { style } else { style.border_l_2().border_color(ring) }
                },
            )
            .on_drop(cx.listener(move |this, drag: &TabDrag, _, cx| this.move_tab(drag.id, id, cx)))
            .context_menu({
                let path = tab.path(cx).map(|path| path.as_str().to_owned());
                let retitle =
                    tab.editor.as_ref().is_some_and(|editor| editor.read(cx).has_front_matter_title());
                move |menu, _, _| {
                    let mut menu = menu;
                    if path.is_some() {
                        menu = menu.menu("重命名", Box::new(RenameActiveTab));
                        if retitle {
                            menu = menu.menu("修改标题", Box::new(RetitleActiveTab));
                        }
                        menu = menu.separator();
                    }
                    menu = menu
                        .menu("关闭", Box::new(CloseTab))
                        .menu("关闭其他标签页", Box::new(CloseOtherTabs));
                    if let Some(path) = path.clone() {
                        menu = menu
                            .separator()
                            .menu("在 Finder 中显示", Box::new(RevealInFinder(path.clone())))
                            .menu("复制路径", Box::new(CopyNotePath(path)));
                    }
                    menu
                }
            })
            .into_any_element()
    }

    fn render_tab_label(&self, id: u64, title: &str, preview: bool) -> AnyElement {
        if let Some(edit) = self.tab_name_edit.as_ref().filter(|edit| edit.tab == id) {
            return div()
                .id(("tab-rename", id))
                .debug_selector(|| "tab-rename".into())
                .flex_1()
                .min_w_0()
                .child(Input::new(&edit.input))
                .into_any_element();
        }
        div()
            .flex_1()
            .min_w_0()
            .truncate()
            .when(preview, |el| el.italic())
            .child(title.to_owned())
            .into_any_element()
    }
}

/// 标题栏里的可点元素：拦住按下事件，免得标题栏把它当成拖动窗口的起点。
pub(super) fn stop_title_drag(child: impl IntoElement) -> Div {
    div().flex_none().on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()).child(child)
}
