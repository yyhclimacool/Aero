//! 块级 Live Preview 编辑器（`ui_design.md` 第 5、6 节；方案第 9 节），M1a 子集。
//!
//! 结构：
//! - 文档真身是 `aero_core::Document`（Rope + 块树 + undo 栈）；
//! - 浏览态：每个编辑单位（[`units::Unit`]）用 `TextView::markdown` 渲染；
//! - 编辑态：只有聚焦单位换成一个 `TextareaState`，它的文本与文档中 `focused.range` 这一段一一对应。
//!   控件每次 `Change` 都被收缩成一次最小 `Edit` 写回 Document，再同步重解析。
//!
//! 聚焦的"真相"是 `focused.range`（随编辑漂移的字节区间），而不是块 id：这样把段落删空、
//! 或粘贴出多个块时，控件仍然有归属，不会因为块消失而失焦。

mod ops;
#[cfg(test)]
mod tests;
mod text_ops;
mod units;

use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aero_core::{Block, BlockId, BlockKind, Document, Edit, FileStamp, Vault, VaultError, VaultPath};
use gpui_kit::base::input::{Backspace, Escape, InputEvent, Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::text::{TextView, TextViewStyle};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::actions::{BlockDown, BlockUp, BlurEditor, DocRedo, DocUndo, SplitBlock};
use ops::Plan;
use text_ops::*;
use units::{Unit, unit_at};

/// 自动保存的静默间隔（`ui_design.md` 10.1）。
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
/// 相邻输入并入同一个 undo 组的时间窗口（方案 9.5）。
const COALESCE_WINDOW: Duration = Duration::from_millis(600);
/// 轮询磁盘文件 mtime/size 的间隔（方案 12 的 `notify` 监听是 M1b 事项；轮询只做一次 stat，够便宜）。
const EXTERNAL_POLL: Duration = Duration::from_secs(2);

pub const BODY_FONT_SIZE: f32 = 16.;
pub const BODY_LINE_HEIGHT: f32 = 1.7;

pub enum EditorEvent {
    /// 内容、光标或保存状态变化，状态栏据此刷新
    Changed,
    SaveFailed(String),
    /// 磁盘内容被别的程序改了，而这里没有未保存修改：已静默重载（方案 12）
    Reloaded,
    /// 磁盘内容与未保存修改冲突：磁盘版本已另存到该路径，本地版本照常落盘
    Conflict(VaultPath),
}

impl EventEmitter<EditorEvent> for Editor {}

#[derive(Clone, Debug, PartialEq)]
pub enum SaveState {
    Clean,
    Dirty,
    Saving,
    Saved(Instant),
    Failed(String),
}

struct Focused {
    /// 控件文本对应的文档区间，随编辑漂移
    range: Range<usize>,
    /// 区间起点上的编辑单位（渲染时决定把控件画在哪个块的位置）；没有块时为 `None`
    id: Option<BlockId>,
    /// Enter 直接换行（代码块等）还是拆块
    enter_newline: bool,
    textarea: Entity<TextareaState>,
    _subscription: Subscription,
}

pub struct Editor {
    vault: Arc<Vault>,
    path: VaultPath,
    doc: Document,
    /// 上次读 / 写磁盘时文件的指纹，用来发现外部修改（5.5）
    stamp: FileStamp,
    units: Vec<Unit>,
    focused: Option<Focused>,
    focus_handle: FocusHandle,
    scroll: ScrollHandle,
    dirty: bool,
    save_state: SaveState,
    save_task: Option<Task<()>>,
    last_edit_at: Option<Instant>,
    /// 正在把点击重放给刚出现的控件（见 `focus_block_at`），此时再收到的点击不要再重放
    replaying_click: bool,
    _poll_task: Task<()>,
}

impl Editor {
    /// 从磁盘读取笔记。读失败时由调用方展示错误，不构造编辑器。
    pub fn load(vault: Arc<Vault>, path: VaultPath) -> Result<(Document, FileStamp), VaultError> {
        vault.read(&path)
    }

    pub fn new(
        vault: Arc<Vault>,
        path: VaultPath,
        doc: Document,
        stamp: FileStamp,
        cx: &mut Context<Self>,
    ) -> Self {
        let units = units::units(&doc);
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(EXTERNAL_POLL).await;
                if this.update(cx, |this, cx| this.check_external(cx)).is_err() {
                    break;
                }
            }
        });
        Editor {
            vault,
            path,
            doc,
            stamp,
            units,
            focused: None,
            focus_handle: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            dirty: false,
            save_state: SaveState::Clean,
            save_task: None,
            last_edit_at: None,
            replaying_click: false,
            _poll_task: poll,
        }
    }

    pub fn path(&self) -> &VaultPath {
        &self.path
    }

    pub fn title(&self) -> &str {
        self.path.stem()
    }

    pub fn save_state(&self) -> &SaveState {
        &self.save_state
    }

    pub fn word_count(&self) -> usize {
        word_count(&self.doc.to_string())
    }

    /// 浏览态聚焦（键盘可用，但没有光标）。
    pub fn focus_browse(&self, window: &mut Window, cx: &mut App) {
        self.focus_handle.focus(window, cx);
    }

    /// 标题树（右侧大纲面板）：`(level, 文本, 块 id)`。
    pub fn headings(&self) -> Vec<(u8, String, BlockId)> {
        self.doc
            .blocks()
            .iter()
            .filter_map(|(_, b)| match b.kind {
                BlockKind::Heading { level } => {
                    let text = self.doc.slice(self.doc.content_range(b)).to_string();
                    let text = text[heading_prefix_len(&text)..].trim().to_owned();
                    Some((level, text, b.id))
                }
                _ => None,
            })
            .collect()
    }

    /// 新建笔记后直接进入编辑态（光标在文末的空段落）。
    pub fn start_writing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.append_paragraph(window, cx);
    }

    /// 大纲点击：把该块置为编辑态（光标在块尾），并滚到可见位置。
    pub fn focus_block(&mut self, id: BlockId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.units.iter().position(|u| u.id == id) {
            let len = self.units[ix].range.len();
            self.focus_unit(ix, len, window, cx);
        }
    }

    /// 点击浏览态的块：进入编辑态，然后把这次点击重放给刚出现的控件，由它按像素位置放光标。
    /// 渲染文本与源码只差行内标记（`**`、`[[`），所以落点是"近似到字符"；不重放的话光标总在块尾。
    /// 控件要先画一帧才有布局，而 `on_next_frame` 的回调在绘制之前跑，所以隔两帧再派发。
    fn focus_block_at(&mut self, id: BlockId, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_block(id, window, cx);
        if self.replaying_click || !self.is_focused_block(id) {
            return;
        }
        let this = cx.weak_entity();
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| {
                let Some(editor) = this.upgrade() else { return };
                if !editor.read(cx).is_focused_block(id) {
                    return;
                }
                editor.update(cx, |e, _| e.replaying_click = true);
                let modifiers = Modifiers::default();
                window.dispatch_event(
                    PlatformInput::MouseDown(MouseDownEvent {
                        button: MouseButton::Left,
                        position,
                        modifiers,
                        click_count: 1,
                        first_mouse: false,
                    }),
                    cx,
                );
                window.dispatch_event(
                    PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position, modifiers, click_count: 1 }),
                    cx,
                );
                editor.update(cx, |e, _| e.replaying_click = false);
            });
        });
    }

    // ---- 聚焦 ----

    fn focus_unit(&mut self, ix: usize, cursor: usize, window: &mut Window, cx: &mut Context<Self>) {
        let unit = &self.units[ix];
        let (range, id, nl) = (unit.range.clone(), Some(unit.id), unit.enter_inserts_newline());
        self.focus_range(range, id, nl, cursor, window, cx);
    }

    /// 聚焦文档偏移 `offset` 处的单位；那里没有块（空段落）时聚焦一个空区间。
    fn focus_at(
        &mut self,
        offset: usize,
        cursor: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = offset.min(self.doc.len());
        // 正好落在某个单位上；否则若 offset 与下一个单位之间只有同一行的空白（段首空格被 Markdown 忽略），
        // 也算落在那个单位上
        let hit = unit_at(&self.units, offset).or_else(|| {
            let ix = self.units.iter().position(|u| u.range.start >= offset)?;
            let gap = self.doc.slice(offset..self.units[ix].range.start).to_string();
            gap.chars().all(|c| c == ' ' || c == '\t').then_some(ix)
        });
        match hit {
            Some(ix) => {
                let cursor = cursor.unwrap_or(offset.saturating_sub(self.units[ix].range.start));
                self.focus_unit(ix, cursor, window, cx)
            }
            None => self.focus_range(offset..offset, None, false, 0, window, cx),
        }
    }

    fn focus_range(
        &mut self,
        range: Range<usize>,
        id: Option<BlockId>,
        enter_newline: bool,
        cursor: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self.doc.slice(range.clone()).to_string();
        let cursor = clamp_to_char_boundary(&text, cursor.min(text.len()));
        let textarea = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 100_000));
        let subscription = cx.subscribe_in(&textarea, window, Self::on_input_event);
        textarea.update(cx, |ta, cx| {
            ta.set_value(text, window, cx);
            ta.set_selected_range(cursor..cursor, cx);
            ta.focus(window, cx);
        });
        self.last_edit_at = None;
        log::debug!("focus {:?} id={:?} cursor={cursor}", range, id);
        self.focused = Some(Focused { range, id, enter_newline, textarea, _subscription: subscription });
        cx.notify();
    }

    /// 退出编辑态（不改焦点）。失焦即落盘（10.1）。
    fn exit_edit(&mut self, cx: &mut Context<Self>) {
        if self.focused.take().is_some() {
            self.last_edit_at = None;
            self.save_now(cx);
            cx.emit(EditorEvent::Changed);
            cx.notify();
        }
    }

    fn blur_to_browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.exit_edit(cx);
        self.focus_handle.focus(window, cx);
    }

    // ---- 控件事件 ----

    fn on_input_event(
        &mut self,
        textarea: &Entity<TextareaState>,
        event: &InputEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_current = self.focused.as_ref().is_some_and(|f| &f.textarea == textarea);
        if !is_current {
            return;
        }
        match event {
            InputEvent::Change => self.on_change(cx),
            // ⏎ 由 SplitBlock 动作截获（见 actions.rs）；⇧⏎ 软换行由控件自己插入，随后的 Change 已经写回
            InputEvent::PressEnter { .. } | InputEvent::Focus => {}
            InputEvent::Blur => self.exit_edit(cx),
        }
    }

    /// 控件文本 → 文档：收缩成最小编辑写回。
    fn on_change(&mut self, cx: &mut Context<Self>) {
        let Some(f) = self.focused.as_mut() else { return };
        let new_text = f.textarea.read(cx).value();
        let old_text = self.doc.slice(f.range.clone()).to_string();
        let Some(edit) = minimal_edit(f.range.start, &old_text, &new_text) else { return };

        let now = Instant::now();
        let small = !edit.text.contains('\n') && edit.text.len() <= 8 && edit.range.len() <= 8;
        let coalesce = small && self.last_edit_at.is_some_and(|t| now.duration_since(t) < COALESCE_WINDOW);
        let delta = edit.delta();
        let result = if coalesce { self.doc.edit_coalescing(edit) } else { self.doc.edit(edit) };
        if let Err(err) = result {
            log::error!("edit rejected: {err}");
            return;
        }
        f.range.end = (f.range.end as isize + delta).max(f.range.start as isize) as usize;
        self.last_edit_at = Some(now);
        self.after_doc_change(cx);
    }

    /// 文本已改：重解析、刷新单位、标脏、排队保存。
    fn after_doc_change(&mut self, cx: &mut Context<Self>) {
        self.doc.reparse();
        self.units = units::units(&self.doc);
        if let Some(f) = self.focused.as_mut() {
            f.id = unit_at(&self.units, f.range.start)
                .filter(|&ix| self.units[ix].range.start == f.range.start)
                .map(|ix| self.units[ix].id);
        }
        self.dirty = true;
        self.save_state = SaveState::Dirty;
        self.schedule_save(cx);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// 一次不经控件的结构性编辑（拆块、合并、undo）：写回文档后把焦点放到 `offset`。
    fn structural_edit(
        &mut self,
        edit: Edit,
        focus_offset: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(err) = self.doc.edit(edit) {
            log::error!("edit rejected: {err}");
            return;
        }
        self.focused = None;
        self.after_doc_change(cx);
        self.focus_at(focus_offset, None, window, cx);
    }

    /// 当前聚焦区间起点上的单位下标。
    fn focused_unit(&self) -> Option<usize> {
        let f = self.focused.as_ref()?;
        unit_at(&self.units, f.range.start).filter(|&ix| self.units[ix].range.start == f.range.start)
    }

    fn textarea_text_and_cursor(&self, cx: &App) -> Option<(String, usize)> {
        let f = self.focused.as_ref()?;
        let ta = f.textarea.read(cx);
        Some((ta.value().to_string(), ta.cursor()))
    }

    // ---- Enter / Backspace ----

    /// ⏎：段落末新建段落、段落中拆块；列表项续行、空项退出列表（6、7.3）。
    /// 代码块等多行单位 propagate，让文本控件自己插入换行。
    fn on_split_block(&mut self, _: &SplitBlock, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, cursor)) = self.textarea_text_and_cursor(cx) else { cx.propagate(); return };
        let Some(f) = self.focused.as_ref() else { cx.propagate(); return };
        if f.enter_newline {
            cx.propagate();
            return;
        }
        let range = f.range.clone();
        let unit = self.focused_unit().map(|ix| self.units[ix].clone());
        let plan = ops::split(&self.doc, unit.as_ref(), &range, cursor);
        self.apply_plan(plan, window, cx);
    }

    /// ⌫ 在块首：列表项脱离列表；段落合并到上一块（保留上一块类型）；否则只移动光标。
    fn on_backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, cursor)) = self.textarea_text_and_cursor(cx) else {
            cx.propagate();
            return;
        };
        if cursor != 0 {
            cx.propagate();
            return;
        }
        let outcome = match self.focused_unit() {
            Some(ix) => ops::backspace_at_start(&self.doc, &self.units, ix),
            None => {
                let offset = self.focused.as_ref().map_or(0, |f| f.range.start);
                ops::backspace_at_empty_slot(&self.doc, &self.units, offset)
            }
        };
        match outcome {
            ops::Backspace::Edit(plan) => self.apply_plan(plan, window, cx),
            ops::Backspace::MoveTo { unit, cursor } => self.focus_unit(unit, cursor, window, cx),
            ops::Backspace::Nothing => {}
        }
    }

    fn apply_plan(&mut self, plan: Plan, window: &mut Window, cx: &mut Context<Self>) {
        self.structural_edit(plan.edit, plan.focus_offset, window, cx);
    }

    // ---- ↑ / ↓ 跨块 ----

    fn on_block_up(&mut self, _: &BlockUp, window: &mut Window, cx: &mut Context<Self>) {
        let Some((text, cursor)) = self.textarea_text_and_cursor(cx) else {
            cx.propagate();
            return;
        };
        let (line, _) = line_col(&text, cursor);
        if line != 0 {
            cx.propagate();
            return;
        }
        let col = cursor - line_start(&text, cursor);
        let start = self.focused.as_ref().map_or(0, |f| f.range.start);
        if let Some((ix, cursor)) = ops::up_target(&self.doc, &self.units, start, col) {
            self.focus_unit(ix, cursor, window, cx);
        }
    }

    fn on_block_down(&mut self, _: &BlockDown, window: &mut Window, cx: &mut Context<Self>) {
        let Some((text, cursor)) = self.textarea_text_and_cursor(cx) else {
            cx.propagate();
            return;
        };
        let (line, _) = line_col(&text, cursor);
        if line + 1 != line_count(&text) {
            cx.propagate();
            return;
        }
        let col = cursor - line_start(&text, cursor);
        let Some(f) = self.focused.as_ref() else { return };
        let range = f.range.clone();
        match ops::down_target(&self.doc, &self.units, &range, col) {
            Some((ix, cursor)) => self.focus_unit(ix, cursor, window, cx),
            None => {
                // 已是最后一块：非文本块（代码块等）用 ↓ 离开时在文末补一个空段落
                let is_text = self.focused_unit().is_none_or(|ix| self.units[ix].is_text());
                if !is_text {
                    self.append_paragraph(window, cx);
                }
            }
        }
    }

    /// 在文末新建空段落并聚焦。
    fn append_paragraph(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match ops::append_paragraph(&self.doc) {
            (Some(edit), at) => self.structural_edit(edit, at, window, cx),
            (None, at) => {
                self.focused = None;
                self.focus_at(at, None, window, cx);
            }
        }
    }

    /// 点击正文下方空白：聚焦最后一个段落尾，或新建段落。
    fn on_click_tail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.units.last() {
            Some(last) if last.is_text() && last.list.is_none() => {
                let ix = self.units.len() - 1;
                let len = last.range.len();
                self.focus_unit(ix, len, window, cx);
            }
            _ => self.append_paragraph(window, cx),
        }
    }

    // ---- 撤销 / 重做 ----

    fn on_undo(&mut self, _: &DocUndo, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(affected) = self.doc.undo() {
            self.after_history(affected, window, cx);
        }
    }

    fn on_redo(&mut self, _: &DocRedo, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(affected) = self.doc.redo() {
            self.after_history(affected, window, cx);
        }
    }

    /// 撤销 / 重做之后：光标放到受影响区间的末尾（编辑态下），像所有编辑器那样"跟着变化走"。
    fn after_history(&mut self, affected: Range<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.last_edit_at = None;
        let was_editing = self.focused.take().is_some();
        self.after_doc_change(cx);
        if was_editing {
            self.focus_at(affected.end, None, window, cx);
        }
    }

    // ---- 保存 ----

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| this.save_now(cx));
        }));
    }

    /// 立即（异步）写盘。写前先看磁盘有没有被别人改过（5.5 写前校验）。
    pub fn save_now(&mut self, cx: &mut Context<Self>) {
        if !self.dirty || self.save_state == SaveState::Saving {
            return;
        }
        self.save_task = None;
        if let Some((disk, stamp)) = self.external_change() {
            self.write_conflict_copy(&disk, stamp, cx);
        }
        let bytes = self.doc.to_bytes();
        let version = self.doc.version();
        let (vault, path) = (self.vault.clone(), self.path.clone());
        self.save_state = SaveState::Saving;
        cx.emit(EditorEvent::Changed);
        let write = cx.background_spawn(async move { vault.write_bytes(&path, &bytes) });
        cx.spawn(async move |this, cx| {
            let result = write.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(stamp) => {
                        this.stamp = stamp;
                        if this.doc.version() == version {
                            this.dirty = false;
                            this.save_state = SaveState::Saved(Instant::now());
                        } else {
                            // 写盘期间又有编辑，交给它自己的定时器
                            this.save_state = SaveState::Dirty;
                            if this.save_task.is_none() {
                                this.schedule_save(cx);
                            }
                        }
                    }
                    Err(err) => {
                        let msg = err.to_string();
                        log::error!("save {}: {msg}", this.path.as_str());
                        this.save_state = SaveState::Failed(msg.clone());
                        cx.emit(EditorEvent::SaveFailed(msg));
                    }
                }
                cx.emit(EditorEvent::Changed);
                cx.notify();
            });
        })
        .detach();
    }

    /// 同步落盘（切换笔记、退出前）。有外部修改时同样先留冲突副本，返回其路径。
    pub fn flush_sync(&mut self) -> Result<Option<VaultPath>, VaultError> {
        if !self.dirty {
            return Ok(None);
        }
        self.save_task = None;
        let conflict = match self.external_change() {
            Some((disk, _)) => Some(self.vault.write_bytes(&self.conflict_path(), &disk).map(|_| self.conflict_path())?),
            None => None,
        };
        let stamp = self.vault.write_bytes(&self.path, &self.doc.to_bytes())?;
        self.stamp = stamp;
        self.dirty = false;
        self.save_state = SaveState::Saved(Instant::now());
        Ok(conflict)
    }

    // ---- 外部修改（方案 12，M1a 子集：轮询 + 写前校验；`notify` 监听与三方合并留给 M1b） ----

    /// 磁盘文件是否被别的程序改过：先比 mtime/size（不读内容），变了再比内容 hash。
    /// 返回磁盘上的新内容与指纹。文件不存在时视为"没变"——下次保存会重建它。
    fn external_change(&mut self) -> Option<(Vec<u8>, FileStamp)> {
        let (mtime, size) = self.vault.stat(&self.path).ok()?;
        if mtime == self.stamp.mtime && size == self.stamp.size {
            return None;
        }
        let (bytes, stamp) = self.vault.read_bytes(&self.path).ok()?;
        if stamp.hash == self.stamp.hash {
            // 只是被 touch 了一下（或同步工具写回同样的内容）
            self.stamp = stamp;
            return None;
        }
        Some((bytes, stamp))
    }

    /// 轮询 / 窗口激活时调用：无未保存修改则静默重载，有则留冲突副本并立即落盘本地版本。
    pub fn check_external(&mut self, cx: &mut Context<Self>) {
        if self.save_state == SaveState::Saving {
            return;
        }
        let Some((disk, stamp)) = self.external_change() else { return };
        if self.dirty {
            self.write_conflict_copy(&disk, stamp, cx);
            self.save_now(cx);
        } else {
            self.reload(&disk, stamp, cx);
        }
    }

    /// 用磁盘内容替换文档（undo 栈随之丢弃）。编辑态退回浏览态：旧的控件区间对新文本已无意义，
    /// 而这里拿不到 `Window` 去重新聚焦——无未保存修改时通常也没人正在打字。
    fn reload(&mut self, disk: &[u8], stamp: FileStamp, cx: &mut Context<Self>) {
        let Ok(doc) = Document::from_bytes(disk) else {
            log::warn!("reload {}: not valid UTF-8, keeping in-memory version", self.path.as_str());
            return;
        };
        log::info!("reload {} (changed on disk)", self.path.as_str());
        self.focused = None;
        self.doc = doc;
        self.stamp = stamp;
        self.units = units::units(&self.doc);
        self.dirty = false;
        self.save_task = None;
        self.last_edit_at = None;
        self.save_state = SaveState::Clean;
        cx.emit(EditorEvent::Reloaded);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// `<name>.conflict-<本地时间>.md`，与原文件同目录。
    fn conflict_path(&self) -> VaultPath {
        let time = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let name = format!("{}.conflict-{time}.md", self.path.stem());
        let parent = self.path.parent();
        let s = if parent.is_empty() { name } else { format!("{parent}/{name}") };
        VaultPath::new(&s).expect("conflict path derived from a valid note path")
    }

    /// 把磁盘版本另存为冲突副本，然后本地版本继续走正常保存（绝不静默覆盖外部修改）。
    fn write_conflict_copy(&mut self, disk: &[u8], stamp: FileStamp, cx: &mut Context<Self>) {
        let conflict = self.conflict_path();
        match self.vault.write_bytes(&conflict, disk) {
            Ok(_) => {
                log::warn!("{}: changed on disk while dirty; disk version saved as {}", self.path.as_str(), conflict.as_str());
                // 记下磁盘当前指纹：接下来的写盘不该再把同一份外部修改当成新冲突
                self.stamp = stamp;
                cx.emit(EditorEvent::Conflict(conflict));
            }
            Err(err) => {
                let msg = format!("无法保存冲突副本 {}：{err}", conflict.as_str());
                log::error!("{msg}");
                cx.emit(EditorEvent::SaveFailed(msg));
            }
        }
    }

    // ---- 渲染 ----

    fn text_view_style(&self, cx: &App) -> TextViewStyle {
        let theme = cx.theme();
        TextViewStyle {
            paragraph_gap: rems(0.5),
            heading_base_font_size: px(BODY_FONT_SIZE),
            heading_font_size: Some(Arc::new(|level, base| match level {
                1 => base * 1.75,
                2 => base * 1.4,
                3 => base * 1.2,
                _ => base,
            })),
            highlight_theme: theme.highlight_theme.clone(),
            is_dark: theme.mode.is_dark(),
            ..Default::default()
        }
    }

    fn is_focused_block(&self, id: BlockId) -> bool {
        self.focused.as_ref().is_some_and(|f| f.id == Some(id))
    }

    /// 块整体落在聚焦区间内但不是聚焦块（粘贴出多块时），渲染时跳过。
    fn is_covered(&self, range: &Range<usize>) -> bool {
        self.focused
            .as_ref()
            .is_some_and(|f| f.range.start <= range.start && range.end <= f.range.end && !f.range.is_empty())
    }

    fn render_textarea(&self, kind: Option<&BlockKind>, cx: &Context<Self>) -> AnyElement {
        let Some(f) = self.focused.as_ref() else { return div().into_any_element() };
        let theme = cx.theme();
        let mut el = div().w_full().child(Textarea::new(&f.textarea));
        match kind {
            Some(BlockKind::Heading { level }) => {
                el = el
                    .text_size(heading_size(*level))
                    .font_weight(FontWeight::SEMIBOLD)
                    .line_height(relative(1.3));
            }
            Some(k) if is_monospace(k) => {
                el = el
                    .font_family(theme.mono_font_family.clone())
                    .text_size(px(14.))
                    .line_height(relative(1.6))
                    .bg(theme.muted)
                    .rounded(px(6.))
                    .px_3()
                    .py_2();
            }
            _ => {}
        }
        el.into_any_element()
    }

    fn render_blocks(&self, blocks: &[Block], cx: &mut Context<Self>) -> Vec<AnyElement> {
        // 聚焦区间没有落在任何块上（空段落）时，控件画在它所在位置
        let mut pending = self.focused.as_ref().filter(|f| f.id.is_none()).map(|f| f.range.start);
        let mut out = Vec::with_capacity(blocks.len() + 1);
        for b in blocks {
            if let Some(off) = pending
                && off <= self.doc.range_of(b).start
            {
                out.push(self.render_textarea(None, cx));
                pending = None;
            }
            if let Some(el) = self.render_block(b, 0, cx) {
                out.push(el);
            }
        }
        if pending.is_some() {
            out.push(self.render_textarea(None, cx));
        }
        out
    }

    fn render_block(&self, b: &Block, depth: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        match &b.kind {
            BlockKind::Marker => None,
            BlockKind::List { ordered, start } => {
                let items: Vec<&Block> =
                    b.children.iter().filter(|c| matches!(c.kind, BlockKind::ListItem { .. })).collect();
                let els: Vec<AnyElement> = items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| {
                        self.render_list_item(item, ordered.then_some(*start + i as u32), depth, cx)
                    })
                    .collect();
                Some(v_flex().w_full().gap(px(4.)).children(els).into_any_element())
            }
            BlockKind::ListItem { .. } => Some(self.render_list_item(b, None, depth, cx)),
            k if units::is_atomic(k) => self.render_unit(b, cx),
            _ => {
                let els: Vec<AnyElement> =
                    b.children.iter().filter_map(|c| self.render_block(c, depth, cx)).collect();
                Some(v_flex().w_full().gap(px(8.)).children(els).into_any_element())
            }
        }
    }

    fn render_list_item(
        &self,
        item: &Block,
        number: Option<u32>,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let content: Vec<&Block> = item.children.iter().filter(|c| c.kind != BlockKind::Marker).collect();
        let task = match item.kind {
            BlockKind::ListItem { task } => task,
            _ => None,
        };
        let marker: AnyElement = match (task, number) {
            (Some(checked), _) => div()
                .text_color(if checked { theme.primary } else { theme.muted_foreground })
                .child(if checked { "☑" } else { "☐" })
                .into_any_element(),
            (None, Some(n)) => {
                div().text_color(theme.muted_foreground).child(format!("{n}.")).into_any_element()
            }
            (None, None) => div()
                .text_color(theme.muted_foreground)
                .child(match depth % 3 {
                    0 => "•",
                    1 => "◦",
                    _ => "▪",
                })
                .into_any_element(),
        };

        let body: AnyElement = if content.is_empty() {
            if self.is_focused_block(item.id) {
                self.render_textarea(None, cx)
            } else {
                let id = item.id;
                div()
                    .id(("empty-item", id.0))
                    .w_full()
                    .h(px(BODY_FONT_SIZE * BODY_LINE_HEIGHT))
                    .cursor_text()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| this.focus_block(id, window, cx)),
                    )
                    .into_any_element()
            }
        } else {
            let els: Vec<AnyElement> =
                content.iter().filter_map(|c| self.render_block(c, depth + 1, cx)).collect();
            v_flex().w_full().gap(px(4.)).children(els).into_any_element()
        };

        h_flex()
            .w_full()
            .items_start()
            .gap(px(4.))
            .child(div().flex_none().w(px(24.)).text_center().child(marker))
            .child(div().flex_1().min_w_0().child(body))
            .into_any_element()
    }

    /// 一个编辑单位：聚焦则画控件，否则渲染。
    fn render_unit(&self, b: &Block, cx: &mut Context<Self>) -> Option<AnyElement> {
        let range = self.doc.content_range(b);
        if self.is_focused_block(b.id) {
            return Some(self.render_textarea(Some(&b.kind), cx));
        }
        if self.is_covered(&range) {
            return None;
        }
        let theme = cx.theme();
        let text: SharedString = self.doc.slice(range).to_string().into();
        let id = b.id;
        let focus = cx.listener(move |this: &mut Self, ev: &MouseDownEvent, window, cx| {
            this.focus_block_at(id, ev.position, window, cx)
        });

        let body: AnyElement = match &b.kind {
            BlockKind::ThematicBreak => div().w_full().h(px(1.)).my_3().bg(theme.border).into_any_element(),
            BlockKind::FrontMatter | BlockKind::HtmlBlock | BlockKind::Raw => div()
                .w_full()
                .font_family(theme.mono_font_family.clone())
                .text_size(px(13.))
                .line_height(relative(1.6))
                .text_color(theme.muted_foreground)
                .bg(theme.muted)
                .rounded(px(6.))
                .px_3()
                .py_2()
                .child(text)
                .into_any_element(),
            _ => {
                let text = if text.is_empty() { SharedString::from("\u{200b}") } else { text };
                // TextView 的解析状态按元素 id 缓存；带上编辑器实体 id，切换笔记后不会复用上一篇的状态
                let view_id = SharedString::from(format!("blk-{}-{}", cx.entity_id(), id.0));
                TextView::markdown(view_id, text)
                    .style(self.text_view_style(cx))
                    .w_full()
                    .into_any_element()
            }
        };
        let is_heading = matches!(b.kind, BlockKind::Heading { .. });
        Some(
            div()
                .id(("unit", id.0))
                .debug_selector(|| format!("unit-{}", id.0))
                .w_full()
                .min_h(px(BODY_FONT_SIZE * BODY_LINE_HEIGHT))
                .when(is_heading, |el| el.pt_2())
                .cursor_text()
                .on_mouse_down(MouseButton::Left, focus)
                .child(body)
                .into_any_element(),
        )
    }
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Editor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (foreground, muted) = (cx.theme().foreground, cx.theme().muted_foreground);
        let roots = self.doc.blocks().roots.clone();
        let blocks = self.render_blocks(&roots, cx);
        let empty = self.doc.blocks().is_empty() && self.focused.is_none();

        div()
            .id("editor")
            .key_context("Editor")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_block_up))
            .on_action(cx.listener(Self::on_block_down))
            .on_action(cx.listener(Self::on_split_block))
            .on_action(cx.listener(Self::on_undo))
            .on_action(cx.listener(Self::on_redo))
            .on_action(cx.listener(Self::on_backspace))
            .on_action(cx.listener(|this, _: &Escape, window, cx| this.blur_to_browse(window, cx)))
            .on_action(cx.listener(|this, _: &BlurEditor, window, cx| this.blur_to_browse(window, cx)))
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(720. + 48.))
                    .mx_auto()
                    .px_6()
                    .pt_10()
                    .text_size(px(BODY_FONT_SIZE))
                    .line_height(relative(BODY_LINE_HEIGHT))
                    .text_color(foreground)
                    .child(
                        div()
                            .pb_4()
                            .mb_2()
                            .text_size(px(28.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .line_height(relative(1.3))
                            .child(self.title().to_owned()),
                    )
                    .child(v_flex().w_full().gap(px(8.)).children(blocks))
                    .when(empty, |el| el.child(div().text_color(muted).child("点击这里开始写，或按 ⏎")))
                    .child(
                        div()
                            .id("editor-tail")
                            .w_full()
                            .min_h(px(240.))
                            .flex_1()
                            .cursor_text()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, window, cx| this.on_click_tail(window, cx)),
                            ),
                    ),
            )
            .vertical_scrollbar(&self.scroll)
    }
}

fn heading_size(level: u8) -> Pixels {
    px(BODY_FONT_SIZE
        * match level {
            1 => 1.75,
            2 => 1.4,
            3 => 1.2,
            _ => 1.0,
        })
}

fn is_monospace(kind: &BlockKind) -> bool {
    matches!(
        kind,
        BlockKind::CodeFence { .. }
            | BlockKind::CodeBlock
            | BlockKind::MathBlock
            | BlockKind::Table
            | BlockKind::HtmlBlock
            | BlockKind::Raw
            | BlockKind::FrontMatter
    )
}

fn clamp_to_char_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
