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

mod backlinks_ui;
mod block_menu;
mod block_refs_ui;
mod color_menu;
mod colors_ui;
mod embeds_ui;
mod highlight_ui;
mod images_ui;
mod inline_format_ui;
mod inline_links_ui;
#[cfg(test)]
mod layout_tests;
mod link_preview;
mod math_ui;
mod ops;
mod pairs;
mod pairs_ui;
mod pdf_reader;
mod presenter;
mod preview;
mod rich_ui;
mod slash;
mod table;
mod tags;
mod tags_ui;
#[cfg(test)]
mod tests;
mod text_ops;
mod toolbar;
mod units;
mod wiki;
mod wiki_ui;

use std::cell::Cell;
use std::collections::HashSet;
use std::ops::Range;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aero_core::{Block, BlockId, BlockKind, Document, Edit, FileStamp, Vault, VaultError, VaultPath};
use gpui_kit::base::input::{
    Backspace, Delete, Escape, Input, InputEvent, InputState, Textarea, TextareaState,
};
use gpui_kit::base::text::TextViewStyle;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use images_ui::ImageViewer;

pub use link_preview::set_preview_mode;

/// 「聚焦时显示源码」（9.1）。关掉时聚焦块隐藏标记；工作区从 `state.toml` 写入。
#[derive(Clone, Copy, Debug, Default)]
pub struct FocusSource(pub bool);

impl gpui_kit::Global for FocusSource {}

pub fn focus_shows_source(cx: &App) -> bool {
    cx.try_global::<FocusSource>().map(|mode| mode.0).unwrap_or(false)
}

pub fn set_focus_shows_source(on: bool, cx: &mut App) {
    cx.set_global(FocusSource(on));
}

use crate::actions::{
    BlockDown, BlockUp, BlurEditor, ConvertBlock, CopySelectedBlocks, CutSelectedBlocks,
    DeleteSelectedBlocks, DocRedo, DocUndo, EditLink, EditSelectedBlock, ExtendBlockDown, ExtendBlockUp,
    FormatInline, LinkAlias, MenuConfirm, SelectBlockDown, SelectBlockUp, SplitBlock,
};
use crate::colors::Paint;
use ops::{BlockType, Plan};
use slash::SlashAction;
use text_ops::*;
use units::{Unit, unit_at};

/// 自动保存的静默间隔（`ui_design.md` 10.1）。
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
/// 相邻输入并入同一个 undo 组的时间窗口（方案 9.5）。
const COALESCE_WINDOW: Duration = Duration::from_millis(600);
/// 轮询磁盘文件 mtime/size 的兜底间隔（主要靠 vault 监听推着 `check_external`；轮询只做一次 stat，够便宜）。
const EXTERNAL_POLL: Duration = Duration::from_secs(2);
/// "已重新载入"横幅自动消失的时间（`ui_design.md` 10.2）
const RELOADED_BANNER: Duration = Duration::from_secs(10);

pub const BODY_FONT_SIZE: f32 = 16.;
pub const BODY_LINE_HEIGHT: f32 = 1.7;
/// 正文列宽（`ui_design.md` 2.1）
const COLUMN_WIDTH: f32 = 720.;
/// 块 gutter（`⠿ +`，5.3）占的宽度，留在正文列左侧的内边距里
const GUTTER: f32 = 48.;
/// 列表标记列（24）+ 间距（4）：嵌套一层，正文右移这么多
const LIST_INDENT: f32 = 28.;

pub enum EditorEvent {
    /// 内容、光标或保存状态变化，状态栏据此刷新
    Changed,
    SaveFailed(String),
    /// 磁盘内容被别的程序改了，而这里没有未保存修改：已重载（方案 12）
    Reloaded,
    /// 编辑器在自己的路径之外还动了这些文件（冲突副本、删掉的副本、另存为）：文件树重扫、索引对齐
    PathsTouched(Vec<VaultPath>),
    /// 文件已在外部被删除：编辑器停止自动保存并显示横幅；工作区让文件树与索引跟上
    Deleted,
    /// 用户在标题处输入了新文件名（不含 `.md`）：真正的重命名由工作区做，文件树与路径要一起变
    RenameRequested(String),
    RetitleRequested(String),
    /// 横幅上的"打开副本"
    OpenRequested(VaultPath),
    /// 预览里点了 wikilink：已解码的目标与可选片段（`target` 为空 = 同篇锚点 `[[#h]]`），工作区负责解析与打开
    LinkClicked {
        target: String,
        fragment: Option<String>,
    },
    /// 横幅上的"关闭"：工作区丢掉这个编辑器
    CloseRequested,
    /// 工作区提供本次菜单的笔记目录；返回时按 generation 丢弃旧请求。
    LinkCompletionRequested(u64),
    TagCompletionRequested(u64),
    BacklinksRequested,
    /// 异步保存成功落盘；工作区据此增量更新索引
    Saved,
}

impl EventEmitter<EditorEvent> for Editor {}

/// 文档顶部的非模态横幅（`ui_design.md` 10.2）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Banner {
    /// 蓝：磁盘版本已重新载入（无未保存修改时不做合并，直接换），10 s 自动消失
    Reloaded,
    /// 黄：外部修改与未保存修改冲突，磁盘版本已另存为副本；本地版本已照常落盘
    Conflict { copy: VaultPath },
    /// 红：文件已在外部被删除；自动保存停止，内容保留
    Deleted,
}

/// 横幅上的一个按钮：元素 id、文字、点击时做的事。
type BannerAction = (&'static str, &'static str, Box<dyn Fn(&mut Editor, &mut Window, &mut Context<Editor>)>);

/// 磁盘上这篇文件相对编辑器所知指纹的状态。
enum Disk {
    Unchanged,
    Changed(Vec<u8>, FileStamp),
    Missing,
}

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
    /// 过滤后的候选（固定条目或 7.7 的颜色）
    matches: Vec<slash::Match>,
    selected: usize,
}

/// 标题编辑态（5.1）：单行输入框替换标题；⏎ 提交、Esc / 失焦取消。
struct TitleEdit {
    input: Entity<InputState>,
    _subscription: Subscription,
}

/// 块选中态（`ui_design.md` 6）：连续几个编辑单位被整块选中，没有文本光标。
/// 以单位下标记：`anchor` 是起点，`head` 随 ↑↓ 移动（⇧↑↓ 只动 `head`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockSelection {
    anchor: usize,
    head: usize,
}

impl BlockSelection {
    fn first(&self) -> usize {
        self.anchor.min(self.head)
    }

    fn last(&self) -> usize {
        self.anchor.max(self.head)
    }

    fn contains(&self, ix: usize) -> bool {
        (self.first()..=self.last()).contains(&ix)
    }
}

/// 定位某个块之后怎么滚：搜索命中放到视口上 1/3；键盘移动块选区只保证看得见。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RevealMode {
    Center,
    Visible,
}

struct Focused {
    /// 控件文本对应的文档区间，随编辑漂移
    range: Range<usize>,
    /// 区间起点上的编辑单位（渲染时决定把控件画在哪个块的位置）；没有块时为 `None`
    id: Option<BlockId>,
    /// Enter 直接换行（代码块等）还是拆块
    enter_newline: bool,
    /// 公式源码在弹窗里改，文档流里仍是渲染结果。
    math_popup: bool,
    textarea: Entity<TextareaState>,
    pairs: pairs::State,
    highlight: highlight_ui::SourceHighlight,
    _pair_keys: Subscription,
    _subscription: Subscription,
    _cursor_subscription: Subscription,
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
    link_preview: link_preview::LinkPreview,
    backlinks: Option<Entity<crate::backlinks::BacklinksView>>,
    backlinks_subscription: Option<Subscription>,
    /// 嵌入内容缓存（`![[笔记]]` / `#章节` / `^块`）
    embeds: embeds_ui::Embeds,
    /// 重内容渲染服务（9.9）；没连上时图片按源码显示
    images: Option<Entity<crate::render::RenderService>>,
    images_subscription: Option<Subscription>,
    rich: rich_ui::RichUi,
    /// 全屏看图：双击图片打开，`Space` / `Esc` / 点击关闭
    viewer: Option<ImageViewer>,
    /// 上一帧的 HiDPI 倍率：图片 / 公式按它算设备像素（渲染块时拿不到 `Window`）
    scale: f32,
    focused: Option<Focused>,
    /// 块选中态；与 `focused` 互斥
    selection: Option<BlockSelection>,
    picker_open: bool,
    title_edit: Option<TitleEdit>,
    slash: Option<SlashMenu>,
    /// 颜色菜单（7.7）；开着时焦点在它的过滤输入框上
    color_menu: Option<colors_ui::ColorMenu>,
    /// 块菜单（5.3）
    block_menu: Option<block_menu::BlockMenu>,
    /// 拖拽块时的插入线
    block_drop: Option<block_menu::BlockDropHint>,
    dragging_block: bool,
    unit_bounds: Vec<(BlockId, Bounds<Pixels>)>,
    /// 上一帧行内公式的点击区域（块、源码区间、窗口坐标）。
    inline_math_hits: Vec<(BlockId, Range<usize>, Bounds<Pixels>)>,
    /// 这一篇暂时整篇显示源码（9.1「查看源码」）。不写入笔记。
    note_source: bool,
    /// 单独打开源码的块。
    source_blocks: HashSet<BlockId>,
    /// 文字类块的控件，浏览与编辑共用（见 `presenter`）
    presenters: std::collections::HashMap<presenter::Key, presenter::Presenter>,
    /// 在没编辑的文字里按下的链接，松开时还在它上面才打开
    pending_link: Option<(presenter::Key, String)>,
    hovered_link: Option<(presenter::Key, SharedString, Bounds<Pixels>)>,
    /// 可折叠 callout 的展开状态（块 id → 是否展开），只在界面上，不写进笔记
    callout_folds: std::collections::HashMap<BlockId, bool>,
    /// 上一帧每张表表头各格的矩形（拖列宽、冻结列宽用）
    table_layout: Rc<std::cell::RefCell<std::collections::HashMap<BlockId, Vec<Bounds<Pixels>>>>>,
    table_resize: Option<table::ResizeState>,
    /// 正在编辑的那张表的列宽比例：打字时列宽不跟着内容伸缩
    table_frozen: Option<(BlockId, Vec<f32>)>,
    /// 选区浮动工具条（5.9）与它的延迟出现计时
    toolbar: Option<toolbar::Toolbar>,
    toolbar_task: Option<Task<()>>,
    /// 块选中态里"选区头"那一块的矩形，每帧更新；颜色菜单靠它定位
    head_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    wiki: Option<wiki::Menu>,
    wiki_generation: u64,
    tag_menu: Option<tags::Menu>,
    tag_generation: u64,
    focus_handle: FocusHandle,
    scroll: ScrollHandle,
    /// 下一帧把这个块滚到合适位置（搜索命中定位、块选区移动）
    reveal: Option<BlockId>,
    reveal_mode: RevealMode,
    reveal_generation: u64,
    /// 短暂高亮的块（定位后 1.5 s）
    highlight: Option<BlockId>,
    highlight_task: Option<Task<()>>,
    banner: Option<Banner>,
    banner_task: Option<Task<()>>,
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
            link_preview: link_preview::LinkPreview::default(),
            backlinks: None,
            backlinks_subscription: None,
            embeds: embeds_ui::Embeds::default(),
            images: None,
            images_subscription: None,
            rich: Default::default(),
            viewer: None,
            scale: 1.,
            focused: None,
            selection: None,
            picker_open: false,
            title_edit: None,
            slash: None,
            color_menu: None,
            block_menu: None,
            block_drop: None,
            dragging_block: false,
            unit_bounds: Vec::new(),
            inline_math_hits: Vec::new(),
            note_source: false,
            source_blocks: HashSet::new(),
            presenters: Default::default(),
            pending_link: None,
            hovered_link: None,
            callout_folds: Default::default(),
            table_layout: Rc::default(),
            table_resize: None,
            table_frozen: None,
            toolbar: None,
            toolbar_task: None,
            head_bounds: Rc::default(),
            wiki: None,
            wiki_generation: 0,
            tag_menu: None,
            tag_generation: 0,
            focus_handle: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            reveal: None,
            reveal_mode: RevealMode::Center,
            reveal_generation: 0,
            highlight: None,
            highlight_task: None,
            banner: None,
            banner_task: None,
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

    pub fn set_picker_open(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.picker_open = open;
        if open {
            self.cancel_link_preview(cx);
            self.slash = None;
            self.color_menu = None;
            self.hide_toolbar(cx);
            self.wiki = None;
            self.tag_menu = None;
        } else {
            // 同帧恢复再转移焦点可能不触发 Blur，下一帧按命令执行后的实际焦点收尾。
            // 命令自己打开了弹层（颜色菜单）时焦点在弹层上，不算失焦。
            cx.on_next_frame(window, |this, window, cx| {
                if !this.picker_open
                    && this.color_menu.is_none()
                    && this
                        .focused
                        .as_ref()
                        .is_some_and(|focused| !focused.textarea.focus_handle(cx).is_focused(window))
                {
                    this.exit_edit(cx);
                }
            });
        }
    }

    pub fn command_available(&self, action: &dyn Action) -> bool {
        if action.as_any().is::<DocUndo>() {
            self.doc.can_undo()
        } else if action.as_any().is::<DocRedo>() {
            self.doc.can_redo()
        } else if action.as_any().is::<EditLink>() || action.as_any().is::<FormatInline>() {
            self.focused.is_some() && self.title_edit.is_none()
        } else if let Some(action) = action.as_any().downcast_ref::<ConvertBlock>() {
            let Some(focused) = &self.focused else { return false };
            let Some(target) = BlockType::from_shortcut(action.0) else { return false };
            unit_at(&self.units, focused.range.start).is_none_or(|ix| {
                ops::block_type(&self.units[ix])
                    .is_some_and(|current| current != BlockType::Quote && current != target)
            })
        } else {
            false
        }
    }

