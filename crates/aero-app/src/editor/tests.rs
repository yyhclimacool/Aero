//! 无头 GPUI 测试：真正渲染编辑器、模拟按键，验证"控件 → Document → 磁盘"整条链路。
//!
//! 这里不 `use gpui_kit::*`：它会把 GPUI 的 `test` 属性宏带进作用域，遮住标准 `#[test]`。

use std::sync::Arc;
use std::time::Duration;

use aero_core::{Vault, VaultPath};
use gpui_kit::{Entity, TestAppContext, VisualTestContext};

use super::Editor;

fn text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    cx.update(|_, cx| editor.read(cx).doc.to_string())
}

fn focus_unit(editor: &Entity<Editor>, ix: usize, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            let id = e.units[ix].id;
            e.focus_block(id, window, cx);
        })
    });
    cx.run_until_parked();
}

fn open(cx: &mut TestAppContext, body: &str) -> (tempfile::TempDir, Arc<Vault>, VaultPath, Entity<Editor>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::actions::init(cx);
    });
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.md"), body).unwrap();
    let vault = Arc::new(Vault::open(dir.path()).unwrap());
    let path = VaultPath::new("a.md").unwrap();
    let (doc, stamp) = Editor::load(vault.clone(), path.clone()).unwrap();
    let (v, p) = (vault.clone(), path.clone());
    let (editor, _) = cx.add_window_view(|_, cx| Editor::new(v, p, doc, stamp, cx));
    (dir, vault, path, editor)
}

#[gpui_kit::test]
fn typing_enter_backspace_undo_and_save(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "hello\n\nworld\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let file = dir.path().join("a.md");

    // 点击第二段：光标在块尾，输入直达 Document
    focus_unit(&editor, 1, cx);
    cx.simulate_input("!");
    assert_eq!(text(&editor, cx), "hello\n\nworld!\n");

    // ⏎ 段末：新建空段落；接着输入
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "hello\n\nworld!\n\n\n");
    cx.simulate_input("new");
    assert_eq!(text(&editor, cx), "hello\n\nworld!\n\nnew\n");

    // 删空新段落，再在块首 ⌫：回到上一段末尾
    cx.simulate_keystrokes("backspace backspace backspace");
    assert_eq!(text(&editor, cx), "hello\n\nworld!\n\n\n");
    cx.simulate_keystrokes("backspace");
    assert_eq!(text(&editor, cx), "hello\n\nworld!\n");

    // 文档级撤销：一步步回到原文
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "hello\n\nworld!\n\n\n");
    cx.simulate_keystrokes("cmd-z cmd-z");
    assert_eq!(text(&editor, cx), "hello\n\nworld!\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "hello\n\nworld\n");
    cx.simulate_keystrokes("cmd-shift-z");
    assert_eq!(text(&editor, cx), "hello\n\nworld!\n");

    // 500 ms 静默后自动保存
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello\n\nworld\n");
    cx.executor().advance_clock(Duration::from_millis(600));
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello\n\nworld!\n");

    // 失焦（Esc）立即落盘
    cx.simulate_input("?");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello\n\nworld!?\n");
    cx.update(|_, cx| assert!(editor.read(cx).focused.is_none()));
}

#[gpui_kit::test]
fn enter_in_middle_splits_and_arrows_cross_blocks(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "ab\n\ncd\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);

    // 光标放到 "ab" 的 a|b，⏎ 拆成 a / b
    focus_unit(&editor, 0, cx);
    cx.simulate_keystrokes("left");
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "a\n\nb\n\ncd\n");
    // 焦点落在 "b" 上：继续输入
    cx.simulate_input("x");
    assert_eq!(text(&editor, cx), "a\n\nxb\n\ncd\n");

    // ↓ 到下一块（列 1），输入落在 c|d
    cx.simulate_keystrokes("down");
    cx.simulate_input("-");
    assert_eq!(text(&editor, cx), "a\n\nxb\n\nc-d\n");
    // ← 回到列 1，↑ 回上一块保持列 1
    cx.simulate_keystrokes("left up");
    cx.simulate_input("+");
    assert_eq!(text(&editor, cx), "a\n\nx+b\n\nc-d\n");
}

#[gpui_kit::test]
fn shift_enter_soft_breaks_and_code_fence_enter_inserts_newline(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "ab\n\n```\nx\n```\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);

    // ⇧⏎：段落内软换行，不拆块
    focus_unit(&editor, 0, cx);
    cx.simulate_keystrokes("shift-enter");
    cx.simulate_input("z");
    assert_eq!(text(&editor, cx), "ab\nz\n\n```\nx\n```\n");
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert_eq!(e.units.len(), 2, "软换行后仍是两个单位");
    });

    // 代码块：⏎ 只是换行。控件内的 ↑↓ 依赖像素布局，无头环境下不可靠，这里用 ← 回到 `x` 之后
    focus_unit(&editor, 1, cx);
    cx.simulate_keystrokes("left left left left");
    cx.simulate_keystrokes("enter");
    cx.simulate_input("y");
    assert_eq!(text(&editor, cx), "ab\nz\n\n```\nx\ny\n```\n");
    // 末行 ↓ 离开代码块：文末补出空段落并进入（⌘↓ 先到控件末尾）
    cx.simulate_keystrokes("cmd-down down");
    cx.simulate_input("after");
    assert_eq!(text(&editor, cx), "ab\nz\n\n```\nx\ny\n```\n\nafter");
}

