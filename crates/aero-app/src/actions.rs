//! 动作与快捷键（`ui_design.md` 第 14 节，M1a 子集）。
//!
//! 编辑器相关的绑定放在 `Input` 上下文里、并在 `gpui_kit::init` **之后**注册：
//! GPUI 对同一上下文深度的绑定按注册顺序取后者，这样 `up` / `down` / `cmd-z` 先到我们手里，
//! 处理不了（光标不在首行 / 末行）时 `cx.propagate()` 再交还给 gpui-base 的文本控件。

use gpui_kit::*;
use std::rc::Rc;

actions!(
    aero,
    [
        Quit,
        /// 关于 Aero
        About,
        /// 检查这个构建是否带更新渠道
        CheckForUpdates,
        /// ⌘,：设置
        OpenSettings,
        /// ⌘H：隐藏 Aero
        Hide,
        /// ⌥⌘H：隐藏其他应用
        HideOthers,
        /// 全部显示
        ShowAll,
        /// 打开文件夹作为 vault
        OpenVault,
        /// 新建 vault（选位置，创建 daily/ 与欢迎笔记）
        NewVault,
        /// 回到首次运行页
        CloseVault,
        NewNote,
        ImportNoteCopy,
        /// 今日日记 `daily/YYYY-MM-DD.md`，不存在则创建（3.4）
        OpenDaily,
        ToggleSidebar,
        TogglePanel,
        /// 手动立即保存
        SaveNow,
        /// 把文件树里选中的笔记（没有则是打开中的笔记）移到废纸篓
        DeleteNote,
        /// ⌘⇧F：左侧栏切到搜索视图并聚焦输入框（`ui_design.md` 4.2）
        OpenSearch,
        /// 本地语义搜索、模型下载与隐私设置
        OpenSearchSettings,
        /// ⌘⇧E：左侧栏切回文件视图，并在树里定位当前笔记
        OpenFiles,
        /// 左侧栏切到标签视图（`ui_design.md` 4.3）
        OpenTags,
        /// 链接悬停预览：悬停 500 ms（8.1）
        LinkPreviewHover,
        /// 链接悬停预览：按住 ⌘ 才显示
        LinkPreviewCommand,
        /// 链接悬停预览：关闭
        LinkPreviewOff,
        /// ⌘P：快速打开（3.2）
        QuickOpen,
        CommandPalette,
        /// ⌘T：在当前标签右边开一个空白的「新标签页」（`ui_design.md` 2.3）
        NewTab,
        /// ⌘W：关闭当前标签页
        CloseTab,
        /// 关闭当前之外的所有标签页
        CloseOtherTabs,
        /// 标签页右键：重命名当前笔记的文件
        RenameActiveTab,
        /// 标签页右键：改 front matter 里的 title，不改文件名
        RetitleActiveTab,
        /// ⌘⇧T：重新打开最近关闭的标签页
        ReopenClosedTab,
        /// ⌘⇧] / ⌃⇥：下一个标签页
        NextTab,
        /// ⌘⇧[ / ⌃⇧⇥：上一个标签页
        PrevTab,
        /// 收藏 / 取消收藏当前笔记
        ToggleFavorite,
        /// 右侧面板切到反链
        ShowBacklinks,
        /// 右侧面板切到大纲
        ShowOutline,
    ]
);

/// ⌘1–⌘8 跳到第 n 个标签页（从 0 数），⌘9 跳到最后一个（`usize::MAX`）
#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = aero, no_json)]
pub struct ActivateTab(pub usize);

/// 文件树右键菜单：收藏 / 取消收藏某一篇
#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = aero, no_json)]
pub struct ToggleFavoriteNote(pub String);

/// 文件树右键菜单：在新标签页打开
#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = aero, no_json)]
pub struct OpenInNewTab(pub String);

actions!(
    picker,
    [
        /// 搜索视图 / 快速打开里 ↑↓ 移动选中项
        SelectPrev,
        SelectNext,
        /// Esc：关闭快速打开 / 从搜索框回到编辑器
        Dismiss,
    ]
);

/// 文件树右键菜单：对某一篇笔记（vault 相对路径）的操作
#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = aero, no_json)]
pub struct TrashNote(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = aero, no_json)]
pub struct RevealInFinder(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = aero, no_json)]
pub struct CopyNotePath(pub String);