    /// 文件已被工作区重命名 / 移动：换掉路径，内容与脏状态不变。
    pub fn set_path(&mut self, path: VaultPath, cx: &mut Context<Self>) {
        self.wiki = None;
        self.tag_menu = None;
        self.rich.cancel_export();
        let old_path = std::mem::replace(&mut self.path, path);
        self.repath_link_preview(&old_path, cx);
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
        self.selection = None;
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
        if self.front_matter_title().is_some() {
            cx.emit(EditorEvent::RetitleRequested(new));
        } else {
            cx.emit(EditorEvent::RenameRequested(new));
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
        self.reveal_block(id, cx);
    }

    /// 反链来源定位：版本已经变化时拒绝使用旧位置。
    pub fn reveal_backlink(&mut self, hit: &aero_index::Backlink, cx: &mut Context<Self>) -> bool {
        if self.path != hit.path
            || self.stamp.hash != hit.source_hash
            || matches!(self.save_state, SaveState::Dirty | SaveState::Saving | SaveState::Failed(_))
        {
            return false;
        }
        let Some(unit) = self
            .units
            .iter()
            .find(|unit| unit.range.start <= hit.range.start && hit.range.end <= unit.range.end)
        else {
            return false;
        };
        self.reveal_block(unit.id, cx);
        true
    }

    /// wikilink 标题片段跳转：按标题文本定位（与右侧大纲同源），找不到返回 false、什么都不动。
    /// 比对去掉行内标记（`**`、`` ` ``、`[[ ]]` 等）、忽略大小写与首尾空白；先找完全相等的，再退到去标记后相等的。
    pub fn reveal_heading(&mut self, heading: &str, cx: &mut Context<Self>) -> bool {
        let headings = self.headings();
        let wanted = heading.trim();
        let wanted_key = heading_key(wanted);
        let exact = headings.iter().find(|(_, text, _)| text == wanted);
        let Some((_, _, id)) =
            exact.or_else(|| headings.iter().find(|(_, text, _)| heading_key(text) == wanted_key))
        else {
            return false;
        };
        // 引用 / callout 的标题没有独立的编辑单位，定位其容器并展开折叠预览。
        let range =
            self.doc.blocks().iter().find(|(_, b)| b.id == *id).map(|(_, b)| self.doc.content_range(b));
        let id = range
            .and_then(|range| {
                self.units.iter().find(|u| u.range.start <= range.start && range.end <= u.range.end)
            })
            .map_or(*id, |u| u.id);
        self.reveal_block(id, cx);
        true
    }

    /// 标题与显式块 ID 使用不同命名空间；重复 ID 拒绝猜测。
    pub fn reveal_fragment(&mut self, fragment: &str, cx: &mut Context<Self>) -> Result<(), String> {
        if let Some(id) = fragment.strip_prefix('^') {
            let id = aero_core::block_refs::resolve(&self.doc, id)?;
            let b = self.doc.blocks().iter().find(|(_, b)| b.id == id).unwrap().1;
            let range = self.doc.content_range(b);
            let unit = self
                .units
                .iter()
                .find(|u| u.range.start <= range.start && range.end <= u.range.end)
                .or_else(|| {
                    self.units.iter().find(|u| range.start <= u.range.start && u.range.end <= range.end)
                })
                .ok_or("此块没有可显示的内容")?;
            self.reveal_block(unit.id, cx);
            Ok(())
        } else if self.reveal_heading(fragment, cx) {
            Ok(())
        } else {
            Err(format!("没有标题「{fragment}」"))
        }
    }

    /// 滚到块可见位置（视口上 1/3）并短暂高亮。
    fn reveal_block(&mut self, id: BlockId, cx: &mut Context<Self>) {
        // 定位到折叠着的 callout：展开它，高亮褪去后也保持展开
        if self.doc.blocks().find(id).is_some_and(|b| matches!(b.kind, BlockKind::Callout { .. })) {
            self.callout_folds.insert(id, true);
        }
        self.reveal_generation += 1;
        self.reveal = Some(id);
        self.reveal_mode = RevealMode::Center;
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

    /// 松手落位后高亮 300 ms（`ui_design.md` 6 拖拽态）。
    pub(super) fn pulse_block(&mut self, id: BlockId, cx: &mut Context<Self>) {
        self.highlight = Some(id);
        self.highlight_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(300)).await;
            let _ = this.update(cx, |this, cx| {
                if this.highlight == Some(id) {
                    this.highlight = None;
                    cx.notify();
                }
            });
        }));
    }

    /// 定位块的滚动：在块的子元素布局完成后算出它相对滚动容器的位置——`Center` 放到视口上 1/3 处，
    /// `Visible` 只在块超出视口时把它挪进来。改 offset 在本帧已经来不及生效，所以顺手 `notify` 再画一帧。
    fn reveal_listener(
        &self,
        id: BlockId,
        cx: &Context<Self>,
    ) -> impl Fn(Vec<Bounds<Pixels>>, &mut Window, &mut App) + 'static {
        let scroll = self.scroll.clone();
        let mode = self.reveal_mode;
        let this = cx.weak_entity();
        move |bounds, _window, cx| {
            let Some(target) = bounds.first() else { return };
            let container = scroll.bounds();
            if container.size.height <= px(0.) {
                return;
            }
            let delta = match mode {
                RevealMode::Center => target.origin.y - (container.origin.y + container.size.height / 3.),
                RevealMode::Visible => {
                    let margin = px(16.);
                    if target.origin.y < container.origin.y + margin {
                        target.origin.y - (container.origin.y + margin)
                    } else if target.bottom_right().y > container.bottom_right().y - margin {
                        (target.bottom_right().y - (container.bottom_right().y - margin))
                            .min(target.origin.y - (container.origin.y + margin))
                    } else {
                        px(0.)
                    }
                }
            };
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
        aero_core::headings::list(&self.doc).into_iter().map(|h| (h.level, h.text, h.id)).collect()
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

    fn inline_formula_at_position(&self, id: BlockId, position: Point<Pixels>) -> Option<Range<usize>> {
        self.inline_math_hits.iter().find_map(|(block, range, bounds)| {
            (*block == id
                && position.x >= bounds.left()
                && position.x < bounds.right()
                && position.y >= bounds.top()
                && position.y < bounds.bottom())
            .then(|| range.clone())
        })
    }

    /// 公式源码放到弹窗里。文档里的块继续画渲染结果，高度不因为源码而变。
    fn open_math_popup(
        &mut self,
        range: Range<usize>,
        id: Option<BlockId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cursor = range.len();
        self.focus_range(range, id, true, cursor, window, cx);
        if let Some(focused) = &mut self.focused {
            focused.math_popup = true;
        }
        self.refresh_markup(cx);
        cx.notify();
    }

    /// 公式弹窗。不进文档流，所以预览块的位置和高度保持原样。
    fn render_math_editor(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let focused = self.focused.as_ref()?;
        if !focused.math_popup {
            return None;
        }
        let theme = cx.theme();
        let origin = focused.id.and_then(|id| {
            self.unit_bounds
                .iter()
                .find(|(block, _)| *block == id)
                .map(|(_, bounds)| bounds.origin + point(px(0.), bounds.size.height + px(8.)))
        });
        let origin = origin.unwrap_or_else(|| point(px(80.), px(120.)));
        let textarea = focused.textarea.clone();
        Some(
            deferred(
                anchored().position(origin).snap_to_window_with_margin(px(8.)).child(
                    v_flex()
                        .id("math-editor")
                        .debug_selector(|| "math-editor".into())
                        .occlude()
                        .w(px(440.))
                        .max_h(px(320.))
                        .p_3()
                        .gap_2()
                        .bg(theme.popover)
                        .text_color(theme.popover_foreground)
                        .border_1()
                        .border_color(theme.border)
                        .rounded(px(10.))
                        .shadow_lg()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                cx.stop_propagation();
                                if let Some(focused) = &this.focused {
                                    focused.textarea.update(cx, |textarea, cx| textarea.focus(window, cx));
                                }
                            }),
                        )
                        .child(div().text_size(px(12.)).text_color(theme.muted_foreground).child("公式"))
                        .child(
                            div()
                                .w_full()
                                .font_family(theme.mono_font_family.clone())
                                .text_size(theme.mono_font_size)
                                .line_height(relative(1.6))
                                .bg(theme.muted)
                                .text_color(theme.foreground)
                                .rounded(px(6.))
                                .p_2()
                                .child(Textarea::new(&textarea)),
                        ),
                ),
            )
            .into_any_element(),
        )
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
        if let Some(range) = self.inline_formula_at_position(id, position) {
            self.open_math_popup(range, Some(id), window, cx);
            return;
        }
        self.focus_block(id, window, cx);
        if self.focused.as_ref().is_some_and(|focused| focused.math_popup)
            || self.replaying_click
            || !self.is_focused_block(id)
        {
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
        if matches!(unit.kind, BlockKind::MathBlock) {
            let (range, id) = (unit.range.clone(), unit.id);
            self.open_math_popup(range, Some(id), window, cx);
            return;
        }
        let (range, id, nl) = (unit.range.clone(), Some(unit.id), unit.enter_inserts_newline());
        let (unit_id, kind) = (unit.id, unit.kind.clone());
        let text = self.doc.slice(range.clone()).to_string();
        // 表格和 callout 按格子 / 标题与正文分别编辑，键盘进来时落到光标所在的那一格
        if self.hides_markers(cx) && !self.block_shows_source(unit_id) {
            match kind {
                BlockKind::CodeFence { .. } if aero_core::rich_content::mermaid_source(&text).is_none() => {
                    if let Some(inner) = presenter::fence_body(&text) {
                        let local = cursor.clamp(inner.start, inner.end) - inner.start;
                        let abs = range.start + inner.start..range.start + inner.end;
                        self.focus_range(abs, id, true, local, window, cx);
                        return;
                    }
                }
                BlockKind::Table
                    if presenter::presents_table(&text)
                        && self.focus_table_at(unit_id, cursor, window, cx) =>
                {
                    return;
                }
                BlockKind::Callout { .. } => {
                    if let Some(parts) = callout_edit_parts(&text) {
                        let (part, local, nl) = if cursor <= parts.title.end || parts.body.is_empty() {
                            (parts.title.clone(), cursor.clamp(parts.title.start, parts.title.end), false)
                        } else {
                            (parts.body.clone(), cursor.clamp(parts.body.start, parts.body.end), true)
                        };
                        let abs = range.start + part.start..range.start + part.end;
                        self.focus_range(abs, id, nl, local - part.start, window, cx);
                        return;
                    }
                }
                _ => {}
            }
        }
        if self.renders_inline_formulas(unit_id, &text, cx) {
            let at = cursor.min(text.len());
            if let Some(local) = math_ui::inline_formula_containing(&text, at) {
                self.open_math_popup(range.start + local.start..range.start + local.end, id, window, cx);
                return;
            }
            if let Some(local) = math_ui::text_span_for_cursor(&text, at) {
                let abs = range.start + local.start..range.start + local.end;
                let rel = at.saturating_sub(local.start).min(abs.len());
                self.focus_range(abs, id, nl, rel, window, cx);
                return;
            }
            if let Some(formula) = aero_core::highlight::inline_formulas(&text).into_iter().next() {
                self.open_math_popup(
                    range.start + formula.range.start..range.start + formula.range.end,
                    id,
                    window,
                    cx,
                );
                return;
            }
        }
        self.focus_range(range, id, nl, cursor, window, cx);
    }

    /// 有渲染服务时，行内公式留在预览里，文本框只覆盖公式两侧的文字。
    fn renders_inline_formulas(&self, id: BlockId, text: &str, cx: &App) -> bool {
        self.images.is_some()
            && self.hides_markers(cx)
            && !self.block_shows_source(id)
            && !aero_core::highlight::inline_formulas(text).is_empty()
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
        self.cancel_link_preview(cx);
        let text = self.doc.slice(range.clone()).to_string();
        let cursor = clamp_to_char_boundary(&text, cursor.min(text.len()));
        // 这一段已经有控件在画（浏览态也是它）：接着用它，字一个像素都不动。
        let textarea = match self.presenter_for(id, &range).and_then(|key| self.presenters.get(&key)) {
            Some(p) => p.textarea.clone(),
            None => cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 100_000)),
        };
        textarea.update(cx, |ta, cx| {
            if ta.value().as_ref() != text {
                ta.set_value(text, window, cx);
            }
            ta.set_selected_range(cursor..cursor, cx);
            ta.focus(window, cx);
        });
        self.install_focus(range, id, enter_newline, textarea, window, cx);
    }

    /// 把 `textarea` 设为编辑中的那一段：订阅它的输入、光标与按键。文字与光标由调用方备好。
    pub(super) fn install_focus(
        &mut self,
        range: Range<usize>,
        id: Option<BlockId>,
        enter_newline: bool,
        textarea: Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let subscription = cx.subscribe_in(&textarea, window, Self::on_input_event);
        let editor = cx.weak_entity();
        let pair_keys = cx.intercept_keystrokes(move |event, window, cx| {
            let _ = editor.update(cx, |this, cx| this.observe_pair_key(event, window, cx));
        });
        let mut last_selection = None;
        let mut last_text: Option<SharedString> = None;
        let cursor_subscription = cx.observe_in(&textarea, window, move |this, textarea, window, cx| {
            if this.focused.as_ref().is_some_and(|f| f.textarea == textarea) {
                let selection = textarea.read(cx).selected_range();
                let value = textarea.read(cx).value();
                let typing = last_text.as_ref().is_some_and(|text| *text != value);
                last_text = Some(value);
                if last_selection.as_ref() != Some(&selection) {
                    last_selection = Some(selection.clone());
                    // 光标移进 `$…$` 时改到弹窗，段落预览保持不动。正在打字（刚打完 `$x`）时不弹，
                    // 否则闭合的 `$` 会被当成新字符。
                    if selection.is_empty()
                        && !typing
                        && this.focused.as_ref().is_some_and(|focused| !focused.math_popup)
                        && !this.note_source
                        && !this
                            .focused
                            .as_ref()
                            .and_then(|focused| focused.id)
                            .is_some_and(|id| this.source_blocks.contains(&id))
                        && let Some(local) =
                            math_ui::inline_formula_containing(&textarea.read(cx).value(), selection.start)
                    {
                        let (start, id) = {
                            let focused = this.focused.as_ref().unwrap();
                            (focused.range.start, focused.id)
                        };
                        this.open_math_popup(start + local.start..start + local.end, id, window, cx);
                        return;
                    }
                    if this.normalize_caret(&textarea, cx) {
                        return;
                    }
                    // 选区停下来 300 ms 后浮出工具条（5.9）
                    this.schedule_toolbar(cx);
                    cx.notify();
                }
            }
            if this.focused.as_ref().is_some_and(|f| f.textarea == textarea && f.pairs.pending.is_some())
                && this.input_composing(window, cx)
            {
                this.focused.as_mut().unwrap().pairs.pending = None;
            }
            if this.focused.as_ref().is_some_and(|f| f.textarea == textarea)
                && (this.wiki.is_some() || this.tag_menu.is_some())
                && let Some(f) = &this.focused
                && textarea.read(cx).value().as_ref() == this.doc.slice(f.range.clone()).to_string()
            {
                if this.wiki.is_some() && !this.wiki_is_current(cx) {
                    this.wiki = None;
                    cx.notify();
                }
                if this.tag_menu.is_some() && !this.tag_is_current(cx) {
                    this.tag_menu = None;
                    cx.notify();
                }
            }
        });
        self.last_edit_at = None;
        log::debug!("focus {:?} id={:?}", range, id);
        self.selection = None;
        self.wiki = None;
        self.tag_menu = None;
        match self.presenter_key_of(&textarea) {
            Some(presenter::Key { id, part: presenter::Part::Cell(..) }) => self.freeze_table(id),
            _ => self.table_frozen = None,
        }
        self.focused = Some(Focused {
            range,
            id,
            enter_newline,
            math_popup: false,
            textarea,
            pairs: Default::default(),
            highlight: Default::default(),
            _pair_keys: pair_keys,
            _subscription: subscription,
            _cursor_subscription: cursor_subscription,
        });
        self.sync_diagram_preview(true, cx);
        self.refresh_markup(cx);
        cx.notify();
    }

    /// 默认藏标记。偏好、整篇源码、或这一块的「编辑源码」打开时，回到现在的源码态。
    pub fn hides_markers(&self, cx: &App) -> bool {
        !self.note_source && !focus_shows_source(cx)
    }

    pub fn note_source(&self) -> bool {
        self.note_source
    }

    pub fn sync_presentation(&mut self, cx: &mut Context<Self>) {
        self.exit_edit(cx);
        self.presenters.clear();
        cx.notify();
    }

    pub fn toggle_note_source(&mut self, cx: &mut Context<Self>) {
        self.note_source = !self.note_source;
        self.sync_presentation(cx);
    }

    fn block_shows_source(&self, id: BlockId) -> bool {
        self.source_blocks.contains(&id)
    }

    pub(super) fn toggle_block_source(&mut self, id: BlockId, window: &mut Window, cx: &mut Context<Self>) {
        if self.source_blocks.contains(&id) {
            self.source_blocks.remove(&id);
            self.exit_edit(cx);
            self.focus_handle.focus(window, cx);
            cx.notify();
            return;
        }
        self.source_blocks.insert(id);
        self.exit_edit(cx);
        self.presenters.retain(|key, _| key.id != id);
        self.focus_block(id, window, cx);
    }

    fn conceal_focused(&self, cx: &App) -> bool {
        if self.focused.as_ref().is_some_and(|focused| focused.math_popup) {
            return false;
        }
        if !self.hides_markers(cx) {
            return false;
        }
        let Some(focused) = &self.focused else { return false };
        let Some(id) = focused.id else { return false };
        if self.source_blocks.contains(&id) {
            return false;
        }
        if self.editing_code() {
            return false;
        }
        if self.presenter_key_of(&focused.textarea).is_some() {
            return true;
        }
        // 夹在行内公式之间的那段文字：公式留在预览里，文字照样藏标记
        let Some(block) = self.doc.blocks().find(id) else { return false };
        if !matches!(block.kind, BlockKind::Paragraph | BlockKind::Heading { .. }) {
            return false;
        }
        let text = self.doc.slice(self.doc.content_range(block)).to_string();
        !preview::is_image_embed(text.trim()) && aero_core::rich_content::pdf_embed(&text).is_none()
    }

    fn refresh_markup(&mut self, cx: &mut Context<Self>) {
        let Some(focused) = &self.focused else { return };
        let textarea = focused.textarea.clone();
        if let Some(key) = self.presenter_key_of(&textarea) {
            self.restyle_presenter(key, cx);
            return;
        }
        let markup = if self.conceal_focused(cx) {
            let text = textarea.read(cx).value().to_string();
            let analysis = aero_core::markup::analyze(&text);
            self.presenter_markup(&text, &analysis, &presenter::Extra::default(), cx)
        } else {
            gpui_kit::base::input::Markup::default()
        };
        textarea.update(cx, |textarea, cx| textarea.set_markup(markup, cx));
    }

    /// 光标不停在藏起来的行首标记（`# `、`> `）前面，也不停在隐藏字节中间：
    /// 在那里打字会拆坏标记，让语法露出来。返回是否挪了光标。
    fn normalize_caret(&mut self, textarea: &Entity<TextareaState>, cx: &mut Context<Self>) -> bool {
        if !self.conceal_focused(cx) {
            return false;
        }
        let ta = textarea.read(cx);
        let selection = ta.selected_range();
        if !selection.is_empty() {
            return false;
        }
        let text = ta.value().to_string();
        let caret = selection.start;
        let analysis = aero_core::markup::analyze_editing(&text, Some(selection));
        let conceal: Vec<_> =
            analysis.conceal.iter().filter(|range| !analysis.hard_breaks.contains(range)).cloned().collect();
        let target = analysis
            .hard_breaks
            .iter()
            .find(|range| range.start < caret && caret < range.end)
            .map_or_else(|| text_ops::settle_caret(&conceal, &text, caret), |range| range.end);
        if target == caret {
            return false;
        }
        textarea.update(cx, |ta, cx| ta.set_selected_range(target..target, cx));
        true
    }

    /// `- ` / `1. ` 打完后，解析已经把它变成列表；把控件收成列表项正文，标记留给列表容器画。
    fn promote_typed_prefix(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.hides_markers(cx) {
            return false;
        }
        let Some(focused) = &self.focused else { return false };
        let textarea = focused.textarea.clone();
        let text = textarea.read(cx).value().to_string();
        let cursor = textarea.read(cx).selected_range().start;
        let Some(prefix) = ops::typed_prefix_len(&text, cursor) else { return false };
        let doc_pos = focused.range.start + cursor.min(text.len());
        let Some(ix) = unit_at(&self.units, doc_pos) else { return false };
        let unit_text = self.doc.slice(self.units[ix].range.clone()).to_string();
        if unit_text == text {
            return false;
        }
        let content_cursor = cursor.saturating_sub(prefix).min(unit_text.len());
        self.focus_unit(ix, content_cursor, window, cx);
        true
    }

    /// 退出编辑态（不改焦点）。失焦即落盘（10.1）。
    fn exit_edit(&mut self, cx: &mut Context<Self>) {
        self.cancel_link_preview(cx);
        self.slash = None;
        self.color_menu = None;
        self.hide_toolbar(cx);
        self.wiki = None;
        self.tag_menu = None;
        self.table_frozen = None;
        self.rich.clear_preview();
        if self.focused.take().is_some() {
            self.last_edit_at = None;
            self.save_now(cx);
            cx.emit(EditorEvent::Changed);
            cx.notify();
        }
    }

    /// 编辑态里的 Esc（文本控件的 `Escape` 冒泡到这里）：先关菜单 / 标题编辑；否则退出编辑并选中刚才的块
    /// （`ui_design.md` 6："Esc 两段式退出"）。光标在没有块的空位上时直接回浏览态。
    fn on_escape(&mut self, _: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_link_preview(cx);
        if self.close_image_viewer(window, cx) {
            return;
        }
        if self.input_composing(window, cx) {
            cx.propagate();
            return;
        }
        if self.close_color_menu(window, cx) {
            return;
        }
        if self.close_block_menu(cx) {
            return;
        }
        if self.cancel_block_drag(window, cx) {
            return;
        }
        if self.dismiss_toolbar(cx) {
            return;
        }
        if self.tag_menu.take().is_some() || self.wiki.take().is_some() {
            cx.notify();
            return;
        }
        if self.title_edit.is_some() {
            self.cancel_title_edit(true, window, cx);
            return;
        }
        if self.slash.take().is_some() {
            cx.notify();
            return;
        }
        match self.focused_unit() {
            Some(ix) => self.select_blocks(ix, ix, window, cx),
            None => self.blur_to_browse(window, cx),
        }
    }

    /// 块选中态里的 Esc（或程序化的 `BlurEditor`）：回浏览态。没什么可退的就 propagate。
    fn on_blur_editor(&mut self, _: &BlurEditor, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_link_preview(cx);
        if self.close_image_viewer(window, cx) {
            return;
        }
        if self.cancel_block_drag(window, cx) {
            return;
        }
        let busy = self.title_edit.is_some()
            || self.slash.is_some()
            || self.focused.is_some()
            || self.selection.is_some();
        if busy {
            self.blur_to_browse(window, cx);
        } else {
            cx.propagate();
        }
    }

    fn cancel_block_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.dragging_block && self.block_drop.is_none() {
            return false;
        }
        self.dragging_block = false;
        self.block_drop = None;
        cx.stop_active_drag(window);
        cx.notify();
        true
    }

    fn blur_to_browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tag_menu.take().is_some() || self.wiki.take().is_some() {
            cx.notify();
            return;
        }
        if self.title_edit.is_some() {
            self.cancel_title_edit(true, window, cx);
            return;
        }
        if self.slash.take().is_some() {
            cx.notify();
            return;
        }
        self.exit_edit(cx);
        if self.selection.take().is_some() {
            cx.notify();
        }
        self.focus_handle.focus(window, cx);
    }

    // ---- 块选中态（`ui_design.md` 6） ----

    #[cfg(test)]
    pub fn selected_blocks(&self) -> Option<Range<usize>> {
        self.selection.map(|s| s.first()..s.last() + 1)
    }

    /// 选中 `anchor..=head` 这些单位：退出编辑态（落盘），焦点回容器，`head` 那块滚到可见处。
    fn select_blocks(&mut self, anchor: usize, head: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.units.is_empty() {
            self.blur_to_browse(window, cx);
            return;
        }
        let last = self.units.len() - 1;
        let (anchor, head) = (anchor.min(last), head.min(last));
        self.slash = None;
        self.exit_edit(cx);
        self.selection = Some(BlockSelection { anchor, head });
        self.reveal = Some(self.units[head].id);
        self.reveal_mode = RevealMode::Visible;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    /// ↑↓：选区收成一块并移动；⇧↑↓：只动 `head`。到头了就停。没有选区时 propagate。
    fn move_selection(&mut self, delta: isize, extend: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sel) = self.selection else {
            cx.propagate();
            return;
        };
        let last = self.units.len().saturating_sub(1) as isize;
        let head = (sel.head as isize + delta).clamp(0, last) as usize;
        let anchor = if extend { sel.anchor } else { head };
        self.select_blocks(anchor, head, window, cx);
    }

    fn on_select_up(&mut self, _: &SelectBlockUp, window: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, false, window, cx);
    }

    fn on_select_down(&mut self, _: &SelectBlockDown, window: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, false, window, cx);
    }

    /// ⇧↑ / ⇧↓：块选中态里扩展选区；编辑态里光标已在首 / 末行时，字符选区升级为块选区（当前块 + 相邻块）。
    fn extend_selection(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_some() {
            self.move_selection(delta, true, window, cx);
            return;
        }
        let Some((text, cursor)) = self.textarea_text_and_cursor(cx) else {
            cx.propagate();
            return;
        };
        let (line, _) = line_col(&text, cursor);
        let at_edge = if delta < 0 { line == 0 } else { line + 1 == line_count(&text) };
        let Some(ix) = self.focused_unit().filter(|_| at_edge) else {
            cx.propagate();
            return;
        };
        let last = self.units.len().saturating_sub(1) as isize;
        let head = (ix as isize + delta).clamp(0, last) as usize;
        self.select_blocks(ix, head, window, cx);
    }

    fn on_extend_up(&mut self, _: &ExtendBlockUp, window: &mut Window, cx: &mut Context<Self>) {
        self.extend_selection(-1, window, cx);
    }

    fn on_extend_down(&mut self, _: &ExtendBlockDown, window: &mut Window, cx: &mut Context<Self>) {
        self.extend_selection(1, window, cx);
    }

    /// ⏎：编辑 `head` 那块，光标在块末。
    fn on_edit_selected(&mut self, _: &EditSelectedBlock, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sel) = self.selection else {
            cx.propagate();
            return;
        };
        let Some(unit) = self.units.get(sel.head) else { return };
        let len = unit.range.len();
        self.focus_unit(sel.head, len, window, cx);
    }

    /// 选中块的 Markdown 源码（整行，列表项含标记与嵌套子项）。
    fn selected_source(&self) -> Option<String> {
        let sel = self.selection?;
        let span = ops::units_span(&self.doc, &self.units, sel.first(), sel.last());
        Some(self.doc.slice(span).to_string())
    }

    /// ⌫ / ⌦：删掉选中的块，一个撤销组；选区落到原位置上的下一块（没有就上一块；文档空了回浏览态）。
    fn delete_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sel) = self.selection else { return };
        let edit = ops::delete_units(&self.doc, &self.units, sel.first(), sel.last());
        if let Err(err) = self.doc.edit(edit) {
            log::error!("edit rejected: {err}");
            return;
        }
        let first = sel.first();
        self.last_edit_at = None;
        self.after_doc_change(cx);
        if self.units.is_empty() {
            self.selection = None;
            self.focus_handle.focus(window, cx);
        } else {
            let ix = first.min(self.units.len() - 1);
            self.select_blocks(ix, ix, window, cx);
        }
    }

    fn on_delete_selected(&mut self, _: &DeleteSelectedBlocks, window: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_some() {
            self.delete_selected(window, cx);
        } else {
            cx.propagate();
        }
    }

    // ---- 块 gutter（`ui_design.md` 5.3） ----

    fn unit_index(&self, id: BlockId) -> Option<usize> {
        self.units.iter().position(|u| u.id == id)
    }

    /// gutter 的 `⠿`：进入块选中态。点击另开块菜单；拖动走 `on_drag`。
    fn select_block_by_id(&mut self, id: BlockId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.unit_index(id) {
            self.select_blocks(ix, ix, window, cx);
        }
    }

    /// gutter 的 `+`：在块下方新开段落并打开 `/` 菜单。
    fn insert_block_below(&mut self, id: BlockId, window: &mut Window, cx: &mut Context<Self>) {
        self.block_menu = None;
        let Some(ix) = self.unit_index(id) else { return };
        let plan = ops::insert_slash_below(&self.doc, &self.units, ix);
        self.apply_plan(plan, window, cx);
        self.update_slash(cx);
    }

    fn on_copy_selected(&mut self, _: &CopySelectedBlocks, _: &mut Window, cx: &mut Context<Self>) {
        match self.selected_source() {
            Some(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            None => cx.propagate(),
        }
    }

    fn on_cut_selected(&mut self, _: &CutSelectedBlocks, window: &mut Window, cx: &mut Context<Self>) {
        match self.selected_source() {
            Some(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.delete_selected(window, cx);
            }
            None => cx.propagate(),
        }
    }

    // ---- 控件事件 ----

    fn on_input_event(
        &mut self,
        textarea: &Entity<TextareaState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_current = self.focused.as_ref().is_some_and(|f| &f.textarea == textarea);
        if !is_current {
            return;
        }
        match event {
            InputEvent::Change => self.on_change(window, cx),
            // ⏎ 由 SplitBlock 动作截获（见 actions.rs）；⇧⏎ 软换行由控件自己插入，随后的 Change 已经写回
            InputEvent::PressEnter { .. } | InputEvent::Focus => {}
            // 弹层（命令面板、颜色菜单）暂借焦点，保留控件和选区供命令执行，但仍按失焦规则保存。
            InputEvent::Blur if self.picker_open || self.color_menu.is_some() => self.save_now(cx),
            InputEvent::Blur => self.exit_edit(cx),
        }
    }

    /// 控件文本 → 文档：收缩成最小编辑写回。
    fn on_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sanitize_cell(window, cx);
        let paired = self.complete_pairs(window, cx);
        let Some(f) = self.focused.as_mut() else { return };
        let new_text = f.textarea.read(cx).value();
        let old_text = self.doc.slice(f.range.clone()).to_string();
        let Some(edit) = minimal_edit(f.range.start, &old_text, &new_text) else { return };

        let now = Instant::now();
        let small = !edit.text.contains('\n') && edit.text.len() <= 8 && edit.range.len() <= 8;
        let coalesce =
            !paired && small && self.last_edit_at.is_some_and(|t| now.duration_since(t) < COALESCE_WINDOW);
        let delta = edit.delta();
        let result = if coalesce { self.doc.edit_coalescing(edit) } else { self.doc.edit(edit) };
        if let Err(err) = result {
            log::error!("edit rejected: {err}");
            return;
        }
        f.range.end = (f.range.end as isize + delta).max(f.range.start as isize) as usize;
        self.last_edit_at = (!paired).then_some(now);
        self.after_doc_change(cx);
        self.update_wiki(cx);
        self.update_tags(cx);
        self.update_slash(cx);
        if !self.promote_typed_prefix(window, cx) {
            self.refresh_markup(cx);
        }
    }

    // ---- `/` 菜单（7.1） ----

    /// 每次控件文本变化后：光标前有 `/query` 就开着菜单并实时过滤，否则关掉。
    fn update_slash(&mut self, cx: &mut Context<Self>) {
        if self.wiki.is_some() || self.tag_menu.is_some() {
            self.slash = None;
            return;
        }
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

    /// 确认菜单条目：删掉 `/query`，再把所在块转换成目标类型（或插入日期、上色）。合成一个撤销组。
    fn slash_confirm(&mut self, index: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.slash.take() else { return };
        let Some(&hit) = index.or(Some(menu.selected)).and_then(|i| menu.matches.get(i)) else { return };
        let action = match hit {
            slash::Match::Item(ix) => slash::ITEMS[ix].action,
            slash::Match::Color(ix) => SlashAction::Color(ix),
        };
        // 命令前面的空格（"hello /h" 里的那个）一起删掉，但不越过编辑区间起点。
        // 按字符串裁而不是逐字节回退：`/` 前面可能是多字节字符（"段落 /红色"），按字节切会切在半个字上
        let before = self.doc.slice(menu.block_start..menu.anchor).to_string();
        let start = menu.anchor - (before.len() - before.trim_end_matches(' ').len());
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
            // `/颜色` 与 `/红色`：先把命令本身删掉，再开菜单 / 直接给这一块上色
            SlashAction::OpenColors | SlashAction::Color(_) => {
                self.structural_edit(Edit::delete(command.clone()), command.start, window, cx);
                match action {
                    SlashAction::OpenColors => {
                        self.open_color_menu(window, cx);
                    }
                    SlashAction::Color(ix) => self.apply_menu_color(ix, cx),
                    _ => unreachable!("只有颜色两种"),
                }
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
        if self.input_composing(window, cx) {
            cx.propagate();
            return;
        }
        if self.confirm_tag(None, window, cx) || self.confirm_wiki(None, false, window, cx) {
            return;
        }
        if self.slash.is_some() {
            self.slash_confirm(None, window, cx);
        } else if let Some(presenter::Key { id, part: presenter::Part::Cell(row, col) }) =
            self.focused.as_ref().and_then(|f| self.presenter_key_of(&f.textarea))
        {
            // 表格里 Tab 到下一格，行末换到下一行
            let next = presenter::Key { id, part: presenter::Part::Cell(row, col + 1) };
            let (row, col) = if self.presenters.contains_key(&next) { (row, col + 1) } else { (row + 1, 0) };
            self.move_table_cell(id, row, col, window, cx);
        } else {
            cx.propagate();
        }
    }

    /// 文本已改：重解析、刷新单位、标脏、排队保存。
    fn after_doc_change(&mut self, cx: &mut Context<Self>) {
        self.cancel_link_preview(cx);
        self.doc.reparse();
        self.update_local_link_names();
        self.units = units::units(&self.doc);
        self.refresh_embeds(cx);
        if let Some(f) = self.focused.as_mut() {
            // 单元格、callout 标题与正文从块中间开始：仍属于原来那一块，否则控件会在别处再画一份
            let prev = f.id;
            f.id = unit_at(&self.units, f.range.start)
                .filter(|&ix| {
                    let unit = &self.units[ix];
                    unit.range.start == f.range.start
                        || (Some(unit.id) == prev
                            && unit.range.start <= f.range.start
                            && f.range.end <= unit.range.end)
                })
                .map(|ix| self.units[ix].id);
        }
        self.clamp_selection();
        self.rich.export_menu = None;
        self.sync_diagram_preview(false, cx);
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

    /// 编辑中的这一段在表格里（整张表的源码，或其中一格）：写进去的 `|` 要转义。
    pub(super) fn focused_in_table(&self) -> bool {
        let Some(f) = self.focused.as_ref() else { return false };
        f.id.and_then(|id| self.units.iter().find(|u| u.id == id)).is_some_and(|u| u.kind == BlockKind::Table)
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
        if self.input_composing(window, cx) {
            cx.propagate();
            return;
        }
        if self.confirm_tag(None, window, cx) || self.confirm_wiki(None, false, window, cx) {
            return;
        }
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
        if let Some(key) = self.presenter_key_of(&f.textarea) {
            match key.part {
                presenter::Part::CalloutTitle => return self.enter_callout_body(key.id, window, cx),
                presenter::Part::Cell(row, col) => {
                    return self.move_table_cell(key.id, row + 1, col, window, cx);
                }
                _ if f.enter_newline && !self.editing_code() => return self.enter_in_quote(window, cx),
                _ => {}
            }
        }
        if f.enter_newline {
            cx.propagate();
            return;
        }
        let range = f.range.clone();
        let unit = self.focused_unit().map(|ix| self.units[ix].clone());
        let plan = ops::split(&self.doc, unit.as_ref(), &range, cursor);
        self.apply_plan(plan, window, cx);
    }

    /// 引用 / callout 正文里的 ⏎：新起一行并带上 `> `（界面上看不见）。空的最后一行再按 ⏎ 离开引用。
    fn enter_in_quote(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = self.focused.as_mut() else { return };
        let ta = f.textarea.clone();
        let (text, selection) = {
            let ta = ta.read(cx);
            (ta.value().to_string(), ta.selected_range())
        };
        let ls = line_start(&text, selection.start);
        let line_end = text[selection.end..].find('\n').map_or(text.len(), |i| selection.end + i);
        let line = &text[ls..line_end];
        if selection.is_empty()
            && line_end == text.len()
            && ls > 0
            && line.trim_start_matches('>').trim().is_empty()
        {
            let abs = f.range.start + ls - 1..f.range.start + text.len();
            self.structural_edit(Edit::replace(abs.clone(), "\n\n"), abs.start + 2, window, cx);
            return;
        }
        let depth =
            line.bytes().take_while(|b| *b == b'>' || *b == b' ').filter(|b| *b == b'>').count().max(1);
        let prefix = format!("\n{} ", ">".repeat(depth));
        f.pairs.pending = None;
        ta.update(cx, |ta, cx| ta.replace(prefix, window, cx));
        self.on_change(window, cx);
    }

    /// callout 标题里的 ⏎：到正文开头；还没有正文就先开一行。
    fn enter_callout_body(&mut self, id: BlockId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(block) = self.doc.blocks().find(id) else { return };
        let range = self.doc.content_range(block);
        let src = self.doc.slice(range.clone()).to_string();
        let Some(parts) = callout_edit_parts(&src) else { return };
        if parts.body.is_empty() {
            let at = range.start + parts.title.end;
            self.structural_edit(Edit::insert(at, "\n> "), at + 3, window, cx);
        } else {
            let body = range.start + parts.body.start..range.start + parts.body.end;
            let cursor = src[parts.body.clone()].find(|c: char| c != '>' && c != ' ').unwrap_or(0);
            self.focus_range(body, Some(id), true, cursor, window, cx);
        }
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
        self.backspace_at_block_start(window, cx);
    }

    /// 在隐藏了标记的文字里，⌫ / ⌦ 先于文本控件处理：删的是看得见的字，标记不被拆开。
    fn capture_deletion(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.input_composing(window, cx) || !self.conceal_focused(cx) {
            return;
        }
        let Some(f) = self.focused.as_ref() else { return };
        let ta = f.textarea.read(cx);
        let selection = ta.selected_range();
        if !selection.is_empty() {
            return;
        }
        let text = ta.value().to_string();
        let analysis = aero_core::markup::analyze_editing(&text, Some(selection.clone()));
        let spans: Vec<Range<usize>> = analysis.spans().cloned().collect();
        let hidden_between = |start: usize, end: usize| {
            text[start..end].char_indices().all(|(offset, _)| analysis.conceals(start + offset))
        };
        let line_break = if forward {
            analysis.hard_breaks.iter().find(|range| {
                selection.start < range.end
                    && (range.start <= selection.start || hidden_between(selection.start, range.start))
            })
        } else {
            analysis.hard_breaks.iter().rev().find(|range| {
                range.start < selection.start
                    && (selection.start <= range.end || hidden_between(range.end, selection.start))
            })
        };
        let plan = if let Some(range) = line_break {
            Deletion::Range { range: range.clone(), caret: range.start }
        } else if forward {
            plan_delete(&text, &analysis.conceal, &spans, selection.start)
        } else {
            plan_backspace(&text, &analysis.conceal, &spans, selection.start)
        };
        match plan {
            Deletion::Native => {}
            Deletion::Nothing => cx.stop_propagation(),
            Deletion::AtStart => {
                cx.stop_propagation();
                self.backspace_at_block_start(window, cx);
            }
            Deletion::Range { range, caret } => {
                cx.stop_propagation();
                let f = self.focused.as_mut().expect("checked above");
                f.pairs.pending = None;
                f.textarea.clone().update(cx, |ta, cx| {
                    ta.set_selected_range(range, cx);
                    ta.replace("", window, cx);
                    ta.set_selected_range(caret..caret, cx);
                });
                self.on_change(window, cx);
            }
        }
    }

    /// 块首退格：列表项脱离列表、段落并入上一块，或移到上一块末尾。
    fn backspace_at_block_start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 单元格、callout 的标题与正文、代码：开头退格不跨出这一格
        if self.editing_code()
            || self
                .focused
                .as_ref()
                .and_then(|f| self.presenter_key_of(&f.textarea))
                .is_some_and(|key| key.part != presenter::Part::Unit)
        {
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
        self.move_across_blocks(false, window, cx);
    }

    fn on_block_down(&mut self, _: &BlockDown, window: &mut Window, cx: &mut Context<Self>) {
        self.move_across_blocks(true, window, cx);
    }

    /// ↑ / ↓：光标在这一段的首 / 末**视觉行**时才离开这一段，否则交给文本控件在折行之间移动。
    /// 离开时保持光标的横向位置（9.4）：表格里到上 / 下一行的同一列，callout 标题与正文互通，
    /// 其余到上 / 下一个单位里离得最近的那个字。
    fn move_across_blocks(&mut self, down: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.input_composing(window, cx) {
            cx.propagate();
            return;
        }
        let delta = if down { 1 } else { -1 };
        if self.move_tag(delta, cx) || self.move_wiki(delta, cx) {
            return;
        }
        if self.slash.is_some() {
            self.slash_move(delta, cx);
            return;
        }
        let Some((text, cursor)) = self.textarea_text_and_cursor(cx) else {
            cx.propagate();
            return;
        };
        let Some(f) = self.focused.as_ref() else { return };
        let caret = f.textarea.read(cx).range_to_bounds(&(cursor..cursor));
        let area = f.textarea.read(cx).input_bounds();
        let at_edge = match caret {
            // 有布局时按视觉行判断：长段落折成几行，↓ 先在行间走
            Some(caret) if area.size.height > px(0.) => {
                if down {
                    caret.bottom() >= area.bottom() - px(1.)
                } else {
                    caret.top() <= area.top() + px(1.)
                }
            }
            _ => {
                let (line, _) = line_col(&text, cursor);
                if down { line + 1 == line_count(&text) } else { line == 0 }
            }
        };
        if !at_edge {
            cx.propagate();
            return;
        }
        let x = caret.map(|caret| caret.left());
        let key = self.presenter_key_of(&f.textarea);
        let id = f.id;
        let range = f.range.clone();
        // 表格：上 / 下一行的同一列；callout：标题 ↔ 正文
        let sibling = match key.map(|key| key.part) {
            Some(presenter::Part::Cell(row, col)) => {
                let row = if down { Some(row + 1) } else { row.checked_sub(1) };
                row.map(|row| presenter::Part::Cell(row, col))
            }
            Some(presenter::Part::CalloutTitle) if down => Some(presenter::Part::CalloutBody),
            Some(presenter::Part::CalloutBody) if !down => Some(presenter::Part::CalloutTitle),
            _ => None,
        };
        if let (Some(id), Some(part)) = (id, sibling)
            && let Some(target) = self.presenters.get(&presenter::Key { id, part })
            && !(part == presenter::Part::CalloutBody && target.range.is_empty())
        {
            let target = presenter::Key { id, part };
            self.focus_presenter_at_x(target, x, down, window, cx);
            return;
        }
        let col = cursor - line_start(&text, cursor);
        let target = if down {
            ops::down_target(&self.doc, &self.units, &range, col)
        } else {
            ops::up_target(&self.doc, &self.units, range.start, col)
        };
        match target {
            Some((ix, cursor)) => {
                if let Some(cell) = x.and_then(|x| self.table_cell_under(self.units[ix].id, x, down, cx)) {
                    self.focus_presenter_at_x(cell, x, down, window, cx);
                    return;
                }
                self.focus_unit(ix, cursor, window, cx);
                self.place_caret_at_x(x, down, cx);
            }
            None if down => {
                // 已是最后一块：非文本块（代码块、表格、callout 等）用 ↓ 离开时在文末补一个空段落
                let is_text = id
                    .and_then(|id| self.units.iter().find(|u| u.id == id))
                    .is_none_or(|u| u.is_text() && key.is_none_or(|k| k.part == presenter::Part::Unit));
                if !is_text || self.editing_code() {
                    self.append_paragraph(window, cx);
                }
            }
            None => {}
        }
    }

    /// 从上面进表格落在表头行、从下面进落在末行，列取横向位置 `x` 所在的那一列。
    fn table_cell_under(&self, id: BlockId, x: Pixels, first_row: bool, cx: &App) -> Option<presenter::Key> {
        let cells: Vec<(usize, usize)> = self
            .presenters
            .keys()
            .filter(|key| key.id == id)
            .filter_map(|key| match key.part {
                presenter::Part::Cell(row, col) => Some((row, col)),
                _ => None,
            })
            .collect();
        let row = if first_row { 0 } else { cells.iter().map(|(row, _)| *row).max()? };
        let key = |col| presenter::Key { id, part: presenter::Part::Cell(row, col) };
        let mut columns: Vec<usize> = cells.iter().filter(|(r, _)| *r == row).map(|(_, col)| *col).collect();
        columns.sort_unstable();
        let under = columns.iter().copied().find(|col| {
            let area = self.presenters[&key(*col)].textarea.read(cx).input_bounds();
            x < area.right() + px(8.)
        });
        under.or(columns.last().copied()).map(key)
    }

    /// 聚焦一段已经画出来的文字，光标放在离 `x` 最近的字上：从上面下来落在首行，从下面上来落在末行。
    fn focus_presenter_at_x(
        &mut self,
        key: presenter::Key,
        x: Option<Pixels>,
        from_above: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(p) = self.presenters.get(&key) else { return };
        let (range, nl) = (p.range.clone(), p.enter_newline);
        let cursor = if from_above { 0 } else { range.len() };
        self.focus_range(range, Some(key.id), nl, cursor, window, cx);
        self.place_caret_at_x(x, from_above, cx);
    }

    /// 刚聚焦的那一段里，把光标挪到首行（`top`）或末行上离 `x` 最近的位置。控件还没布局时保持原样。
    fn place_caret_at_x(&mut self, x: Option<Pixels>, top: bool, cx: &mut Context<Self>) {
        let (Some(x), Some(f)) = (x, self.focused.as_ref()) else { return };
        let ta = f.textarea.clone();
        let area = ta.read(cx).input_bounds();
        if area.size.height <= px(0.) {
            return;
        }
        let line = ta.read(cx).line_height().unwrap_or(px(BODY_FONT_SIZE * BODY_LINE_HEIGHT));
        let y = if top { area.top() + line / 2. } else { area.bottom() - line / 2. };
        let x = x.clamp(area.left(), area.right() - px(1.));
        if let Some(offset) = ta.read(cx).offset_for_point(point(x, y)) {
            ta.update(cx, |ta, cx| ta.set_selected_range(offset..offset, cx));
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

    /// 撤销 / 重做之后：光标放到受影响区间的末尾（编辑态下），像所有编辑器那样"跟着变化走"；
    /// 块选中态下改为选中受影响处的块（撤销一次块删除，被恢复的块就是选中的那块）。
    fn after_history(&mut self, affected: Range<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.wiki = None;
        self.tag_menu = None;
        self.last_edit_at = None;
        let was_editing = self.focused.take().is_some();
        let was_selecting = self.selection.is_some();
        self.after_doc_change(cx);
        if was_editing {
            self.focus_at(affected.end, None, window, cx);
        } else if was_selecting {
            let ix = unit_at(&self.units, affected.start)
                .or_else(|| self.units.iter().position(|u| u.range.start >= affected.start))
                .unwrap_or(self.units.len().saturating_sub(1));
            self.select_blocks(ix, ix, window, cx);
        }
    }

    /// 单位变了（编辑、重载）之后让选区仍然落在合法下标上；没有单位就退出块选中态。
    fn clamp_selection(&mut self) {
        if let Some(sel) = self.selection.as_mut() {
            if self.units.is_empty() {
                self.selection = None;
            } else {
                let last = self.units.len() - 1;
                sel.anchor = sel.anchor.min(last);
                sel.head = sel.head.min(last);
            }
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
    /// 文件已在外部被删时不写：自动保存不该把用户刚删掉的文件变回来（横幅上有"恢复"）。
    pub fn save_now(&mut self, cx: &mut Context<Self>) {
        if !self.dirty || self.save_state == SaveState::Saving || self.is_deleted() {
            return;
        }
        self.save_task = None;
        match self.disk_state() {
            Ok(Disk::Changed(disk, stamp)) => {
                if !self.write_conflict_copy(&disk, stamp, cx) {
                    return;
                }
            }
            Ok(Disk::Missing) => {
                self.mark_deleted(cx);
                return;
            }
            Ok(Disk::Unchanged) => {}
            Err(err) => {
                self.save_failed(format!("无法校验磁盘版本，已停止保存：{err}"), cx);
                return;
            }
        }
        self.start_write(cx);
    }

    /// 真正的异步写盘，不看磁盘状态（校验在 [`Self::save_now`]；"恢复"要越过"文件不存在"直接写）。
    fn start_write(&mut self, cx: &mut Context<Self>) {
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
                        this.update_local_link_names();
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

    pub fn has_pending_export(&self) -> bool {
        self.rich.export_in_progress()
    }

    /// 同步落盘（切换笔记、退出前）。有外部修改时同样先留冲突副本，返回其路径。
    /// 文件已在外部被删时不写回（与 [`Self::save_now`] 一致），未保存内容随编辑器丢弃。
    pub fn flush_sync(&mut self) -> Result<Option<VaultPath>, VaultError> {
        if self.has_pending_export() {
            return Err(VaultError::Io {
                path: self.vault.fs_path(&self.path),
                source: std::io::Error::other("正在导出图表，请完成后再试"),
            });
        }
        if !self.dirty || self.is_deleted() {
            return Ok(None);
        }
        self.save_task = None;
        let conflict = match self.disk_state()? {
            Disk::Changed(disk, _) => Some(self.backup_disk(&disk)?),
            Disk::Missing => {
                log::warn!("{}: deleted on disk; unsaved changes dropped", self.path.as_str());
                return Ok(None);
            }
            Disk::Unchanged => None,
        };
        let stamp = self.vault.write_bytes(&self.path, &self.doc.to_bytes())?;
        self.stamp = stamp;
        self.dirty = false;
        self.save_state = SaveState::Saved(Instant::now());
        Ok(conflict)
    }

    // ---- 外部修改（方案 12：vault 监听 + 2 s 轮询兜底 + 写前校验；不做三方合并） ----

    /// 磁盘文件相对编辑器所知指纹的状态：先比 mtime/size（不读内容），变了再比内容 hash。
    fn disk_state(&mut self) -> Result<Disk, VaultError> {
        let (mtime, size) = match self.vault.stat(&self.path) {
            Ok(stamp) => stamp,
            Err(VaultError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Disk::Missing);
            }
            Err(err) => return Err(err),
        };
        if mtime == self.stamp.mtime && size == self.stamp.size {
            return Ok(Disk::Unchanged);
        }
        let (bytes, stamp) = self.vault.read_bytes(&self.path)?;
        if stamp.hash == self.stamp.hash {
            // 只是被 touch 了一下（或同步工具写回同样的内容）
            self.stamp = stamp;
            return Ok(Disk::Unchanged);
        }
        Ok(Disk::Changed(bytes, stamp))
    }

    /// 监听 / 轮询 / 窗口激活时调用：无未保存修改则重载，有则留冲突副本并立即落盘本地版本；
    /// 文件不见了进入"已删除"态，又出现了（比如从废纸篓放回）则解除。
    pub fn check_external(&mut self, cx: &mut Context<Self>) {
        if self.save_state == SaveState::Saving {
            return;
        }
        match self.disk_state() {
            Ok(Disk::Missing) => self.mark_deleted(cx),
            Ok(Disk::Unchanged) => {
                if self.is_deleted() {
                    self.set_banner(None, cx);
                    self.update_local_link_names();
                    if self.dirty {
                        self.schedule_save(cx);
                    }
                }
            }
            Ok(Disk::Changed(disk, stamp)) => {
                if self.is_deleted() {
                    self.set_banner(None, cx);
                }
                if self.dirty {
                    // 统一经过写前校验与备份；失败时不能继续保存。
                    self.save_now(cx);
                } else {
                    self.reload(&disk, stamp, cx);
                    self.set_banner(Some(Banner::Reloaded), cx);
                }
            }
            Err(err) => self.save_failed(format!("无法读取磁盘版本，已停止保存：{err}"), cx),
        }
    }

    /// 用磁盘内容替换文档（undo 栈随之丢弃）。编辑态退回浏览态：旧的控件区间对新文本已无意义，
    /// 而这里拿不到 `Window` 去重新聚焦——无未保存修改时通常也没人正在打字。
    fn reload(&mut self, disk: &[u8], stamp: FileStamp, cx: &mut Context<Self>) {
        self.wiki = None;
        self.tag_menu = None;
        let Ok(doc) = Document::from_bytes(disk) else {
            log::warn!("reload {}: not valid UTF-8, keeping in-memory version", self.path.as_str());
            return;
        };
        log::info!("reload {} (changed on disk)", self.path.as_str());
        self.cancel_link_preview(cx);
        self.focused = None;
        // 块 id 随新文档重排，旧的定位高亮会落到别的块上
        self.highlight = None;
        self.highlight_task = None;
        self.doc = doc;
        self.update_local_link_names();
        self.stamp = stamp;
        self.units = units::units(&self.doc);
        self.refresh_embeds(cx);
        self.clamp_selection();
        self.preview.update(cx, |state, _| *state = preview::PreviewState::default());
        self.callout_folds.clear();
        self.dirty = false;
        self.save_task = None;
        self.last_edit_at = None;
        self.save_state = SaveState::Clean;
        cx.emit(EditorEvent::Reloaded);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    // ---- 横幅（10.2） ----

    #[cfg(test)]
    pub fn banner(&self) -> Option<&Banner> {
        self.banner.as_ref()
    }

    /// 文件已在外部被删除：不再自动保存，内容留在编辑器里。
    pub fn is_deleted(&self) -> bool {
        matches!(self.banner, Some(Banner::Deleted))
    }

    fn set_banner(&mut self, banner: Option<Banner>, cx: &mut Context<Self>) {
        self.banner_task = None;
        if banner == Some(Banner::Reloaded) {
            self.banner_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(RELOADED_BANNER).await;
                let _ = this.update(cx, |this, cx| {
                    if this.banner == Some(Banner::Reloaded) {
                        this.banner = None;
                        cx.notify();
                    }
                });
            }));
        }
        self.banner = banner;
        cx.notify();
    }

    fn mark_deleted(&mut self, cx: &mut Context<Self>) {
        if self.is_deleted() {
            return;
        }
        log::warn!("{}: deleted on disk", self.path.as_str());
        self.cancel_link_preview(cx);
        self.save_task = None;
        self.set_banner(Some(Banner::Deleted), cx);
        self.update_local_link_names();
        cx.emit(EditorEvent::Deleted);
        cx.emit(EditorEvent::Changed);
    }

    /// 横幅"保留我的"：副本留在 vault 里，横幅消失。
    pub fn dismiss_banner(&mut self, cx: &mut Context<Self>) {
        self.set_banner(None, cx);
    }

    /// 横幅"使用磁盘版本"：把冲突副本的内容换进来并落盘到本笔记，副本删掉（内容已在这里，不会丢）。
    pub fn take_disk_version(&mut self, cx: &mut Context<Self>) {
        let Some(Banner::Conflict { copy }) = self.banner.clone() else { return };
        let (bytes, _) = match self.vault.read_bytes(&copy) {
            Ok(v) => v,
            Err(err) => {
                cx.emit(EditorEvent::SaveFailed(format!("无法读取冲突副本 {}：{err}", copy.as_str())));
                return;
            }
        };
        self.reload(&bytes, self.stamp, cx);
        self.dirty = true;
        self.save_state = SaveState::Dirty;
        self.set_banner(None, cx);
        self.save_now(cx);
        if let Err(err) = std::fs::remove_file(self.vault.fs_path(&copy)) {
            log::warn!("remove conflict copy {}: {err}", copy.as_str());
        }
        cx.emit(EditorEvent::PathsTouched(vec![copy]));
    }

    /// 横幅"恢复"：把编辑器里的内容写回原路径。
    pub fn restore_deleted(&mut self, cx: &mut Context<Self>) {
        if !self.is_deleted() || self.save_state == SaveState::Saving {
            return;
        }
        self.set_banner(None, cx);
        self.save_task = None;
        self.dirty = true;
        self.start_write(cx);
    }

    /// 横幅"另存为…"：写到 vault 内的另一条路径并改跟它。
    pub fn save_as(&mut self, path: VaultPath, cx: &mut Context<Self>) -> Result<(), VaultError> {
        let stamp = self.vault.write_bytes(&path, &self.doc.to_bytes())?;
        let old = std::mem::replace(&mut self.path, path.clone());
        self.stamp = stamp;
        self.dirty = false;
        self.save_task = None;
        self.save_state = SaveState::Saved(Instant::now());
        self.set_banner(None, cx);
        self.repath_link_preview(&old, cx);
        cx.emit(EditorEvent::PathsTouched(vec![old, path]));
        cx.emit(EditorEvent::Saved);
        cx.emit(EditorEvent::Changed);
        Ok(())
    }

    /// "另存为…"的系统对话框：默认放在同目录，名字带 `-恢复`。
    fn prompt_save_as(&mut self, cx: &mut Context<Self>) {
        let dir = self.vault.fs_path(&self.path);
        let dir = dir.parent().map(Path::to_path_buf).unwrap_or_else(|| self.vault.root().to_path_buf());
        let suggested = format!("{}-恢复.md", self.path.stem());
        let root = self.vault.root().to_path_buf();
        let rx = cx.prompt_for_new_path(&dir, Some(&suggested));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(target))) = rx.await else { return };
            let _ = this.update(cx, |this, cx| {
                let mut target = target;
                if target.extension().is_none() {
                    target.set_extension("md");
                }
                let result = VaultPath::from_fs(&root, &target)
                    .map_err(|_| format!("{} 不在当前 vault 里", target.display()))
                    .and_then(|p| this.save_as(p, cx).map_err(|e| e.to_string()));
                if let Err(msg) = result {
                    cx.emit(EditorEvent::SaveFailed(format!("无法另存为：{msg}")));
                }
            });
        })
        .detach();
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
    fn backup_disk(&self, disk: &[u8]) -> Result<VaultPath, VaultError> {
        let base = self.conflict_path();
        // 一秒内可能收到多次外部保存；同名时换名字，不能覆盖上一份副本。
        for attempt in 0..1000 {
            let path = if attempt == 0 {
                base.clone()
            } else {
                VaultPath::new(&format!("{}-{attempt}.md", base.as_str().trim_end_matches(".md")))?
            };
            match self.vault.create_bytes(&path, disk) {
                Ok(_) => return Ok(path),
                Err(VaultError::Io { source, .. }) if source.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err),
            }
        }
        Err(VaultError::AlreadyExists(base.to_string()))
    }

    fn save_failed(&mut self, msg: String, cx: &mut Context<Self>) {
        self.save_task = None;
        if self.save_state != SaveState::Failed(msg.clone()) {
            log::error!("{msg}");
            self.save_state = SaveState::Failed(msg.clone());
            cx.emit(EditorEvent::SaveFailed(msg));
            cx.emit(EditorEvent::Changed);
            cx.notify();
        }
    }

    fn write_conflict_copy(&mut self, disk: &[u8], stamp: FileStamp, cx: &mut Context<Self>) -> bool {
        match self.backup_disk(disk) {
            Ok(conflict) => {
                log::warn!(
                    "{}: changed on disk while dirty; disk version saved as {}",
                    self.path.as_str(),
                    conflict.as_str()
                );
                // 记下磁盘当前指纹：接下来的写盘不该再把同一份外部修改当成新冲突
                self.stamp = stamp;
                self.set_banner(Some(Banner::Conflict { copy: conflict.clone() }), cx);
                cx.emit(EditorEvent::PathsTouched(vec![conflict]));
                true
            }
            Err(err) => {
                self.save_failed(format!("无法保存冲突副本，已停止保存：{err}"), cx);
                false
            }
        }
    }

    // ---- 渲染 ----

    /// 文档顶部横幅（10.2）：一行说明 + 几个动作按钮。
    fn render_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let banner = self.banner.as_ref()?;
        let theme = cx.theme();
        let (color, text, actions): (Hsla, String, Vec<BannerAction>) = match banner {
            Banner::Reloaded => (
                theme.info,
                "此文件在外部被修改，已重新载入。".to_owned(),
                vec![("banner-ok", "好", Box::new(|this, _, cx| this.dismiss_banner(cx)))],
            ),
            Banner::Conflict { copy } => {
                let copy_open = copy.clone();
                (
                    theme.warning,
                    format!(
                        "此文件在外部被修改，且你有未保存的改动。磁盘版本已另存为 {}，当前内容已照常保存。",
                        copy.as_str()
                    ),
                    vec![
                        ("banner-keep", "保留我的", Box::new(|this, _, cx| this.dismiss_banner(cx))),
                        ("banner-disk", "使用磁盘版本", Box::new(|this, _, cx| this.take_disk_version(cx))),
                        (
                            "banner-open-copy",
                            "打开副本",
                            Box::new(move |_, _, cx| cx.emit(EditorEvent::OpenRequested(copy_open.clone()))),
                        ),
                    ],
                )
            }
            Banner::Deleted => (
                theme.danger,
                "此文件已在外部被删除。内容保留在这里，不会再自动保存。".to_owned(),
                vec![
                    ("banner-restore", "恢复", Box::new(|this, _, cx| this.restore_deleted(cx))),
                    ("banner-save-as", "另存为…", Box::new(|this, _, cx| this.prompt_save_as(cx))),
                    ("banner-close", "关闭", Box::new(|_, _, cx| cx.emit(EditorEvent::CloseRequested))),
                ],
            ),
        };
        let buttons: Vec<AnyElement> = actions
            .into_iter()
            .map(|(id, label, action)| {
                Button::new(id)
                    .outline()
                    .xsmall()
                    .label(label)
                    .on_click(cx.listener(move |this, _, window, cx| action(this, window, cx)))
                    .into_any_element()
            })
            .collect();
        Some(
            h_flex()
                .id("banner")
                .w_full()
                .mb_4()
                .px_3()
                .py_2()
                .gap_3()
                .items_center()
                .rounded(px(6.))
                .border_l(px(3.))
                .border_color(color)
                .bg(color.opacity(0.12))
                .text_size(px(13.))
                .line_height(relative(1.4))
                .child(div().flex_1().min_w_0().child(text))
                .child(h_flex().flex_none().gap_1().children(buttons))
                .into_any_element(),
        )
    }

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
                .debug_selector(|| "editor-title".into())
                .cursor_text()
                .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                    if !event.is_right_click() {
                        this.start_title_edit(window, cx);
                    }
                }))
                .child(self.title())
                .into_any_element(),
        }
    }

    fn text_view_style(&self, cx: &App) -> TextViewStyle {
        let theme = cx.theme();
        let radius = theme.semantic_tokens().radius.md;
        TextViewStyle::default()
            .with_foreground(theme.foreground)
            .with_muted_foreground(theme.muted_foreground)
            .with_link(theme.link)
            .with_selection(theme.selection)
            .with_code_background(theme.muted)
            .with_border(theme.border)
            .with_inline_code(HighlightStyle { background_color: Some(theme.accent), ..Default::default() })
            .with_code_block(StyleRefinement::default().rounded(radius))
            .with_paragraph_gap(rems(0.5))
            .with_heading_base_font_size(px(BODY_FONT_SIZE))
            .with_heading_font_size(|level, base| match level {
                1 => base * 1.75,
                2 => base * 1.4,
                3 => base * 1.2,
                _ => base,
            })
            .with_dark(theme.mode.is_dark())
            .with_table(StyleRefinement {
                overflow: PointRefinement { x: Some(Overflow::Scroll), y: None },
                ..StyleRefinement::default().rounded(radius)
            })
            .with_table_head(
                StyleRefinement::default()
                    .bg(theme.table_head)
                    .text_color(theme.table_head_foreground)
                    .font_weight(FontWeight::SEMIBOLD),
            )
    }

    fn is_focused_block(&self, id: BlockId) -> bool {
        self.focused.as_ref().is_some_and(|f| f.id == Some(id))
    }

    /// 有块处于编辑态（源码文本框可见）。
    #[cfg(test)]
    pub fn is_editing(&self) -> bool {
        self.focused.is_some()
    }

    /// 块选区里"头"那一块（⇧↑↓ 动的那一端）。
    fn selection_head(&self) -> Option<BlockId> {
        let sel = self.selection?;
        self.units.get(sel.head).map(|u| u.id)
    }

    /// 这个块（作为编辑单位）在块选区里。
    pub(super) fn is_selected_block(&self, id: BlockId) -> bool {
        let Some(sel) = self.selection else { return false };
        self.units.iter().position(|u| u.id == id).is_some_and(|ix| sel.contains(ix))
    }

    /// 块选中态的高亮：整块淡底色，与搜索定位的 `accent` 高亮区分。
    fn selected_style<E: Styled>(el: E, selection: Hsla) -> E {
        el.bg(selection).rounded(px(6.)).mx(px(-8.)).px(px(8.))
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
        let mut el = div()
            .relative()
            .w_full()
            .child(highlight_ui::background(f.textarea.clone(), f.highlight.clone()))
            .child(Textarea::new(&f.textarea))
            .when(!self.conceal_focused(cx), |el| el.child(highlight_ui::faded_markers(f.textarea.clone())));
        match kind {
            Some(BlockKind::Heading { level }) => {
                // 和预览里的标题同一套字号、字重、下边距，点进去行高不变。
                el = el
                    .text_size(heading_size(*level))
                    .line_height(relative(BODY_LINE_HEIGHT))
                    .font_weight(if *level == 1 { FontWeight::BOLD } else { FontWeight::SEMIBOLD })
                    .pb(rems(0.3));
            }
            Some(BlockKind::Paragraph) => {
                el = el.text_size(px(BODY_FONT_SIZE)).line_height(relative(BODY_LINE_HEIGHT));
            }
            Some(BlockKind::CodeFence { .. } | BlockKind::CodeBlock) => {
                // 和预览代码块同一套：等宽字号、四周 p_3、行高跟正文。
                el = el
                    .font_family(theme.mono_font_family.clone())
                    .text_size(theme.mono_font_size)
                    .line_height(relative(BODY_LINE_HEIGHT))
                    .bg(theme.muted)
                    .rounded(theme.semantic_tokens().radius.md)
                    .p_3();
            }
            Some(k) if is_monospace(k) => {
                el = el
                    .font_family(theme.mono_font_family.clone())
                    .text_size(px(13.))
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
        if self.wiki.is_some() {
            el = el.child(self.render_wiki_menu(cx));
        }
        if self.tag_menu.is_some() {
            el = el.child(self.render_tag_menu(cx));
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
        let dark = theme.mode.is_dark();
        let rows: Vec<AnyElement> = menu
            .matches
            .iter()
            .enumerate()
            .map(|(i, &hit)| {
                // 颜色候选（7.7）用真实颜色画的 `A` 代替快捷键提示
                let (label, hint, sample): (SharedString, SharedString, Option<AnyElement>) = match hit {
                    slash::Match::Item(ix) => {
                        (slash::ITEMS[ix].label.into(), slash::ITEMS[ix].hint.into(), None)
                    }
                    slash::Match::Color(ix) => (
                        color_menu::full_name(ix).into(),
                        "".into(),
                        Some(Self::color_swatch(ix, dark, cx).into_any_element()),
                    ),
                };
                let active = i == menu.selected;
                h_flex()
                    .id(("slash-item", i))
                    .w_full()
                    .h(px(28.))
                    .px_2()
                    .gap_2()
                    .rounded(px(4.))
                    .items_center()
                    .when(active, |el| el.bg(theme.list_active))
                    .hover(|el| el.bg(theme.list_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.slash_confirm(Some(i), window, cx);
                        }),
                    )
                    .children(sample)
                    .child(div().flex_1().min_w_0().text_size(px(13.)).child(label))
                    .child(
                        div().flex_none().text_size(px(11.)).text_color(theme.muted_foreground).child(hint),
                    )
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
            if let Some(el) = self.render_block(b, 0, None, cx) {
                out.push(el);
            }
        }
        if pending.is_some() {
            out.push(self.render_textarea(None, cx));
        }
        out
    }

    /// 块颜色作用于整块及其子块（6.3.10）：背景在最外层那个块上铺一次，文字色往下传，
    /// `inherited` 就是祖先块的文字色，本块自己的属性优先。
    fn render_block(
        &self,
        b: &Block,
        depth: usize,
        inherited: Option<Hsla>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let paint = crate::colors::of(&b.attrs, cx).inherit(inherited);
        match &b.kind {
            BlockKind::Marker => None,
            BlockKind::List { ordered, start } => {
                let items: Vec<&Block> =
                    b.children.iter().filter(|c| matches!(c.kind, BlockKind::ListItem { .. })).collect();
                let els: Vec<AnyElement> = items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| {
                        let number = ordered.then_some(*start + i as u32);
                        self.render_list_item(item, number, depth, paint.text, cx)
                    })
                    .collect();
                Some(Self::painted(v_flex().w_full().gap(px(4.)).children(els), paint).into_any_element())
            }
            // 列表项自己的颜色由 `render_list_item` 画：它要把标记列也包进背景里
            BlockKind::ListItem { .. } => Some(self.render_list_item(b, None, depth, inherited, cx)),
            k if units::is_atomic(k) => self.render_unit(b, depth, paint, cx),
            _ => {
                let els: Vec<AnyElement> =
                    b.children.iter().filter_map(|c| self.render_block(c, depth, paint.text, cx)).collect();
                Some(Self::painted(v_flex().w_full().gap(px(8.)).children(els), paint).into_any_element())
            }
        }
    }

    /// 块颜色的画法（7.7）：背景铺满块矩形、左右各多 8 px；子块继承同一段文字色。
    fn painted<E: Styled>(mut el: E, paint: Paint) -> E {
        if let Some(bg) = paint.background {
            el = el.bg(bg).rounded(px(6.)).mx(px(-8.)).px(px(8.));
        }
        if let Some(color) = paint.text {
            el = el.text_color(color);
        }
        el
    }

    fn render_list_item(
        &self,
        item: &Block,
        number: Option<u32>,
        depth: usize,
        inherited: Option<Hsla>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let paint = crate::colors::of(&item.attrs, cx).inherit(inherited);
        let (muted, selection) = (cx.theme().muted_foreground, cx.theme().selection);
        let content: Vec<&Block> = item.children.iter().filter(|c| c.kind != BlockKind::Marker).collect();
        let task = match item.kind {
            BlockKind::ListItem { task } => task,
            _ => None,
        };
        let marker: AnyElement = match (task, number) {
            (Some(checked), _) => self.render_task_checkbox(item.id, checked, cx),
            (None, Some(n)) => div().text_color(muted).child(format!("{n}.")).into_any_element(),
            (None, None) => div()
                .text_color(muted)
                .child(match depth % 3 {
                    0 => "•",
                    1 => "◦",
                    _ => "▪",
                })
                .into_any_element(),
        };

        let body: AnyElement = if content.is_empty() {
            let id = item.id;
            let key = presenter::Key::unit(id);
            let body = if let Some(text) = self.render_presenter(key, cx) {
                self.unit_frame(id, false, Paint::default(), text, cx)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.focus_presenter_near(key, event.position, window, cx);
                        }),
                    )
                    .into_any_element()
            } else if self.is_focused_block(id) {
                self.render_textarea(None, cx)
            } else {
                let selected = self.is_selected_block(id);
                div()
                    .id(("empty-item", id.0))
                    .debug_selector(|| format!("unit-{}", id.0))
                    .w_full()
                    .h(px(BODY_FONT_SIZE * BODY_LINE_HEIGHT))
                    .cursor_text()
                    .when(selected, |el| Self::selected_style(el, selection))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| this.focus_block(id, window, cx)),
                    )
                    .into_any_element()
            };
            self.with_gutter(id, depth + 1, body, cx)
        } else {
            let els: Vec<AnyElement> =
                content.iter().filter_map(|c| self.render_block(c, depth + 1, paint.text, cx)).collect();
            v_flex().w_full().gap(px(4.)).children(els).into_any_element()
        };

        Self::painted(h_flex().w_full().items_start().gap(px(4.)), paint)
            .child(
                div()
                    .flex_none()
                    .w(px(24.))
                    .h(px(BODY_FONT_SIZE * BODY_LINE_HEIGHT))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(marker),
            )
            .child(div().flex_1().min_w_0().child(body))
            .into_any_element()
    }

    /// 任务的方框：点一下在 `[ ]` 和 `[x]` 之间切换，不进编辑态，也不挪动任何文字。
    fn render_task_checkbox(&self, item: BlockId, checked: bool, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (primary, border, fg) =
            (theme.primary, theme.muted_foreground.opacity(0.7), theme.primary_foreground);
        div()
            .id(("task", item.0))
            .debug_selector(move || format!("task-{}", item.0))
            .size(px(16.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .border_1()
            .cursor_pointer()
            .map(|el| if checked { el.bg(primary).border_color(primary) } else { el.border_color(border) })
            .hover(move |el| if checked { el } else { el.border_color(primary) })
            .when(checked, |el| {
                el.child(
                    gpui_kit::component::Icon::new(gpui_kit::assets::IconName::Check)
                        .size(px(12.))
                        .text_color(fg),
                )
            })
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                cx.stop_propagation();
                window.prevent_default();
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle_task(item, cx)))
            .into_any_element()
    }

    /// 勾选 / 取消勾选一个任务：只改 `[ ]` 里那一个字符，长度不变，编辑中的光标也不用挪。
    pub(super) fn toggle_task(&mut self, item: BlockId, cx: &mut Context<Self>) {
        let Some(block) = self.doc.blocks().find(item) else { return };
        let range = self.doc.range_of(block);
        let src = self.doc.slice(range.clone()).to_string();
        let Some(edit) = ops::toggle_task(&src, range.start) else { return };
        if let Err(err) = self.doc.edit(edit) {
            log::error!("edit rejected: {err}");
            return;
        }
        self.last_edit_at = None;
        self.after_doc_change(cx);
    }

    /// 悬停出现的块 gutter（5.3）：`⠿` 进入块选中态，`+` 在下方插入块并打开 `/` 菜单。
    /// 行向左伸出 gutter 的宽度（嵌套列表再加上标记列），按钮画在伸出去的那段里：
    /// 鼠标从块移到按钮上仍在同一个悬停组内，按钮不会在半路消失；各层级的按钮左对齐。
    fn with_gutter(&self, id: BlockId, depth: usize, content: AnyElement, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let reach = px(GUTTER + LIST_INDENT * depth as f32);
        let (muted, hover) = (theme.muted_foreground, theme.list_hover);
        let button = |name: &'static str, glyph: &'static str, title: &'static str| {
            div()
                .id((name, id.0))
                .debug_selector(move || format!("{name}-{}", id.0))
                .flex_none()
                .size(px(20.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .text_size(px(14.))
                .text_color(muted)
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(title).build(window, cx)
                })
                .child(glyph)
        };
        let editor = cx.entity();
        let handle = button("gutter-handle", "⠿", "拖动重排，点击打开菜单")
            .on_drag(block_menu::BlockDrag { id }, move |drag, _, window, cx| {
                let cursor = window.mouse_position();
                editor.update(cx, |this, _| this.preview_block_drag(cursor));
                let text = editor.read(cx).block_preview_text(drag.id);
                cx.new(|_| block_menu::BlockDragGhost::new(text))
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if cx.has_active_drag() || this.dragging_block {
                    return;
                }
                cx.stop_propagation();
                let origin = match event {
                    ClickEvent::Mouse(click) => click.down.position,
                    ClickEvent::Keyboard(key) => key.bounds.center(),
                    ClickEvent::Touch(touch) => touch.position,
                };
                this.select_block_by_id(id, window, cx);
                this.open_block_menu(id, origin, window, cx);
            }));
        let plus = button("gutter-plus", "+", "在下方插入块").on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.insert_block_below(id, window, cx);
            }),
        );
        h_flex()
            .debug_selector(move || format!("row-{}", id.0))
            .w_full()
            .items_start()
            .relative()
            .group("unit")
            .ml(-reach)
            .pl(reach)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.select_block_by_id(id, window, cx);
                    this.open_block_menu(id, event.position, window, cx);
                }),
            )
            .on_children_prepainted({
                let this = cx.weak_entity();
                move |bounds, _, cx| {
                    if let Some(bounds) = bounds.last().copied() {
                        let _ = this.update(cx, |editor, _| editor.remember_unit_bounds(id, bounds));
                    }
                }
            })
            .child(
                h_flex()
                    .absolute()
                    .left_0()
                    .top(px(4.))
                    .w(px(GUTTER))
                    .gap(px(4.))
                    .opacity(0.)
                    .group_hover("unit", |s| s.opacity(1.))
                    .child(handle)
                    .child(plus),
            )
            .child(div().flex_1().min_w_0().child(content))
            .into_any_element()
    }

    fn render_callout_block(&self, block: &Block, cx: &mut Context<Self>) -> AnyElement {
        let range = self.doc.content_range(block);
        let src = self.doc.slice(range.clone()).to_string();
        let Some(parts) = callout_edit_parts(&src) else {
            return self.render_textarea(Some(&block.kind), cx);
        };
        let (label, icon, color) = preview::appearance(&parts.kind, cx);
        let foreground = cx.theme().foreground;
        let id = block.id;
        let body_abs = range.start + parts.body.start..range.start + parts.body.end;
        let title_key = presenter::Key { id, part: presenter::Part::CalloutTitle };
        let body_key = presenter::Key { id, part: presenter::Part::CalloutBody };
        let editing_body = self.focused.as_ref().is_some_and(|f| f.id == Some(id) && f.range == body_abs);
        let foldable = parts.fold != aero_core::Fold::None;
        let revealing = self.highlight == Some(id);
        let open = !foldable || editing_body || revealing || self.callout_open(id, parts.fold);
        let line = px(BODY_FONT_SIZE * BODY_LINE_HEIGHT);
        let title_text = src[parts.title.clone()].trim().to_owned();
        let name = SharedString::from(if title_text.is_empty() { label.to_owned() } else { title_text });
        let title_el =
            self.render_presenter(title_key, cx).unwrap_or_else(|| div().child(label).into_any_element());
        let body_el: Option<AnyElement> = if !open || (parts.body.is_empty() && !editing_body) {
            None
        } else if let Some(text) = self.render_presenter(body_key, cx) {
            Some(text)
        } else if editing_body {
            Some(self.render_textarea(None, cx))
        } else {
            // 正文里有列表、代码这类结构：浏览时照常渲染，点进去编辑这一段源码
            let body_click = body_abs.clone();
            Some(
                div()
                    .id(SharedString::from(format!("callout-body-hit-{}", id.0)))
                    .w_full()
                    .cursor_text()
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        if event.is_right_click() {
                            return;
                        }
                        cx.stop_propagation();
                        this.focus_range(body_click.clone(), Some(id), true, 0, window, cx);
                    }))
                    .child(preview::markdown(
                        SharedString::from(format!("callout-body-{}", id.0)),
                        aero_core::links::to_markdown(&parts.body_text).into(),
                        self.text_view_style(cx),
                        if revealing { self.reveal_generation } else { 0 },
                        self.preview.clone(),
                        cx.entity(),
                        id,
                        0,
                    ))
                    .into_any_element(),
            )
        };
        let toggle = foldable.then(|| {
            let (name, selector) = (name.clone(), name.clone());
            div()
                .debug_selector(move || format!("callout-toggle-{selector}"))
                .flex_none()
                .h(line)
                .flex()
                .items_center()
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    cx.stop_propagation();
                    window.prevent_default();
                })
                .child(
                    Button::new(SharedString::from(format!("callout-fold-{}", id.0)))
                        .ghost()
                        .xsmall()
                        .icon(
                            gpui_kit::component::Icon::new(if open {
                                gpui_kit::assets::IconName::ChevronDown
                            } else {
                                gpui_kit::assets::IconName::ChevronRight
                            })
                            .text_color(color),
                        )
                        .tooltip(if open { "折叠" } else { "展开" })
                        .accessibility_label(format!("{} {name}", if open { "折叠" } else { "展开" }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle_callout(id, parts.fold, cx);
                        })),
                )
        });
        v_flex()
            .id(SharedString::from(format!("callout-{}", id.0)))
            .debug_selector(move || format!("callout-{}", id.0))
            .w_full()
            .gap_1()
            .px_3()
            .py_2()
            .rounded(px(6.))
            .border_l_3()
            .border_color(color)
            .bg(color.opacity(0.08))
            .text_color(foreground)
            .child(
                h_flex()
                    .w_full()
                    .items_start()
                    .gap_2()
                    .text_color(color)
                    .child(
                        div()
                            .flex_none()
                            .h(line)
                            .flex()
                            .items_center()
                            .child(gpui_kit::component::Icon::new(icon).size(px(18.))),
                    )
                    .child({
                        let name = name.clone();
                        div()
                            .debug_selector(move || format!("callout-title-{name}"))
                            .flex_1()
                            .min_w_0()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title_el)
                    })
                    .children(toggle),
            )
            .children(body_el.map(|body| {
                div().debug_selector(move || format!("callout-body-{name}")).w_full().min_w_0().child(body)
            }))
            .into_any_element()
    }

    /// 可折叠的 callout 现在是否展开。折叠状态不写回源码。
    fn callout_open(&self, id: BlockId, fold: aero_core::Fold) -> bool {
        self.callout_folds.get(&id).copied().unwrap_or(fold != aero_core::Fold::Collapsed)
    }

    fn toggle_callout(&mut self, id: BlockId, fold: aero_core::Fold, cx: &mut Context<Self>) {
        let open = self.callout_open(id, fold);
        self.callout_folds.insert(id, !open);
        cx.notify();
    }

    /// 一个编辑单位：聚焦则画控件，否则渲染。
    fn render_unit(
        &self,
        b: &Block,
        depth: usize,
        paint: Paint,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let range = self.doc.content_range(b);
        if let Some(el) = self.render_presented_unit(b, depth, paint, cx) {
            return Some(el);
        }
        if self.is_focused_block(b.id) && !self.focused.as_ref().is_some_and(|focused| focused.math_popup) {
            let full = self.doc.slice(range.clone()).to_string();
            if self.renders_inline_formulas(b.id, &full, cx)
                && let Some(mixed) = self.render_with_inline_math(
                    &full,
                    range.start,
                    b.id,
                    self.text_view_style(cx),
                    SharedString::from(format!("blk-{}-{}", cx.entity_id(), b.id.0)),
                    self.focused.as_ref().map(|focused| focused.range.clone()),
                    cx,
                )
            {
                let body = Self::painted(
                    div().w_full().when(matches!(b.kind, BlockKind::Heading { .. }), |el| el.pt_2()),
                    paint,
                )
                .child(mixed);
                return Some(self.with_gutter(b.id, depth, body.into_any_element(), cx));
            }
            if matches!(b.kind, BlockKind::Callout { .. })
                && self.hides_markers(cx)
                && !self.block_shows_source(b.id)
            {
                return Some(self.with_gutter(b.id, depth, self.render_callout_block(b, cx), cx));
            }
            // 和未聚焦时同一套外框，点进去文字位置不变。
            let textarea = self.render_textarea(Some(&b.kind), cx);
            let textarea = if matches!(b.kind, BlockKind::BlockQuote)
                && self.hides_markers(cx)
                && !self.block_shows_source(b.id)
            {
                let theme = cx.theme();
                div()
                    .w_full()
                    .text_color(theme.muted_foreground)
                    .border_l_3()
                    .border_color(theme.border)
                    .px_4()
                    .child(textarea)
                    .into_any_element()
            } else {
                textarea
            };
            // 源码态也保持块颜色：属性列表本身是尾部标记，文本框里看不到它。
            // 标题上方的空隙和预览一样留着，否则点进去下面的块会往上跳。
            let textarea = Self::painted(
                div().w_full().when(matches!(b.kind, BlockKind::Heading { .. }), |el| el.pt_2()),
                paint,
            )
            .child(textarea);
            // 公式块在源码下面保留实时预览，改完立刻重排
            let body = if matches!(b.kind, BlockKind::MathBlock) {
                let source = self.doc.slice(range).to_string();
                v_flex()
                    .w_full()
                    .gap_2()
                    .child(textarea)
                    .child(self.render_math_block(&source, cx))
                    .into_any_element()
            } else if aero_core::rich_content::mermaid_source(&full).is_some() {
                v_flex()
                    .w_full()
                    .gap_2()
                    .child(textarea)
                    .child(self.render_diagram_preview(b.id, cx))
                    .into_any_element()
            } else {
                textarea.into_any_element()
            };
            return Some(self.with_gutter(b.id, depth, body, cx));
        }
        if self.is_covered(&range) && !self.is_focused_block(b.id) {
            return None;
        }
        let theme = cx.theme().clone();
        let base = range.start;
        let text: SharedString = self.doc.slice(range).to_string().into();
        let id = b.id;
        // 用 click 而不是 mouse down 进编辑态：预览里的链接在 mouse up 时才触发并 stop_propagation，
        // 若 mouse down 就把块换成文本框，链接的 mouse up 落不到 TextView 上，点 wikilink 只会进编辑态。
        let focus = cx.listener(move |this: &mut Self, ev: &ClickEvent, window, cx| {
            if ev.is_right_click() {
                return;
            }
            this.focus_block_at(id, ev.position(), window, cx)
        });

        let body: AnyElement = match &b.kind {
            BlockKind::CodeFence { .. } if aero_core::rich_content::mermaid_source(&text).is_some() => {
                self.render_diagram(&text, id, true, cx)
            }
            BlockKind::Paragraph if aero_core::rich_content::pdf_embed(&text).is_some() => {
                self.render_pdf(&text, id, Some(&self.path), true, cx)
            }
            BlockKind::ThematicBreak => div().w_full().h(px(1.)).my_3().bg(theme.border).into_any_element(),
            BlockKind::MathBlock => self.render_math_block(&text, cx),
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
                // wikilink 在预览里渲染成标准 Markdown 链接（aero://note/…），点击经 on_link_click 回编辑器
                let rendered: SharedString = aero_core::links::to_markdown(&text).into();
                // TextView 的解析状态按元素 id 缓存；带上编辑器实体 id，切换笔记后不会复用上一篇的状态
                let view_id = SharedString::from(format!("blk-{}-{}", cx.entity_id(), id.0));
                let reveal = if self.highlight == Some(id) { self.reveal_generation } else { 0 };
                // TextView 自带正文色，块颜色要写进它的样式，容器上的 text_color 进不去
                let style = self.text_view_style(cx);
                let style = match paint.text {
                    Some(color) => style.with_foreground(color),
                    None => style,
                };
                if matches!(
                    b.kind,
                    BlockKind::Paragraph | BlockKind::Heading { .. } | BlockKind::ListItem { .. }
                ) && let Some(mixed) =
                    self.render_with_inline_math(&text, base, id, style.clone(), view_id.clone(), None, cx)
                {
                    mixed
                } else {
                    preview::markdown(
                        view_id,
                        rendered,
                        style,
                        reveal,
                        self.preview.clone(),
                        cx.entity(),
                        id,
                        0,
                    )
                    .into_any_element()
                }
            }
        };
        let is_heading = matches!(b.kind, BlockKind::Heading { .. });
        let unit = self.unit_frame(id, is_heading, paint, body, cx).on_click(focus).into_any_element();
        Some(self.with_gutter(id, depth, unit, cx))
    }

    /// 编辑单位的外框：块颜色、定位高亮、块选中底色，以及定位 / 颜色菜单要的矩形。
    fn unit_frame(
        &self,
        id: BlockId,
        is_heading: bool,
        paint: Paint,
        body: AnyElement,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let theme = cx.theme();
        let highlighted = self.highlight == Some(id);
        let selected = self.is_selected_block(id);
        let background = if highlighted {
            Some(theme.accent)
        } else if selected {
            Some(theme.selection)
        } else {
            paint.background
        };
        // 定位中的块：多套一层拿子元素布局（`on_children_prepainted` 只在无状态 div 上有）
        let body: AnyElement = if self.reveal == Some(id) {
            div().w_full().on_children_prepainted(self.reveal_listener(id, cx)).child(body).into_any_element()
        } else {
            body
        };
        // 选区头这一块每帧记下自己的矩形：块选中态里颜色菜单要贴在它下面
        let body: AnyElement = if self.selection_head() == Some(id) {
            let cell = self.head_bounds.clone();
            div()
                .w_full()
                .on_children_prepainted(move |bounds, _, _| cell.set(bounds.first().copied()))
                .child(body)
                .into_any_element()
        } else {
            body
        };
        div()
            .id(("unit", id.0))
            .debug_selector(|| format!("unit-{}", id.0))
            .relative()
            .w_full()
            .min_h(px(BODY_FONT_SIZE * BODY_LINE_HEIGHT))
            .when(is_heading, |el| el.pt_2())
            .cursor_text()
            .when_some(paint.text, |el, color| el.text_color(color))
            // 背景向两侧延伸，但不能用 padding 挤窄自适应内容。
            .when_some(background, |el, color| {
                el.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(-8.))
                        .right(px(-8.))
                        .rounded(px(6.))
                        .bg(color),
                )
            })
            .child(body)
    }

    /// 段落、标题、引用：浏览与编辑是同一个控件，外框也是同一个。
    fn render_presented_unit(
        &self,
        b: &Block,
        depth: usize,
        paint: Paint,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.hides_markers(cx) || self.block_shows_source(b.id) {
            return None;
        }
        let has = |part| self.presenters.contains_key(&presenter::Key { id: b.id, part });
        let body = match &b.kind {
            BlockKind::Table if has(presenter::Part::Cell(0, 0)) => {
                let table = self.render_table_block(b, cx);
                self.unit_frame(b.id, false, paint, table, cx).into_any_element()
            }
            BlockKind::Callout { .. } if has(presenter::Part::CalloutTitle) => {
                let callout = self.render_callout_block(b, cx);
                self.unit_frame(b.id, false, paint, callout, cx).into_any_element()
            }
            BlockKind::CodeFence { .. } => {
                let key = presenter::Key::unit(b.id);
                let text = self.render_presenter(key, cx)?;
                let theme = cx.theme();
                let code = div()
                    .w_full()
                    .font_family(theme.mono_font_family.clone())
                    .text_size(theme.mono_font_size)
                    .line_height(relative(BODY_LINE_HEIGHT))
                    .bg(theme.muted)
                    .rounded(theme.semantic_tokens().radius.md)
                    .p_3()
                    .child(text);
                self.unit_frame(b.id, false, paint, code.into_any_element(), cx)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.focus_presenter_near(key, event.position, window, cx);
                        }),
                    )
                    .into_any_element()
            }
            BlockKind::Paragraph | BlockKind::Heading { .. } | BlockKind::BlockQuote => {
                let key = presenter::Key::unit(b.id);
                let text = self.render_presenter(key, cx)?;
                let theme = cx.theme();
                let inner = match &b.kind {
                    BlockKind::Heading { level } => div()
                        .w_full()
                        .text_size(heading_size(*level))
                        .line_height(relative(BODY_LINE_HEIGHT))
                        .font_weight(if *level == 1 { FontWeight::BOLD } else { FontWeight::SEMIBOLD })
                        .pb(rems(0.3))
                        .child(text),
                    BlockKind::BlockQuote => div()
                        .w_full()
                        .text_color(theme.muted_foreground)
                        .border_l_3()
                        .border_color(theme.border)
                        .px_4()
                        .child(text),
                    _ => div().w_full().child(text),
                };
                let is_heading = matches!(b.kind, BlockKind::Heading { .. });
                self.unit_frame(b.id, is_heading, paint, inner.into_any_element(), cx)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.focus_presenter_near(key, event.position, window, cx);
                        }),
                    )
                    .into_any_element()
            }
            _ => return None,
        };
        Some(self.with_gutter(b.id, depth, body, cx))
    }
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::colors::apply_text_selection(cx);
        self.inline_math_hits.clear();
        if self.dragging_block && !cx.has_active_drag() {
            self.dragging_block = false;
            self.block_drop = None;
        }
        // 在编辑区外松手收不到 drop，按拖动结束处理
        if self.table_resize.is_some() && !cx.has_active_drag() {
            self.finish_table_resize(cx);
        }
        self.validate_link_preview_scroll(cx);
        self.refresh_fragment_marks();
        self.sync_presenters(window, cx);
        self.sync_diagram_preview(false, cx);
        self.rich.pdf_readers.borrow_mut().retain(|id, _| {
            self.doc.blocks().roots.iter().any(|block| {
                block.id == *id
                    && block.kind == BlockKind::Paragraph
                    && aero_core::rich_content::pdf_embed(
                        &self.doc.slice(self.doc.content_range(block)).to_string(),
                    )
                    .is_some()
            })
        });
        self.scale = window.scale_factor();
        let (foreground, muted) = (cx.theme().foreground, cx.theme().muted_foreground);
        let roots = self.doc.blocks().roots.clone();
        let blocks = self.render_blocks(&roots, cx);
        let empty = self.doc.blocks().is_empty() && self.focused.is_none();

        div()
            .id("editor")
            .debug_selector(|| "editor".into())
            .key_context("Editor")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_block_up))
            .on_action(cx.listener(Self::on_block_down))
            .on_action(cx.listener(Self::on_split_block))
            .on_action(cx.listener(Self::on_undo))
            .on_action(cx.listener(Self::on_redo))
            .on_action(cx.listener(Self::on_backspace))
            .capture_action(
                cx.listener(|this, _: &Backspace, window, cx| this.capture_deletion(false, window, cx)),
            )
            .capture_action(
                cx.listener(|this, _: &Delete, window, cx| this.capture_deletion(true, window, cx)),
            )
            .on_action(cx.listener(Self::on_convert_block))
            .on_action(cx.listener(Self::on_menu_confirm))
            .on_action(cx.listener(Self::on_link_alias))
            .on_action(cx.listener(Self::on_edit_link))
            .on_action(cx.listener(Self::on_format_inline))
            .on_action(cx.listener(Self::on_apply_last_color))
            .on_action(cx.listener(Self::on_open_color_menu))
            .on_action(cx.listener(Self::on_escape))
            .on_action(cx.listener(Self::on_blur_editor))
            .on_action(cx.listener(Self::on_select_up))
            .on_action(cx.listener(Self::on_select_down))
            .on_action(cx.listener(Self::on_extend_up))
            .on_action(cx.listener(Self::on_extend_down))
            .on_action(cx.listener(Self::on_edit_selected))
            .on_action(cx.listener(Self::on_pdf_previous))
            .on_action(cx.listener(Self::on_pdf_next))
            .on_action(cx.listener(Self::on_open_pdf))
            .on_action(cx.listener(Self::on_delete_selected))
            .on_action(cx.listener(Self::on_copy_selected))
            .on_action(cx.listener(Self::on_cut_selected))
            .on_action(cx.listener(Self::on_paste_attachment))
            .on_drop(cx.listener(|this, paths: &gpui_kit::ExternalPaths, window, cx| {
                this.drop_external_files(paths, window, cx);
            }))
            .on_drop(cx.listener(|this, drag: &crate::file_tree::TreeDrag, window, cx| {
                this.drop_tree_note(drag, window, cx);
            }))
            .can_drop(|value, _, _| {
                value.downcast_ref::<gpui_kit::ExternalPaths>().is_some()
                    || value.downcast_ref::<crate::file_tree::TreeDrag>().is_some_and(|drag| !drag.directory)
                    || value.downcast_ref::<block_menu::BlockDrag>().is_some()
                    || value.downcast_ref::<table::TableResize>().is_some()
            })
            .on_drop(cx.listener(|this, _: &table::TableResize, _, cx| this.finish_table_resize(cx)))
            .on_drag_move(cx.listener(|this, event: &DragMoveEvent<table::TableResize>, _, cx| {
                this.track_table_resize(event.event.position.x, cx);
            }))
            .on_drop(cx.listener(|this, drag: &block_menu::BlockDrag, window, cx| {
                this.finish_block_drag(drag, window, cx);
            }))
            .on_drag_move(cx.listener(|this, event: &DragMoveEvent<block_menu::BlockDrag>, _, cx| {
                this.track_block_drag(event.event.position, cx);
            }))
            .drag_over::<gpui_kit::ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().list_active))
            .drag_over::<crate::file_tree::TreeDrag>(|style, drag, _, cx| {
                if drag.directory { style } else { style.bg(cx.theme().list_active) }
            })
            .size_full()
            .overflow_y_scroll()
            // 横向滑动留给宽表格自己滚，不折算成正文的上下滚动
            .restrict_scroll_to_axis()
            .track_scroll(&self.scroll)
            .on_scroll_wheel(cx.listener(|this, _, _, cx| {
                this.cancel_link_preview(cx);
                this.toolbar_scrolled(cx);
            }))
            .on_modifiers_changed(cx.listener(|this, event: &ModifiersChangedEvent, _, cx| {
                this.preview_modifiers(event.modifiers, cx);
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| this.show_toolbar_now(cx)))
            .child(
                // 左内边距多留 gutter 的宽度：块行向左伸出去画 `⠿ +`，正文仍是 720 宽
                v_flex()
                    .w_full()
                    .max_w(px(COLUMN_WIDTH + GUTTER + 24. + GUTTER + 24.))
                    .mx_auto()
                    .pl(px(24. + GUTTER))
                    .pr_6()
                    .pt_10()
                    .text_size(px(BODY_FONT_SIZE))
                    .line_height(relative(BODY_LINE_HEIGHT))
                    .text_color(foreground)
                    .children(self.render_banner(cx))
                    .child(self.render_title(cx))
                    .children(self.render_backlink_count(cx))
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
            .children(self.render_link_preview(window, cx))
            .children(self.render_toolbar(cx))
            .children(self.render_block_menu(cx))
            .children(self.render_block_drop_line(cx))
            .children(self.render_math_editor(cx))
            // 颜色菜单画在编辑器根上：块选中态没有控件可挂，位置由 `color_menu_position` 给
            .children(self.color_menu.is_some().then(|| self.render_color_menu(cx)))
            .children(self.render_image_viewer(window, cx))
    }
}