#[gpui_kit::test]
fn list_items_continue_and_exit(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "- one\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);

    focus_unit(&editor, 0, cx);
    cx.simulate_keystrokes("enter");
    cx.simulate_input("two");
    assert_eq!(text(&editor, cx), "- one\n- two\n");
    // 空项 ⏎ 退出列表，然后输入成为段落
    cx.simulate_keystrokes("enter enter");
    cx.simulate_input("para");
    assert_eq!(text(&editor, cx), "- one\n- two\n\npara\n");
    // 段首 ⌫：并入上一块？上一块是列表项，不能合并，只移动光标；再输入落在 two 之后
    cx.simulate_keystrokes("home backspace");
    cx.simulate_input("!");
    assert_eq!(text(&editor, cx), "- one\n- two!\n\npara\n");
}

/// 点击浏览态的块：先进入编辑态（光标在块尾），两帧后把点击重放给控件，光标落到点击处。
#[gpui_kit::test]
fn click_positions_cursor_in_block(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "hello\n\nworld\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let draw = |cx: &mut VisualTestContext| cx.update(|window, cx| window.draw(cx).clear(cx));
    let next_frame = |cx: &mut VisualTestContext| cx.update(|window, cx| window.simulate_next_frame(cx));
    let cursor = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| editor.read(cx).focused.as_ref().map(|f| f.textarea.read(cx).cursor()))
    };

    draw(cx);
    let bounds = cx.debug_bounds("unit-1").expect("first block is laid out");
    // 点在第一块最左边、垂直居中
    let at = gpui_kit::point(bounds.origin.x + gpui_kit::px(2.), bounds.origin.y + bounds.size.height / 2.);
    cx.simulate_click(at, gpui_kit::Modifiers::default());
    // 进入编辑态，光标暂时在块尾
    assert_eq!(cursor(cx), Some(5));

    // 第一帧：回调只是再排一帧；绘制让控件有了布局；第二帧：重放点击
    assert_eq!(next_frame(cx), 1);
    draw(cx);
    assert_eq!(next_frame(cx), 1);
    cx.run_until_parked();
    assert_eq!(cursor(cx), Some(0), "cursor follows the click to the start of the block");
    cx.simulate_input("X");
    assert_eq!(text(&editor, cx), "Xhello\n\nworld\n");

    // 回到浏览态，再点第二块的最右边：光标在该块末尾
    cx.simulate_keystrokes("escape");
    draw(cx);
    let bounds = cx.debug_bounds("unit-2").expect("second block is laid out");
    let at = gpui_kit::point(bounds.right() - gpui_kit::px(2.), bounds.origin.y + bounds.size.height / 2.);
    cx.simulate_click(at, gpui_kit::Modifiers::default());
    next_frame(cx);
    draw(cx);
    next_frame(cx);
    cx.run_until_parked();
    cx.simulate_input("!");
    assert_eq!(text(&editor, cx), "Xhello\n\nworld!\n");
}

/// 外部改了文件、这里没有未保存修改：下一次轮询静默重载（方案 12）。
#[gpui_kit::test]
fn external_change_reloads_when_clean(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "old\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let file = dir.path().join("a.md");

    // 让 mtime 一定不同（文件系统精度可能只有 1 s）
    let past = std::time::SystemTime::now() - Duration::from_secs(5);
    std::fs::write(&file, "new from disk\n").unwrap();
    std::fs::File::open(&file).unwrap().set_modified(past).unwrap();

    cx.executor().advance_clock(Duration::from_millis(2100));
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "new from disk\n");
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert_eq!(e.save_state(), &super::SaveState::Clean);
        assert_eq!(e.units.len(), 1);
    });
}

/// 外部改了文件、这里有未保存修改：磁盘版本另存为冲突副本，本地版本照常落盘。
#[gpui_kit::test]
fn external_change_while_dirty_writes_conflict_copy(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "base\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let file = dir.path().join("a.md");

    focus_unit(&editor, 0, cx);
    cx.simulate_input("!");
    assert_eq!(text(&editor, cx), "base!\n");

    let past = std::time::SystemTime::now() - Duration::from_secs(5);
    std::fs::write(&file, "theirs\n").unwrap();
    std::fs::File::open(&file).unwrap().set_modified(past).unwrap();

    // 500 ms 后自动保存：写前校验发现外部修改
    cx.executor().advance_clock(Duration::from_millis(600));
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "base!\n");
    let copies: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("a.conflict-") && n.ends_with(".md"))
        .collect();
    assert_eq!(copies.len(), 1, "exactly one conflict copy, got {copies:?}");
    assert_eq!(std::fs::read_to_string(dir.path().join(&copies[0])).unwrap(), "theirs\n");

    // 之后再保存不会重复产生副本
    cx.simulate_input("?");
    cx.executor().advance_clock(Duration::from_millis(600));
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "base!?\n");
    let n = std::fs::read_dir(dir.path()).unwrap().filter(|e| {
        e.as_ref().is_ok_and(|e| e.file_name().to_string_lossy().starts_with("a.conflict-"))
    });
    assert_eq!(n.count(), 1);
}

#[gpui_kit::test]
fn empty_note_starts_writing(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);

    cx.update(|window, cx| editor.update(cx, |e, cx| e.start_writing(window, cx)));
    cx.run_until_parked();
    cx.simulate_input("# Title");
    cx.simulate_keystrokes("enter");
    cx.simulate_input("body");
    assert_eq!(text(&editor, cx), "# Title\n\nbody");
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert_eq!(e.headings().len(), 1);
        assert_eq!(e.headings()[0].1, "Title");
        assert_eq!(e.word_count(), 2);
    });
}
