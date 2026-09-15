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
mod preview;
mod slash;
#[cfg(test)]
mod tests;
mod text_ops;
mod units;

use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aero_core::{Block, BlockId, BlockKind, Document, Edit, FileStamp, Vault, VaultError, VaultPath};
use gpui_kit::base::input::{Backspace, Escape, Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::text::TextViewStyle;
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::actions::{
    BlockDown, BlockUp, BlurEditor, ConvertBlock, DocRedo, DocUndo, MenuConfirm, SplitBlock,
};
use ops::{BlockType, Plan};
use slash::SlashAction;
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
    /// 用户在标题处输入了新文件名（不含 `.md`）：真正的重命名由工作区做，文件树与路径要一起变
    RenameRequested(String),
    /// 异步保存成功落盘；工作区据此增量更新索引
    Saved,
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

/// `/` 菜单态（6 / 7.1）：`/query` 字面留在文档里，确认时删掉再转换当前块；Esc 只关菜单、保留字符。
struct SlashMenu {
    /// `/` 在文档中的偏移
    anchor: usize,
    /// 所在编辑区间的起点：删命令时最多把它前面的空格吃到这里
    block_start: usize,
    query: String,
    /// 过滤后的条目（`slash::ITEMS` 下标）
    matches: Vec<usize>,
    selected: usize,
}

/// 标题编辑态（5.1）：单行输入框替换标题；⏎ 提交、Esc / 失焦取消。
struct TitleEdit {
    input: Entity<InputState>,
    _subscription: Subscription,
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
    preview: Entity<preview::PreviewState>,
    _preview_subscription: Subscription,
    focused: Option<Focused>,
    title_edit: Option<TitleEdit>,
    slash: Option<SlashMenu>,
    focus_handle: FocusHandle,
    scroll: ScrollHandle,
    /// 搜索命中定位：下一帧把这个块滚到视口上三分之一处
    reveal: Option<BlockId>,
    reveal_generation: u64,
    /// 短暂高亮的块（定位后 1.5 s）
    highlight: Option<BlockId>,
    highlight_task: Option<Task<()>>,
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
        let preview = cx.new(|_| preview::PreviewState::default());
        let preview_subscription = cx.observe(&preview, |_, _, cx| cx.notify());
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
            preview,
            _preview_subscription: preview_subscription,
            focused: None,
            title_edit: None,
            slash: None,
            focus_handle: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            reveal: None,
            reveal_generation: 0,
            highlight: None,
            highlight_task: None,
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

    /// 文件已被工作区重命名 / 移动：换掉路径，内容与脏状态不变。
    pub fn set_path(&mut self, path: VaultPath, cx: &mut Context<Self>) {
        self.path = path;
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// 标题：front matter 的 `title`，没有则是文件名（5.1）。
    pub fn title(&self) -> String {
        self.front_matter_title().map(|(_, t)| t).unwrap_or_else(|| self.path.stem().to_owned())
    }

    /// front matter 里 `title:` 值在文档中的区间与文本。
    fn front_matter_title(&self) -> Option<(Range<usize>, String)> {
        self.doc.front_matter_title()
    }

    // ---- 标题编辑（5.1） ----

    /// 点击标题：换成单行输入框，全选当前标题。
    fn start_title_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.exit_edit(cx);
        let title = self.title();
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("未命名"));
        let subscription = cx.subscribe_in(&input, window, Self::on_title_input_event);
        input.update(cx, |st, cx| {
            st.set_value(title.clone(), window, cx);
            st.set_selected_range(0..title.len(), cx);
            st.focus(window, cx);
        });
        self.title_edit = Some(TitleEdit { input, _subscription: subscription });
        cx.notify();
    }

    fn on_title_input_event(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { .. } => self.commit_title(window, cx),
            InputEvent::Blur => self.cancel_title_edit(false, window, cx),
            InputEvent::Change | InputEvent::Focus => {}
        }
    }

    fn cancel_title_edit(&mut self, refocus: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.title_edit.take().is_some() {
            if refocus {
                self.focus_handle.focus(window, cx);
            }
            cx.notify();
        }
    }

    /// 提交标题：有 front matter `title` 时改它；否则请工作区重命名文件。
    fn commit_title(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.title_edit.take() else { return };
        let new = edit.input.read(cx).value().trim().to_owned();
        self.focus_handle.focus(window, cx);
        cx.notify();
        if new.is_empty() || new == self.title() {
            return;
        }
        match self.front_matter_title() {
            Some((range, _)) => {
                // 值里有 YAML 特殊字符时加引号
                let needs_quote = new.contains([':', '#', '"', '\''])
                    || new.starts_with(['[', '{', '&', '*', '!', '|', '>', '%', '@', '`']);
                let text = if needs_quote {
                    format!("\"{}\"", new.replace('\\', "\\\\").replace('"', "\\\""))
                } else {
                    new
                };
                // 引号内的区间已去掉引号；带引号写回时把原引号一并替换
                let (range, text) =
                    match self.doc.slice(range.start.saturating_sub(1)..range.start).to_string().as_str() {
                        "\"" | "'" => (range.start - 1..range.end + 1, text),
                        _ => (range, text),
                    };
                if let Err(err) = self.doc.edit(Edit::replace(range, text)) {
                    log::error!("title edit rejected: {err}");
                    return;
                }
                self.last_edit_at = None;
                self.after_doc_change(cx);
            }
            None => cx.emit(EditorEvent::RenameRequested(new)),
        }
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

    /// 给索引用的快照：路径、源文本、磁盘指纹。
    pub fn index_snapshot(&self) -> (VaultPath, String, FileStamp) {
        (self.path.clone(), self.doc.to_string(), self.stamp)
    }

    /// 搜索命中定位：第 `ord` 个可索引块（`aero_core::index::indexable_leaves` 的下标）滚到可见位置并短暂高亮。
    /// 索引可能落后于文档，`ord` 越界时定位到最后一个块。
    pub fn reveal_indexed_block(&mut self, ord: u32, cx: &mut Context<Self>) {
        let leaves = aero_core::index::indexable_leaves(&self.doc);
        let Some(block) = leaves.get(ord as usize).or(leaves.last()) else { return };
        let range = self.doc.content_range(block);
        let id = self
            .units
            .iter()
            .find(|unit| unit.range.start <= range.start && range.end <= unit.range.end)
            .map_or(block.id, |unit| unit.id);
        self.reveal_generation += 1;
        self.reveal = Some(id);
        self.highlight = Some(id);
        self.highlight_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(1500)).await;
            let _ = this.update(cx, |this, cx| {
                if this.highlight == Some(id) {
                    this.highlight = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    #[cfg(test)]
    pub fn highlighted_block(&self) -> Option<BlockId> {
        self.highlight
    }

    /// 定位块的滚动：在块的子元素布局完成后算出它相对滚动容器的位置，把它放到视口上 1/3 处。
    /// 改 offset 在本帧已经来不及生效，所以顺手 `notify` 再画一帧。
    fn reveal_listener(
        &self,
        id: BlockId,
        cx: &Context<Self>,
    ) -> impl Fn(Vec<Bounds<Pixels>>, &mut Window, &mut App) + 'static {
        let scroll = self.scroll.clone();
        let this = cx.weak_entity();
        move |bounds, _window, cx| {
            let Some(target) = bounds.first() else { return };
            let container = scroll.bounds();
            if container.size.height <= px(0.) {
                return;
            }
            let desired_top = container.origin.y + container.size.height / 3.;
            let delta = target.origin.y - desired_top;
            let offset = scroll.offset();
            scroll.set_offset(point(offset.x, (offset.y - delta).min(px(0.))));
            let _ = this.update(cx, |this, cx| {
                if this.reveal == Some(id) {
                    this.reveal = None;
                    cx.notify();
                }
            });
        }
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
    fn focus_block_at(
        &mut self,
        id: BlockId,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
                    PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position,
                        modifiers,
                        click_count: 1,
                    }),
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
        self.slash = None;
        if self.focused.take().is_some() {
            self.last_edit_at = None;
            self.save_now(cx);
            cx.emit(EditorEvent::Changed);
            cx.notify();
        }
    }

    fn blur_to_browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.title_edit.is_some() {
            self.cancel_title_edit(true, window, cx);
            return;
        }
        if self.slash.take().is_some() {
            cx.notify();
            return;
        }
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
        self.update_slash(cx);
    }

    // ---- `/` 菜单（7.1） ----

    /// 每次控件文本变化后：光标前有 `/query` 就开着菜单并实时过滤，否则关掉。
    fn update_slash(&mut self, cx: &mut Context<Self>) {
        let Some((text, cursor)) = self.textarea_text_and_cursor(cx) else { return };
        let Some(f) = self.focused.as_ref() else { return };
        // 代码块等源码单位里 `/` 只是字符
        if f.enter_newline {
            return;
        }
        let next = slash::trigger(&text, cursor).and_then(|(at, query)| {
            let matches = slash::filter(query);
            (!matches.is_empty()).then(|| (f.range.start + at, query.to_owned(), matches))
        });
        let block_start = f.range.start;
        match next {
            Some((anchor, query, matches)) => {
                let selected = match self.slash.as_ref() {
                    Some(m) if m.anchor == anchor && m.query == query => m.selected.min(matches.len() - 1),
                    _ => 0,
                };
                self.slash = Some(SlashMenu { anchor, block_start, query, matches, selected });
            }
            None => self.slash = None,
        }
        cx.notify();
    }

    fn slash_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(m) = self.slash.as_mut() {
            let n = m.matches.len() as isize;
            m.selected = ((m.selected as isize + delta).rem_euclid(n)) as usize;
            cx.notify();
        }
    }

    /// 确认菜单条目：删掉 `/query`，再把所在块转换成目标类型（或插入日期）。两步合成一个撤销组。
    fn slash_confirm(&mut self, index: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.slash.take() else { return };
        let Some(&item_ix) = index.or(Some(menu.selected)).and_then(|i| menu.matches.get(i)) else { return };
        let action = slash::ITEMS[item_ix].action;
        // 命令前面的空格（"hello /h" 里的那个）一起删掉，但不越过编辑区间起点
        let mut start = menu.anchor;
        while start > menu.block_start && self.doc.slice(start - 1..start) == " " {
            start -= 1;
        }
        let command = start..menu.anchor + 1 + menu.query.len();
        match action {
            SlashAction::InsertDate => {
                let date = chrono::Local::now().format("%Y-%m-%d").to_string();
                let end = command.start + date.len();
                self.structural_edit(Edit::replace(command, date), end, window, cx);
            }
            SlashAction::Convert(target) => {
                if let Err(err) = self.doc.edit(Edit::delete(command.clone())) {
                    log::error!("edit rejected: {err}");
                    return;
                }
                self.focused = None;
                self.after_doc_change(cx);
                self.convert_at(command.start, target, true, window, cx);
            }
        }
    }

    /// 把 `offset` 所在的单位转成 `target`；那里没有块时写出骨架。`coalesce` 时并入上一个撤销组。
    fn convert_at(
        &mut self,
        offset: usize,
        target: BlockType,
        coalesce: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 落在单位上、或只隔着同一行的空白（"hello |" 里的光标）都算那个单位
        let hit = unit_at(&self.units, offset).or_else(|| {
            let ix = self.units.iter().rposition(|u| u.range.end <= offset)?;
            let gap = self.doc.slice(self.units[ix].range.end..offset).to_string();
            gap.chars().all(|c| c == ' ' || c == '\t').then_some(ix)
        });
        let plan = match hit {
            Some(ix) => ops::convert(&self.doc, &self.units, ix, target),
            None => Some(ops::insert_block(offset, target)),
        };
        let Some(plan) = plan else {
            self.focus_at(offset, None, window, cx);
            return;
        };
        let result = if coalesce { self.doc.edit_coalescing(plan.edit) } else { self.doc.edit(plan.edit) };
        if let Err(err) = result {
            log::error!("edit rejected: {err}");
            return;
        }
        self.focused = None;
        self.after_doc_change(cx);
        self.focus_at(plan.focus_offset, None, window, cx);
    }

    /// `⌘⌥0–8`：转换当前块类型。
    fn on_convert_block(&mut self, action: &ConvertBlock, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = BlockType::from_shortcut(action.0) else { return };
        let Some(f) = self.focused.as_ref() else {
            cx.propagate();
            return;
        };
        let offset = f.range.start;
        self.slash = None;
        self.convert_at(offset, target, false, window, cx);
    }

    fn on_menu_confirm(&mut self, _: &MenuConfirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.slash.is_some() {
            self.slash_confirm(None, window, cx);
        } else {
            cx.propagate();
        }
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
        if self.slash.is_some() {
            self.slash_confirm(None, window, cx);
            return;
        }
        let Some((_, cursor)) = self.textarea_text_and_cursor(cx) else {
            cx.propagate();
            return;
        };
        let Some(f) = self.focused.as_ref() else {
            cx.propagate();
            return;
        };
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
        if self.slash.is_some() {
            self.slash_move(-1, cx);
            return;
        }
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
        if self.slash.is_some() {
            self.slash_move(1, cx);
            return;
        }
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
        if self.title_edit.is_some() {
            cx.propagate();
            return;
        }
        if let Some(affected) = self.doc.undo() {
            self.after_history(affected, window, cx);
        }
    }

    fn on_redo(&mut self, _: &DocRedo, window: &mut Window, cx: &mut Context<Self>) {
        if self.title_edit.is_some() {
            cx.propagate();
            return;
        }
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
                            cx.emit(EditorEvent::Saved);
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
            Some((disk, _)) => {
                Some(self.vault.write_bytes(&self.conflict_path(), &disk).map(|_| self.conflict_path())?)
            }
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
        self.preview.update(cx, |state, _| *state = preview::PreviewState::default());
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
                log::warn!(
                    "{}: changed on disk while dirty; disk version saved as {}",
                    self.path.as_str(),
                    conflict.as_str()
                );
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

    /// 文档标题（5.1）：浏览时是大字，点击变成单行输入框。
    fn render_title(&self, cx: &mut Context<Self>) -> AnyElement {
        let base = div()
            .pb_4()
            .mb_2()
            .text_size(px(28.))
            .font_weight(FontWeight::SEMIBOLD)
            .line_height(relative(1.3));
        match &self.title_edit {
            Some(edit) => base.w_full().child(Input::new(&edit.input)).into_any_element(),
            None => base
                .id("title")
                .cursor_text()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| this.start_title_edit(window, cx)),
                )
                .child(self.title())
                .into_any_element(),
        }
    }

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
            table: StyleRefinement {
                overflow: PointRefinement { x: Some(Overflow::Scroll), y: None },
                ..Default::default()
            },
            table_head: StyleRefinement::default().font_weight(FontWeight::SEMIBOLD),
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
        if self.slash.is_some() {
            el = el.child(self.render_slash_menu(cx));
        }
        el.into_any_element()
    }

    /// `/` 菜单浮层：锚在光标下一行，越出窗口时贴边。
    fn render_slash_menu(&self, cx: &Context<Self>) -> AnyElement {
        let (Some(menu), Some(f)) = (self.slash.as_ref(), self.focused.as_ref()) else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let ta = f.textarea.read(cx);
        let position = match ta.cursor_layout() {
            Some((bounds, line_height)) => bounds.origin + point(px(0.), line_height + px(4.)),
            None => {
                let b = ta.input_bounds();
                b.origin + point(px(0.), b.size.height + px(4.))
            }
        };
        let rows: Vec<AnyElement> = menu
            .matches
            .iter()
            .enumerate()
            .map(|(i, &item_ix)| {
                let item = &slash::ITEMS[item_ix];
                let active = i == menu.selected;
                h_flex()
                    .id(("slash-item", i))
                    .w_full()
                    .h(px(28.))
                    .px_2()
                    .rounded(px(4.))
                    .items_center()
                    .justify_between()
                    .when(active, |el| el.bg(theme.list_active))
                    .hover(|el| el.bg(theme.list_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.slash_confirm(Some(i), window, cx);
                        }),
                    )
                    .child(div().text_size(px(13.)).child(item.label))
                    .child(div().text_size(px(11.)).text_color(theme.muted_foreground).child(item.hint))
                    .into_any_element()
            })
            .collect();
        deferred(
            anchored().position(position).snap_to_window_with_margin(px(8.)).child(
                v_flex()
                    .id("slash-menu")
                    .occlude()
                    .w(px(240.))
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .p_1()
                    .gap(px(1.))
                    .bg(theme.popover)
                    .text_color(theme.popover_foreground)
                    .border_1()
                    .border_color(theme.border)
                    .rounded(px(8.))
                    .shadow_lg()
                    .children(rows),
            ),
        )
        .into_any_element()
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
                let reveal = if self.highlight == Some(id) { self.reveal_generation } else { 0 };
                preview::markdown(view_id, text, self.text_view_style(cx), reveal, self.preview.clone())
                    .into_any_element()
            }
        };
        let is_heading = matches!(b.kind, BlockKind::Heading { .. });
        let highlighted = self.highlight == Some(id);
        let accent = theme.accent;
        // 定位中的块：多套一层拿子元素布局（`on_children_prepainted` 只在无状态 div 上有）
        let body: AnyElement = if self.reveal == Some(id) {
            div().w_full().on_children_prepainted(self.reveal_listener(id, cx)).child(body).into_any_element()
        } else {
            body
        };
        Some(
            div()
                .id(("unit", id.0))
                .debug_selector(|| format!("unit-{}", id.0))
                .w_full()
                .min_h(px(BODY_FONT_SIZE * BODY_LINE_HEIGHT))
                .when(is_heading, |el| el.pt_2())
                .cursor_text()
                .when(highlighted, |el| el.bg(accent).rounded(px(6.)).mx(px(-8.)).px(px(8.)))
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
            .on_action(cx.listener(Self::on_convert_block))
            .on_action(cx.listener(Self::on_menu_confirm))
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
                    .child(self.render_title(cx))
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