/// 标题比对用的键：去行内标记与 wikilink 括号、压掉多余空白、小写。
fn heading_key(text: &str) -> String {
    let plain = aero_core::index::plain_text(text, &BlockKind::Paragraph);
    plain.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

struct CalloutEdit {
    kind: aero_core::CalloutKind,
    fold: aero_core::Fold,
    title: Range<usize>,
    body: Range<usize>,
    body_text: String,
    /// 正文只有段落，按文字画；有列表、代码、表格时仍用源码编辑
    simple_body: bool,
}

fn callout_edit_parts(src: &str) -> Option<CalloutEdit> {
    let header_end = src.find('\n').unwrap_or(src.len());
    let header = &src[..header_end];
    let (kind, fold, _, _) = aero_core::parse::callout_header(header)?;
    let close = header.find(']')?;
    let mut title_at = close + 1;
    let bytes = header.as_bytes();
    if matches!(bytes.get(title_at), Some(b'-' | b'+')) {
        title_at += 1;
    }
    while bytes.get(title_at) == Some(&b' ') {
        title_at += 1;
    }
    let body = if header_end < src.len() { header_end + 1..src.len() } else { src.len()..src.len() };
    let body_src = src.get(body.clone()).unwrap_or("");
    let body_text = body_src.lines().map(strip_quote_marker).collect::<Vec<_>>().join("\n");
    let simple_body = body_src.lines().all(|line| {
        line.trim_start().starts_with('>') && presenter::simple_body_line(strip_quote_marker(line))
    });
    Some(CalloutEdit { kind, fold, title: title_at..header_end, body, body_text, simple_body })
}

fn strip_quote_marker(line: &str) -> &str {
    line.trim_start()
        .strip_prefix('>')
        .map(|rest| rest.strip_prefix([' ', '\t']).unwrap_or(rest))
        .unwrap_or(line)
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
