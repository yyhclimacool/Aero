//! 动作与快捷键（`ui_design.md` 第 14 节，M1a 子集）。
//!
//! 编辑器相关的绑定放在 `Input` 上下文里、并在 `gpui_kit::init` **之后**注册：
//! GPUI 对同一上下文深度的绑定按注册顺序取后者，这样 `up` / `down` / `cmd-z` 先到我们手里，
//! 处理不了（光标不在首行 / 末行）时 `cx.propagate()` 再交还给 gpui-base 的文本控件。

use gpui_kit::*;

actions!(
    aero,
    [
        Quit,
        /// 打开文件夹作为 vault
        OpenVault,
        /// 新建 vault（选位置，创建 daily/ 与欢迎笔记）
        NewVault,
        /// 回到首次运行页
        CloseVault,
        NewNote,
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
        /// ⌘⇧E：左侧栏切回文件视图
        OpenFiles,
        /// ⌘P：快速打开（3.2）
        QuickOpen,
        CommandPalette,
    ]
);

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
        /// `/` 菜单开着时 Tab 确认；否则交给文本控件
        MenuConfirm,
        /// 笔记补全：⇧⏎ 插入并选中显示别名；其余情况仍是软换行。
        LinkAlias,
        /// 块选中态（`ui_design.md` 6）：↑↓ 移动选中
        SelectBlockUp,
        SelectBlockDown,
        /// ⇧↑↓：扩展块选区；编辑态里光标在首 / 末行时把选区升级为块选中
        ExtendBlockUp,
        ExtendBlockDown,
        /// ⏎：编辑选中的块（光标在块末）
        EditSelectedBlock,
        /// ⌫ / ⌦：删除选中的块（可撤销）
        DeleteSelectedBlocks,
        /// ⌘C / ⌘X：把选中的块作为 Markdown 源码复制 / 剪切
        CopySelectedBlocks,
        CutSelectedBlocks,
    ]
);

/// `⌘⌥0–8` 转换块类型（`ui_design.md` 6 / 7.1）
#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = editor, no_json)]
pub struct ConvertBlock(pub u8);

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
        Command::new("open-vault", "打开 Vault…", "open folder dkwjk", Global, OpenVault),
        Command::new("new-vault", "新建 Vault…", "new vault xj", Global, NewVault),
        Command::new("close-vault", "关闭 Vault", "close vault gb", Vault, CloseVault),
        Command::new("new-note", "新建笔记", "new note xjbj", Vault, NewNote),
        Command::new("open-daily", "今日日记", "daily journal jrrj", Vault, OpenDaily),
        Command::new("save", "保存笔记", "save note bcbj", Note, SaveNow),
        Command::new("delete-note", "移到废纸篓", "delete trash ydfzl", NoteTarget, DeleteNote),
        Command::new("quick-open", "快速打开笔记…", "quick open ksdk", Vault, QuickOpen),
        Command::new("search", "全文搜索", "search find qwss", Vault, OpenSearch),
        Command::new("files", "显示文件树", "files explorer xswjs", Vault, OpenFiles),
        Command::new("sidebar", "折叠 / 展开左侧栏", "toggle sidebar zcl", Vault, ToggleSidebar),
        Command::new("panel", "折叠 / 展开右侧面板", "toggle panel ycmb", Vault, TogglePanel),
        Command::new("undo", "撤销", "undo cx", Editor, DocUndo),
        Command::new("redo", "重做", "redo cz", Editor, DocRedo),
        Command::new("quit", "退出 Aero", "quit exit tc", Global, Quit),
    ];
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
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-o", OpenVault, None),
        KeyBinding::new("cmd-n", NewNote, None),
        KeyBinding::new("cmd-shift-d", OpenDaily, None),
        KeyBinding::new("cmd-s", SaveNow, None),
        KeyBinding::new("cmd-backspace", DeleteNote, Some("FileTree")),
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
        // 编辑器：覆盖 gpui-base 同键绑定，处理不了时 propagate
        KeyBinding::new("up", BlockUp, Some("Input")),
        KeyBinding::new("down", BlockDown, Some("Input")),
        KeyBinding::new("enter", SplitBlock, Some("Input")),
        KeyBinding::new("tab", MenuConfirm, Some("Input")),
        KeyBinding::new("shift-enter", LinkAlias, Some("Input")),
        KeyBinding::new("cmd-z", DocUndo, Some("Input")),
        KeyBinding::new("cmd-shift-z", DocRedo, Some("Input")),
        // 编辑态里 ⇧↑↓ 越过块边界时升级为块选中；否则 propagate 给控件扩展字符选区
        KeyBinding::new("shift-up", ExtendBlockUp, Some("Input")),
        KeyBinding::new("shift-down", ExtendBlockDown, Some("Input")),
        // 浏览态 / 块选中态（编辑器容器持有焦点）。这些绑定在 Input 聚焦时也匹配，但深度更浅，
        // 只在控件那一层的绑定都 propagate 之后才轮到；处理器在没有块选区时 propagate。
        KeyBinding::new("cmd-z", DocUndo, Some("Editor")),
        KeyBinding::new("cmd-shift-z", DocRedo, Some("Editor")),
        KeyBinding::new("escape", BlurEditor, Some("Editor")),
        KeyBinding::new("up", SelectBlockUp, Some("Editor")),
        KeyBinding::new("down", SelectBlockDown, Some("Editor")),
        KeyBinding::new("shift-up", ExtendBlockUp, Some("Editor")),
        KeyBinding::new("shift-down", ExtendBlockDown, Some("Editor")),
        KeyBinding::new("enter", EditSelectedBlock, Some("Editor")),
        KeyBinding::new("backspace", DeleteSelectedBlocks, Some("Editor")),
        KeyBinding::new("delete", DeleteSelectedBlocks, Some("Editor")),
        KeyBinding::new("cmd-c", CopySelectedBlocks, Some("Editor")),
        KeyBinding::new("cmd-x", CutSelectedBlocks, Some("Editor")),
    ]);
    cx.bind_keys((0..=8u8).map(|n| KeyBinding::new(&format!("cmd-alt-{n}"), ConvertBlock(n), Some("Input"))));

    // 窗口里没人处理时的兜底
    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());

    cx.set_menus(vec![
        Menu::new("Aero").items(vec![MenuItem::action("退出 Aero", Quit)]),
        Menu::new("文件").items(vec![
            MenuItem::action("打开 Vault…", OpenVault),
            MenuItem::action("新建 Vault…", NewVault),
            MenuItem::action("关闭 Vault", CloseVault),
            MenuItem::separator(),
            MenuItem::action("新建笔记", NewNote),
            MenuItem::action("今日日记", OpenDaily),
            MenuItem::action("保存", SaveNow),
            MenuItem::separator(),
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
        ]),
        Menu::new("视图").items(vec![
            MenuItem::action("文件", OpenFiles),
            MenuItem::action("搜索", OpenSearch),
            MenuItem::action("快速打开…", QuickOpen),
            MenuItem::action("命令面板…", CommandPalette),
            MenuItem::separator(),
            MenuItem::action("折叠 / 展开左侧栏", ToggleSidebar),
            MenuItem::action("折叠 / 展开右侧面板", TogglePanel),
        ]),
    ]);
}