actions!(
    editor,
    [
        /// 光标在首行时移到上一块，否则交给文本控件
        BlockUp,
        /// 光标在末行时移到下一块
        BlockDown,
        /// ⏎：拆块 / 列表续行；代码块等多行单位里 propagate 给文本控件插入换行。
        /// 必须以动作截获而不是监听 PressEnter：gpui-base 在 submit 模式下会 propagate，
        /// 平台随后把 "\n" 当作输入塞进控件。
        SplitBlock,
        /// 文档级撤销（文本控件自己的撤销栈被绕过，见 9.5）
        DocUndo,
        DocRedo,
        /// Esc：块选中态 → 浏览态（编辑态里的 Esc 由文本控件的 `Escape` 动作冒泡到编辑器，先进块选中态）
        BlurEditor,
        /// 菜单确认或增加列表结构层级，其他输入场景交给文本控件
        MenuConfirm,
        /// ⇧Tab：列表正文降低结构层级，其他输入场景保持原行为
        ListOutdent,
        /// 笔记补全：⇧⏎ 插入并选中显示别名；其余情况仍是软换行。
        LinkAlias,
        /// ⌘K：选区变为行内链接，已有链接选中地址，无选区打开笔记补全。
        EditLink,
        OpenCharacterPalette,
        /// 块选中态（`ui_design.md` 6）：↑↓ 移动选中
        SelectBlockUp,
        SelectBlockDown,
        /// ⇧↑↓：扩展块选区；编辑态里光标在首 / 末行时把选区升级为块选中
        ExtendBlockUp,
        ExtendBlockDown,
        /// ⏎：编辑选中的块（光标在块末）
        EditSelectedBlock,
        /// PDF 块选中时左右翻页，⌘⏎ 在系统阅读器打开。
        PdfPreviousPage,
        PdfNextPage,
        OpenPdf,
        /// ⌫ / ⌦：删除选中的块（可撤销）
        DeleteSelectedBlocks,
        /// ⌘C / ⌘X：把选中的块作为 Markdown 源码复制 / 剪切
        CopySelectedBlocks,
        CutSelectedBlocks,
        /// 全屏看图里的 `Space` / `Esc`：关闭浮层（只在 `ImageViewer` 上下文里绑定，不影响正文输入）
        CloseImageViewer,
        /// `⌘V`：剪贴板里是位图 / 文件就导入到 `<note>.assets/`；是文字就交还给文本控件
        PasteAttachment,
        /// `⌘⇧H`：把上次用过的颜色应用到选中的文字或选中的块（7.7）
        ApplyLastColor,
        /// `⌘/`：打开颜色菜单（7.7）。块菜单（5.3）做出来之后它会变成菜单里的"颜色 ▸"
        OpenColorMenu,
        /// 颜色菜单里的 `Tab` / `Esc`
        ConfirmColorMenu,
        ConfirmSyncSource,
        CloseColorMenu,
        /// 当前笔记在「隐藏标记」和「整篇源码」之间换（9.1）
        ToggleNoteSource,
        /// 偏好：聚焦时显示源码，等于原来的 Live Preview（9.1）
        ToggleFocusSource,
    ]
);

/// 颜色菜单里的方向键：`(dx, dy)`，见 `editor::color_menu::step`。
#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = editor, no_json)]
pub struct MoveColorMenu(pub i8, pub i8);

/// `⌘⌥0–8` 转换块类型（`ui_design.md` 6 / 7.1）
#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = editor, no_json)]
pub struct ConvertBlock(pub u8);

#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = editor, no_json)]
pub struct FormatInline(pub aero_core::inline_format::Format);

#[derive(Clone, Copy)]
pub enum CommandScope {
    Global,
    Vault,
    Note,
    NoteTarget,
    Editor,
}

pub struct Command {
    pub id: &'static str,
    pub label: &'static str,
    pub keywords: &'static str,
    pub scope: CommandScope,
    pub action: Box<dyn Action>,
    pub shortcut: String,
}

impl Command {
    fn new(
        id: &'static str,
        label: &'static str,
        keywords: &'static str,
        scope: CommandScope,
        action: impl Action,
    ) -> Self {
        Self { id, label, keywords, scope, action: Box::new(action), shortcut: String::new() }
    }
}

