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
        /// 编辑态 → 浏览态
        BlurEditor,
        /// `/` 菜单开着时 Tab 确认；否则交给文本控件
        MenuConfirm,
    ]
);

/// `⌘⌥0–8` 转换块类型（`ui_design.md` 6 / 7.1）
#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = editor, no_json)]
pub struct ConvertBlock(pub u8);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-o", OpenVault, None),
        KeyBinding::new("cmd-n", NewNote, None),
        KeyBinding::new("cmd-shift-d", OpenDaily, None),
        KeyBinding::new("cmd-s", SaveNow, None),
        KeyBinding::new("cmd-backspace", DeleteNote, Some("FileTree")),
        KeyBinding::new("cmd-\\", ToggleSidebar, None),
        KeyBinding::new("cmd-shift-\\", TogglePanel, None),
        KeyBinding::new("cmd-|", TogglePanel, None),
        // 编辑器：覆盖 gpui-base 同键绑定，处理不了时 propagate
        KeyBinding::new("up", BlockUp, Some("Input")),
        KeyBinding::new("down", BlockDown, Some("Input")),
        KeyBinding::new("enter", SplitBlock, Some("Input")),
        KeyBinding::new("tab", MenuConfirm, Some("Input")),
        KeyBinding::new("cmd-z", DocUndo, Some("Input")),
        KeyBinding::new("cmd-shift-z", DocRedo, Some("Input")),
        // 浏览态（编辑器容器持有焦点）下的撤销 / 重做
        KeyBinding::new("cmd-z", DocUndo, Some("Editor")),
        KeyBinding::new("cmd-shift-z", DocRedo, Some("Editor")),
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
            MenuItem::action("折叠 / 展开左侧栏", ToggleSidebar),
            MenuItem::action("折叠 / 展开右侧面板", TogglePanel),
        ]),
    ]);
}
