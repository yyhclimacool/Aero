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
mod block_refs_ui;
mod color_menu;
mod colors_ui;
mod embeds_ui;
mod highlight_ui;
mod images_ui;
mod inline_format_ui;
mod inline_links_ui;
mod link_preview;
mod math_ui;
mod ops;
mod pairs;
mod pairs_ui;
mod preview;
mod slash;
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
use std::ops::Range;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aero_core::{Block, BlockId, BlockKind, Document, Edit, FileStamp, Vault, VaultError, VaultPath};
use gpui_kit::base::input::{Backspace, Escape, Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::base::text::TextViewStyle;
use gpui_kit::component::button::Button;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use images_ui::ImageViewer;

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
            viewer: None,
            scale: 1.,
            focused: None,
            selection: None,
            picker_open: false,
            title_edit: None,
            slash: None,
            color_menu: None,
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
        self.cancel_link_preview(cx);
        let text = self.doc.slice(range.clone()).to_string();
        let cursor = clamp_to_char_boundary(&text, cursor.min(text.len()));
        let textarea = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 100_000));
        let subscription = cx.subscribe_in(&textarea, window, Self::on_input_event);
        let editor = cx.weak_entity();
        let pair_keys = cx.intercept_keystrokes(move |event, window, cx| {
            let _ = editor.update(cx, |this, cx| this.observe_pair_key(event, window, cx));
        });
        let mut last_selection = None;
        let cursor_subscription = cx.observe_in(&textarea, window, move |this, textarea, window, cx| {
            if this.focused.as_ref().is_some_and(|f| f.textarea == textarea) {
                let selection = textarea.read(cx).selected_range();
                if last_selection.as_ref() != Some(&selection) {
                    last_selection = Some(selection);
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
        textarea.update(cx, |ta, cx| {
            ta.set_value(text, window, cx);
            ta.set_selected_range(cursor..cursor, cx);
            ta.focus(window, cx);
        });
        self.last_edit_at = None;
        log::debug!("focus {:?} id={:?} cursor={cursor}", range, id);
        self.selection = None;
        self.wiki = None;
        self.tag_menu = None;
        self.focused = Some(Focused {
            range,
            id,
            enter_newline,
            textarea,
            pairs: Default::default(),
            highlight: Default::default(),
            _pair_keys: pair_keys,
            _subscription: subscription,
            _cursor_subscription: cursor_subscription,
        });
        cx.notify();
    }

    /// 退出编辑态（不改焦点）。失焦即落盘（10.1）。
    fn exit_edit(&mut self, cx: &mut Context<Self>) {
        self.cancel_link_preview(cx);
        self.slash = None;
        self.color_menu = None;
        self.hide_toolbar(cx);
        self.wiki = None;
        self.tag_menu = None;
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

    /// gutter 的 `⠿`：进入块选中态（块菜单与拖拽重排不在 M1）。
    fn select_block_by_id(&mut self, id: BlockId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.unit_index(id) {
            self.select_blocks(ix, ix, window, cx);
        }
    }

    /// gutter 的 `+`：在块下方新开段落并打开 `/` 菜单。
    fn insert_block_below(&mut self, id: BlockId, window: &mut Window, cx: &mut Context<Self>) {
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
            f.id = unit_at(&self.units, f.range.start)
                .filter(|&ix| self.units[ix].range.start == f.range.start)
                .map(|ix| self.units[ix].id);
        }
        self.clamp_selection();
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
        if self.input_composing(window, cx) {
            cx.propagate();
            return;
        }
        if self.move_tag(-1, cx) || self.move_wiki(-1, cx) {
            return;
        }
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
        if self.input_composing(window, cx) {
            cx.propagate();
            return;
        }
        if self.move_tag(1, cx) || self.move_wiki(1, cx) {
            return;
        }
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

    /// 同步落盘（切换笔记、退出前）。有外部修改时同样先留冲突副本，返回其路径。
    /// 文件已在外部被删时不写回（与 [`Self::save_now`] 一致），未保存内容随编辑器丢弃。
    pub fn flush_sync(&mut self) -> Result<Option<VaultPath>, VaultError> {
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
        self.doc = doc;
        self.update_local_link_names();
        self.stamp = stamp;
        self.units = units::units(&self.doc);
        self.refresh_embeds(cx);
        self.clamp_selection();
        self.preview.update(cx, |state, _| *state = preview::PreviewState::default());
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
    fn is_selected_block(&self, id: BlockId) -> bool {
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
            .child(Textarea::new(&f.textarea));
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
            let id = item.id;
            let body = if self.is_focused_block(id) {
                self.render_textarea(None, cx)
            } else {
                let selected = self.is_selected_block(id);
                let selection = theme.selection;
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
            .child(div().flex_none().w(px(24.)).text_center().child(marker))
            .child(div().flex_1().min_w_0().child(body))
            .into_any_element()
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
        let handle = button("gutter-handle", "⠿", "选中块").on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.select_block_by_id(id, window, cx);
            }),
        );
        let plus = button("gutter-plus", "+", "在下方插入块").on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.insert_block_below(id, window, cx);
            }),
        );
        h_flex()
            .w_full()
            .items_start()
            .relative()
            .group("unit")
            .ml(-reach)
            .pl(reach)
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

    /// 一个编辑单位：聚焦则画控件，否则渲染。
    fn render_unit(
        &self,
        b: &Block,
        depth: usize,
        paint: Paint,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let range = self.doc.content_range(b);
        if self.is_focused_block(b.id) {
            // 源码态也保持块颜色：属性列表本身是尾部标记，文本框里看不到它
            let textarea =
                Self::painted(div().w_full(), paint).child(self.render_textarea(Some(&b.kind), cx));
            return Some(self.with_gutter(b.id, depth, textarea.into_any_element(), cx));
        }
        if self.is_covered(&range) {
            return None;
        }
        let theme = cx.theme().clone();
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
                preview::markdown(view_id, rendered, style, reveal, self.preview.clone(), cx.entity(), id, 0)
                    .into_any_element()
            }
        };
        let is_heading = matches!(b.kind, BlockKind::Heading { .. });
        let highlighted = self.highlight == Some(id);
        let selected = self.is_selected_block(id);
        let transient = highlighted || selected;
        let (accent, selection) = (theme.accent, theme.selection);
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
        let unit = div()
            .id(("unit", id.0))
            .debug_selector(|| format!("unit-{}", id.0))
            .w_full()
            .min_h(px(BODY_FONT_SIZE * BODY_LINE_HEIGHT))
            .when(is_heading, |el| el.pt_2())
            .cursor_text()
            // 定位高亮与块选中是临时状态，背景压过块颜色；块的文字色照常
            .map(|el| {
                Self::painted(el, Paint { background: paint.background.filter(|_| !transient), ..paint })
            })
            .when(highlighted, |el| el.bg(accent).rounded(px(6.)).mx(px(-8.)).px(px(8.)))
            .when(selected && !highlighted, |el| Self::selected_style(el, selection))
            .on_click(focus)
            .child(body)
            .into_any_element();
        Some(self.with_gutter(id, depth, unit, cx))
    }
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.validate_link_preview_scroll(cx);
        self.scale = window.scale_factor();
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
            .on_action(cx.listener(Self::on_delete_selected))
            .on_action(cx.listener(Self::on_copy_selected))
            .on_action(cx.listener(Self::on_cut_selected))
            .on_action(cx.listener(Self::on_paste_attachment))
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .on_scroll_wheel(cx.listener(|this, _, _, cx| {
                this.cancel_link_preview(cx);
                this.toolbar_scrolled(cx);
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
            .children(self.render_link_preview(cx))
            .children(self.render_toolbar(cx))
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