pub fn commands() -> Vec<Command> {
    use CommandScope::*;
    let mut commands = vec![
        Command::new("settings", "设置…", "settings preferences sz", Global, OpenSettings),
        Command::new("about", "关于 Aero", "about version gy", Global, About),
        Command::new("check-updates", "检查更新", "update jcgx", Global, CheckForUpdates),
        Command::new("open-vault", "打开 Vault…", "open folder dkwjk", Global, OpenVault),
        Command::new("new-vault", "新建 Vault…", "new vault xj", Global, NewVault),
        Command::new("close-vault", "关闭 Vault", "close vault gb", Vault, CloseVault),
        Command::new("new-note", "新建笔记", "new note xjbj", Vault, NewNote),
        Command::new(
            "import-note-copy",
            "作为副本导入…",
            "import markdown copy dr fb",
            Vault,
            ImportNoteCopy,
        ),
        Command::new("open-daily", "今日日记", "daily journal jrrj", Vault, OpenDaily),
        Command::new("save", "保存笔记", "save note bcbj", Note, SaveNow),
        Command::new("delete-note", "移到废纸篓", "delete trash ydfzl", NoteTarget, DeleteNote),
        Command::new("quick-open", "快速打开笔记…", "quick open ksdk", Vault, QuickOpen),
        Command::new("search", "全文搜索", "search find qwss", Vault, OpenSearch),
        Command::new(
            "search-settings",
            "搜索与隐私…",
            "search privacy semantic model settings ss ys",
            Vault,
            OpenSearchSettings,
        ),
        Command::new(
            "files",
            "在文件树中定位当前笔记",
            "files explorer reveal locate wjs dw",
            Vault,
            OpenFiles,
        ),
        Command::new("tags", "显示标签", "tags bq", Vault, OpenTags),
        Command::new(
            "backlinks",
            "跳到文档链接与反链",
            "backlinks links references fl yy",
            Vault,
            ShowBacklinks,
        ),
        Command::new("outline", "显示大纲", "outline headings dg", Vault, ShowOutline),
        Command::new("new-tab", "新建标签页", "new tab xjbqy", Vault, NewTab),
        Command::new("close-tab", "关闭标签页", "close tab gbbqy", Vault, CloseTab),
        Command::new("close-other-tabs", "关闭其他标签页", "close other tabs gbqtbqy", Vault, CloseOtherTabs),
        Command::new("reopen-tab", "重新打开关闭的标签页", "reopen closed tab cxdk", Vault, ReopenClosedTab),
        Command::new("next-tab", "下一个标签页", "next tab xygbqy", Vault, NextTab),
        Command::new("prev-tab", "上一个标签页", "previous tab sygbqy", Vault, PrevTab),
        Command::new(
            "favorite",
            "收藏 / 取消收藏当前笔记",
            "favorite star bookmark pin sc",
            Note,
            ToggleFavorite,
        ),
        Command::new(
            "link-preview-hover",
            "链接预览：悬停",
            "hover preview link ylxf",
            Vault,
            LinkPreviewHover,
        ),
        Command::new(
            "link-preview-command",
            "链接预览：按住 ⌘",
            "command modifier preview link az",
            Vault,
            LinkPreviewCommand,
        ),
        Command::new("link-preview-off", "链接预览：关闭", "disable preview link gb", Vault, LinkPreviewOff),
        Command::new("sidebar", "折叠 / 展开左侧栏", "toggle sidebar zcl", Vault, ToggleSidebar),
        Command::new("panel", "折叠 / 展开右侧面板", "toggle panel ycmb", Vault, TogglePanel),
        Command::new("undo", "撤销", "undo cx", Editor, DocUndo),
        Command::new("redo", "重做", "redo cz", Editor, DocRedo),
        Command::new("edit-link", "插入或编辑链接", "link url insert edit lj", Editor, EditLink),
        #[cfg(target_os = "macos")]
        Command::new("emoji", "表情与符号…", "emoji character symbols bq", Editor, OpenCharacterPalette),
        Command::new("color-last", "应用上次使用的颜色", "color last ysyd", Editor, ApplyLastColor),
        Command::new("color-menu", "颜色…", "color palette ys tsb", Editor, OpenColorMenu),
        Command::new("note-source", "查看源码", "source markdown ckym", Note, ToggleNoteSource),
        Command::new(
            "focus-source",
            "聚焦时显示源码",
            "focus source live preview jjxsxym",
            Vault,
            ToggleFocusSource,
        ),
        Command::new("quit", "退出 Aero", "quit exit tc", Global, Quit),
    ];
    use aero_core::inline_format::Format;
    for (id, label, keywords, format) in [
        ("format-bold", "粗体", "bold strong ct", Format::Bold),
        ("format-italic", "斜体", "italic emphasis xt", Format::Italic),
        ("format-code", "行内代码", "inline code hndm", Format::Code),
        ("format-strike", "删除线", "strikethrough scx", Format::Strike),
        ("format-highlight", "黄色高亮", "highlight yellow hsgl", Format::Highlight),
    ] {
        commands.push(Command::new(id, label, keywords, Editor, FormatInline(format)));
    }
    for (n, (id, label, keywords)) in [
        ("text", "转换为正文", "text paragraph zw"),
        ("heading-1", "转换为标题 1", "heading 1 bt1"),
        ("heading-2", "转换为标题 2", "heading 2 bt2"),
        ("heading-3", "转换为标题 3", "heading 3 bt3"),
        ("bullet-list", "转换为无序列表", "bullet list wxlb"),
        ("ordered-list", "转换为有序列表", "ordered list yxlb"),
        ("task-list", "转换为任务列表", "task list rwlb"),
        ("quote", "转换为引用", "quote yy"),
        ("code", "转换为代码块", "code dmk"),
    ]
    .into_iter()
    .enumerate()
    {
        commands.push(Command::new(id, label, keywords, Editor, ConvertBlock(n as u8)));
    }
    commands
}

pub fn init(cx: &mut App) {
    use aero_core::inline_format::Format;
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("cmd-alt-h", HideOthers, None),
        KeyBinding::new("cmd-o", OpenVault, None),
        KeyBinding::new("cmd-n", NewNote, None),
        KeyBinding::new("cmd-shift-d", OpenDaily, None),
        KeyBinding::new("cmd-s", SaveNow, None),
        KeyBinding::new("cmd-backspace", DeleteNote, Some("FileTree")),
        KeyBinding::new("f2", crate::file_tree::RenameSelection, Some("FileTree")),
        KeyBinding::new("cmd-shift-f", OpenSearch, None),
        KeyBinding::new("cmd-shift-e", OpenFiles, None),
        KeyBinding::new("cmd-p", QuickOpen, None),
        KeyBinding::new("cmd-shift-p", CommandPalette, None),
        // 搜索框 / 快速打开里的 ↑↓ / Esc：谓词写成 "Picker > Input"，匹配深度与 Input 自己的绑定相同、
        // 注册更晚，所以先于它们（以及下面编辑器的 up / down）匹配
        KeyBinding::new("up", SelectPrev, Some("Picker > Input")),
        KeyBinding::new("down", SelectNext, Some("Picker > Input")),
        KeyBinding::new("escape", Dismiss, Some("Picker > Input")),
        KeyBinding::new("cmd-\\", ToggleSidebar, None),
        KeyBinding::new("cmd-shift-\\", TogglePanel, None),
        KeyBinding::new("cmd-|", TogglePanel, None),
        // 标签页（`ui_design.md` 14）。⇧ 与 [ ] 组合时 macOS 可能报成 { }，两种写法都绑
        KeyBinding::new("cmd-t", NewTab, None),
        KeyBinding::new("cmd-w", CloseTab, None),
        KeyBinding::new("cmd-shift-t", ReopenClosedTab, None),
        KeyBinding::new("cmd-shift-]", NextTab, None),
        KeyBinding::new("cmd-}", NextTab, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("cmd-shift-[", PrevTab, None),
        KeyBinding::new("cmd-{", PrevTab, None),
        KeyBinding::new("ctrl-shift-tab", PrevTab, None),
        // 编辑器：覆盖 gpui-base 同键绑定，处理不了时 propagate
        KeyBinding::new("up", BlockUp, Some("Input")),
        KeyBinding::new("down", BlockDown, Some("Input")),
        KeyBinding::new("enter", SplitBlock, Some("Input")),
        KeyBinding::new("tab", MenuConfirm, Some("Input")),
        KeyBinding::new("shift-tab", ListOutdent, Some("Editor > Input")),
        KeyBinding::new("shift-enter", LinkAlias, Some("Input")),
        KeyBinding::new("cmd-k", EditLink, Some("Editor > Input")),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-cmd-space", OpenCharacterPalette, Some("Editor > Input")),
        KeyBinding::new("cmd-b", FormatInline(Format::Bold), Some("Editor > Input")),
        KeyBinding::new("cmd-i", FormatInline(Format::Italic), Some("Editor > Input")),
        KeyBinding::new("cmd-e", FormatInline(Format::Code), Some("Editor > Input")),
        KeyBinding::new("cmd-shift-s", FormatInline(Format::Strike), Some("Editor > Input")),
        // ⌘⇧H 应用上次用过的颜色（7.7）；没用过菜单时那就是黄色背景，与 `==` 同一条路
        KeyBinding::new("cmd-shift-h", ApplyLastColor, Some("Editor > Input")),
        // ⌘/ 是块菜单的键位（14 节）；块菜单还没做，先直接打开它唯一做好的那一项——颜色
        KeyBinding::new("cmd-/", OpenColorMenu, Some("Editor > Input")),
        // 颜色菜单（7.7）：焦点在菜单的过滤输入框上，这些绑定比控件自己的同键绑定更晚注册，先于它们匹配
        KeyBinding::new("up", MoveColorMenu(0, -1), Some("ColorMenu > Input")),
        KeyBinding::new("down", MoveColorMenu(0, 1), Some("ColorMenu > Input")),
        KeyBinding::new("left", MoveColorMenu(-1, 0), Some("ColorMenu > Input")),
        KeyBinding::new("right", MoveColorMenu(1, 0), Some("ColorMenu > Input")),
        // ⏎ 也要显式绑定：编辑器在 `Input` 上下文里把 enter 接成了拆块，那条绑定同样匹配菜单里的输入框
        KeyBinding::new("enter", ConfirmColorMenu, Some("ColorMenu > Input")),
        KeyBinding::new("tab", ConfirmColorMenu, Some("ColorMenu > Input")),
        KeyBinding::new("escape", CloseColorMenu, Some("ColorMenu > Input")),
        // ⌘/ 是开关：菜单开着时焦点在它自己的输入框上，外面那条绑定匹配不到这里
        KeyBinding::new("cmd-/", CloseColorMenu, Some("ColorMenu > Input")),
        KeyBinding::new("enter", ConfirmSyncSource, Some("SyncPicker > Input")),
        KeyBinding::new("cmd-z", DocUndo, Some("Input")),
        KeyBinding::new("cmd-shift-z", DocRedo, Some("Input")),
        KeyBinding::new("cmd-z", gpui_kit::base::input::Undo, Some("PdfPageInput > Input")),
        KeyBinding::new("cmd-shift-z", gpui_kit::base::input::Redo, Some("PdfPageInput > Input")),
        // 编辑态里 ⇧↑↓ 越过块边界时升级为块选中；否则 propagate 给控件扩展字符选区
        KeyBinding::new("shift-up", ExtendBlockUp, Some("Input")),
        KeyBinding::new("shift-down", ExtendBlockDown, Some("Input")),
        // 浏览态 / 块选中态（编辑器容器持有焦点）。这些绑定在 Input 聚焦时也匹配，但深度更浅，
        // 只在控件那一层的绑定都 propagate 之后才轮到；处理器在没有块选区时 propagate。
        KeyBinding::new("cmd-z", DocUndo, Some("Editor")),
        KeyBinding::new("cmd-shift-z", DocRedo, Some("Editor")),
        KeyBinding::new("escape", BlurEditor, Some("Editor")),
        KeyBinding::new("escape", CloseImageViewer, Some("ImageViewer")),
        KeyBinding::new("space", CloseImageViewer, Some("ImageViewer")),
        // 比文本控件自己的 cmd-v 更具体：先看剪贴板里有没有图片 / 文件，没有就 propagate 回去按文字粘贴
        KeyBinding::new("cmd-v", PasteAttachment, Some("Editor > Input")),
        KeyBinding::new("cmd-v", PasteAttachment, Some("Editor")),
        KeyBinding::new("up", SelectBlockUp, Some("Editor")),
        KeyBinding::new("down", SelectBlockDown, Some("Editor")),
        KeyBinding::new("shift-up", ExtendBlockUp, Some("Editor")),
        KeyBinding::new("shift-down", ExtendBlockDown, Some("Editor")),
        KeyBinding::new("enter", EditSelectedBlock, Some("Editor")),
        KeyBinding::new("left", PdfPreviousPage, Some("Editor")),
        KeyBinding::new("right", PdfNextPage, Some("Editor")),
        KeyBinding::new("cmd-enter", OpenPdf, Some("Editor")),
        KeyBinding::new("backspace", DeleteSelectedBlocks, Some("Editor")),
        KeyBinding::new("delete", DeleteSelectedBlocks, Some("Editor")),
        KeyBinding::new("cmd-c", CopySelectedBlocks, Some("Editor")),
        KeyBinding::new("cmd-x", CutSelectedBlocks, Some("Editor")),
        KeyBinding::new("cmd-shift-h", ApplyLastColor, Some("Editor")),
        KeyBinding::new("cmd-/", OpenColorMenu, Some("Editor")),
    ]);
    let input_context =
        Rc::new(KeyBindingContextPredicate::parse("Input").expect("Input is a valid key binding context"));
    let convert_block_bindings = (0..=8u8)
        .map(|n| {
            KeyBinding::load(
                &format!("cmd-alt-{n}"),
                Box::new(ConvertBlock(n)),
                Some(input_context.clone()),
                false,
                None,
                cx,
            )
            .expect("generated block conversion key binding is valid")
        })
        .collect::<Vec<_>>();
    cx.bind_keys(convert_block_bindings);
    let activate_tab_bindings = (1..=9usize)
        .map(|n| {
            KeyBinding::load(
                &format!("cmd-{n}"),
                Box::new(ActivateTab(if n == 9 { usize::MAX } else { n - 1 })),
                None,
                false,
                None,
                cx,
            )
            .expect("generated tab activation key binding is valid")
        })
        .collect::<Vec<_>>();
    cx.bind_keys(activate_tab_bindings);

    // 窗口里没人处理时的兜底。隐藏类动作走系统，不经过编辑器。
    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
    cx.on_action(|_: &Hide, cx: &mut App| cx.hide());
    cx.on_action(|_: &HideOthers, cx: &mut App| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx: &mut App| cx.unhide_other_apps());

    sync_menus(
        MenuState {
            preview: crate::paths::LinkPreviewMode::Hover,
            focus_shows_source: false,
            note_source: false,
        },
        cx,
    );
}

/// 应用菜单，顺序与 macOS 的应用菜单一致。
fn application_menu() -> Menu {
    Menu::new("Aero").items(vec![
        MenuItem::action("关于 Aero", About),
        MenuItem::action("检查更新", CheckForUpdates),
        MenuItem::action("设置…", OpenSettings),
        MenuItem::separator(),
        MenuItem::os_submenu("服务", SystemMenuType::Services),
        MenuItem::separator(),
        MenuItem::action("隐藏 Aero", Hide),
        MenuItem::action("隐藏其他", HideOthers),
        MenuItem::action("全部显示", ShowAll),
        MenuItem::separator(),
        MenuItem::action("退出 Aero", Quit),
    ])
}

pub struct MenuState {
    pub preview: crate::paths::LinkPreviewMode,
    pub focus_shows_source: bool,
    pub note_source: bool,
}

/// 按当前预览设置重画菜单（勾选「悬停 / 按住 ⌘ / 关闭」里的一项）。
pub fn sync_menus(state: MenuState, cx: &mut App) {
    let MenuState { preview, focus_shows_source, note_source } = state;
    use crate::paths::LinkPreviewMode;
    use aero_core::inline_format::Format;
    cx.set_menus(vec![
        application_menu(),
        Menu::new("文件").items(vec![
            MenuItem::action("打开 Vault…", OpenVault),
            MenuItem::action("新建 Vault…", NewVault),
            MenuItem::action("关闭 Vault", CloseVault),
            MenuItem::separator(),
            MenuItem::action("新建笔记", NewNote),
            MenuItem::action("作为副本导入…", ImportNoteCopy),
            MenuItem::action("今日日记", OpenDaily),
            MenuItem::action("保存", SaveNow),
            MenuItem::separator(),
            MenuItem::action("新建标签页", NewTab),
            MenuItem::action("关闭标签页", CloseTab),
            MenuItem::action("重新打开关闭的标签页", ReopenClosedTab),
            MenuItem::separator(),
            MenuItem::action("收藏 / 取消收藏", ToggleFavorite),
            MenuItem::action("移到废纸篓", DeleteNote),
        ]),
        Menu::new("编辑").items(vec![
            MenuItem::action("撤销", DocUndo),
            MenuItem::action("重做", DocRedo),
            MenuItem::separator(),
            MenuItem::action("剪切", gpui_kit::base::input::Cut),
            MenuItem::action("复制", gpui_kit::base::input::Copy),
            MenuItem::action("粘贴", gpui_kit::base::input::Paste),
            MenuItem::action("全选", gpui_kit::base::input::SelectAll),
            MenuItem::separator(),
            MenuItem::action("插入或编辑链接", EditLink),
            MenuItem::separator(),
            MenuItem::action("粗体", FormatInline(Format::Bold)),
            MenuItem::action("斜体", FormatInline(Format::Italic)),
            MenuItem::action("行内代码", FormatInline(Format::Code)),
            MenuItem::action("删除线", FormatInline(Format::Strike)),
            MenuItem::action("黄色高亮", FormatInline(Format::Highlight)),
            MenuItem::action("颜色…", OpenColorMenu),
            MenuItem::action("上次使用的颜色", ApplyLastColor),
            #[cfg(target_os = "macos")]
            MenuItem::separator(),
            #[cfg(target_os = "macos")]
            MenuItem::action("表情与符号…", OpenCharacterPalette),
        ]),
        Menu::new("视图").items(vec![
            MenuItem::action("文件", OpenFiles),
            MenuItem::action("搜索", OpenSearch),
            MenuItem::action("搜索与隐私…", OpenSearchSettings),
            MenuItem::action("标签", OpenTags),
            MenuItem::action("反链", ShowBacklinks),
            MenuItem::action("大纲", ShowOutline),
            MenuItem::separator(),
            MenuItem::action("下一个标签页", NextTab),
            MenuItem::action("上一个标签页", PrevTab),
            MenuItem::separator(),
            MenuItem::action("快速打开…", QuickOpen),
            MenuItem::action("命令面板…", CommandPalette),
            MenuItem::separator(),
            MenuItem::submenu(Menu::new("链接预览").items(vec![
                MenuItem::action("悬停", LinkPreviewHover).checked(preview == LinkPreviewMode::Hover),
                MenuItem::action("按住 ⌘", LinkPreviewCommand).checked(preview == LinkPreviewMode::Command),
                MenuItem::action("关闭", LinkPreviewOff).checked(preview == LinkPreviewMode::Off),
            ])),
            MenuItem::action("查看源码", ToggleNoteSource).checked(note_source),
            MenuItem::action("聚焦时显示源码", ToggleFocusSource).checked(focus_shows_source),
            MenuItem::separator(),
            MenuItem::action("折叠 / 展开左侧栏", ToggleSidebar),
            MenuItem::action("折叠 / 展开右侧面板", TogglePanel),
        ]),
    ]);
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    #[test]
    fn emoji_command_is_scoped_to_the_editor() {
        let command = super::commands()
            .into_iter()
            .find(|command| command.id == "emoji")
            .expect("system emoji command must be discoverable");
        assert_eq!(command.label, "表情与符号…");
        assert!(matches!(command.scope, super::CommandScope::Editor));
        assert!(command.keywords.contains("emoji"));
    }

    #[test]
    fn search_privacy_command_is_available_in_vault() {
        let command = super::commands()
            .into_iter()
            .find(|c| c.id == "search-settings")
            .expect("search privacy settings must be discoverable");
        assert_eq!(command.label, "搜索与隐私…");
        assert!(matches!(command.scope, super::CommandScope::Vault));
    }

    #[test]
    fn application_menu_follows_the_platform_order() {
        let labels: Vec<_> = super::application_menu()
            .items
            .iter()
            .filter_map(|item| match item {
                super::MenuItem::Action { name, .. } => Some(name.to_string()),
                super::MenuItem::SystemMenu(menu) => Some(menu.name.to_string()),
                super::MenuItem::Separator | super::MenuItem::Submenu(_) => None,
            })
            .collect();
        assert_eq!(
            labels,
            ["关于 Aero", "检查更新", "设置…", "服务", "隐藏 Aero", "隐藏其他", "全部显示", "退出 Aero",]
        );
    }
}
