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
fn callout_preview_folds_without_editing_source(cx: &mut TestAppContext) {
    let source =
        "> [!warning]- Caution\n> **Body**\n>\n> - first\n> - second\n\n> [!mystery] Unknown\n> kept\n";
    let (dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("callout-title-Caution").is_some());
    assert!(cx.debug_bounds("callout-body-Caution").is_none());
    assert!(cx.debug_bounds("callout-body-Unknown").is_some());
    assert!(cx.debug_bounds("callout-toggle-Unknown").is_none());
    let toggle = cx.debug_bounds("callout-toggle-Caution").unwrap();
    cx.simulate_click(toggle.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("callout-body-Caution").is_some());
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert!(e.focused.is_none(), "folding must not enter the source editor");
        assert!(!e.dirty);
        assert_eq!(e.units.len(), 2);
    });
    assert_eq!(text(&editor, cx), source);
    assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), source);
    let toggle = cx.debug_bounds("callout-toggle-Caution").unwrap();
    cx.simulate_click(toggle.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("callout-body-Caution").is_none());
}

#[gpui_kit::test]
fn callout_preview_renders_all_default_titles(cx: &mut TestAppContext) {
    let source = ["note", "TiP", "warning", "danger", "info", "question", "example", "quote"]
        .into_iter()
        .map(|kind| format!("> [!{kind}]\n> body\n\n"))
        .collect::<String>();
    let (_dir, _vault, _path, editor) = open(cx, &source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    for selector in [
        "callout-title-Note",
        "callout-title-Tip",
        "callout-title-Warning",
        "callout-title-Danger",
        "callout-title-Info",
        "callout-title-Question",
        "callout-title-Example",
        "callout-title-Quote",
    ] {
        assert!(cx.debug_bounds(selector).is_some(), "{selector}");
    }
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn callout_fold_button_is_keyboard_accessible(cx: &mut TestAppContext) {
    let source = "> [!info]- Keyboard\n> body\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let (_, cx) = cx.add_window_view(|window, cx| gpui_kit::component::Root::new(editor.clone(), window, cx));
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.focus(&editor.read(cx).focus_handle.clone(), cx);
        window.draw(cx).clear(cx);
    });
    cx.simulate_keystrokes("tab");
    for (key, open) in [("space", true), ("enter", false)] {
        let keystroke = gpui_kit::Keystroke::parse(key).unwrap();
        cx.simulate_event(gpui_kit::KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        cx.simulate_event(gpui_kit::KeyUpEvent { keystroke });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(cx.debug_bounds("callout-body-Keyboard").is_some(), open, "{key}");
    }
    cx.update(|_, cx| assert!(editor.read(cx).focused.is_none()));
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn nested_callouts_keep_independent_folds_and_atomic_source_editing(cx: &mut TestAppContext) {
    let source = "> [!note]+ Outer\n> intro\n>\n> > [!tip]- Inner\n> > hidden\n>\n> | A | B |\n> | --- | ---: |\n> | **x** | 2 |\n";
    let (dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("callout-body-Outer").is_some());
    assert!(cx.debug_bounds("callout-title-Inner").is_some());
    assert!(cx.debug_bounds("callout-body-Inner").is_none());
    let toggle = cx.debug_bounds("callout-toggle-Inner").unwrap();
    cx.simulate_click(toggle.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("callout-body-Inner").is_some());
    assert!(cx.debug_bounds("callout-body-Outer").is_some());
    for expanded in [false, true] {
        let toggle = cx.debug_bounds("callout-toggle-Outer").unwrap();
        cx.simulate_click(toggle.center(), gpui_kit::Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(cx.debug_bounds("callout-body-Outer").is_some(), expanded);
    }
    assert!(cx.debug_bounds("callout-body-Inner").is_some(), "parent folding retains the child choice");
    let title = cx.debug_bounds("callout-title-Inner").unwrap();
    cx.simulate_click(title.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert_eq!(e.units.len(), 1);
        let f = e.focused.as_ref().expect("clicking nested content edits the outer source unit");
        assert!(f.enter_newline);
        assert_eq!(f.textarea.read(cx).value(), source.trim_end());
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("callout-body-Inner").is_some(), "source editing retains the child choice");
    focus_unit(&editor, 0, cx);
    cx.simulate_keystrokes("cmd-down");
    cx.simulate_input("!");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), source);
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), source);
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("callout-title-Outer").is_some());
}

#[gpui_kit::test]
fn callout_preview_handles_quotes_lists_empty_and_literal_markers(cx: &mut TestAppContext) {
    let source = "> ordinary quote\n>\n> > [!example] Nested\n> > body\n\n- > [!question] Listed\n  > answer\n\n> [!note]\n\n```md\n> [!danger] Literal\n```\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    for selector in ["callout-body-Nested", "callout-body-Listed", "callout-title-Note"] {
        assert!(cx.debug_bounds(selector).is_some(), "{selector}");
    }
    assert!(cx.debug_bounds("callout-title-Literal").is_none());
    assert!(cx.debug_bounds("callout-body-Note").is_none());
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn search_reveals_content_inside_collapsed_callouts(cx: &mut TestAppContext) {
    let source = "> [!note]- Outer\n> > [!tip]- Inner\n> > needle\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.run_until_parked();
    cx.update(|_, cx| editor.update(cx, |e, cx| e.reveal_indexed_block(0, cx)));
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("callout-body-Outer").is_some());
    assert!(cx.debug_bounds("callout-body-Inner").is_some());
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert_eq!(e.highlight, Some(e.units[0].id));
        assert!(e.reveal.is_none());
        assert!(e.focused.is_none());
        assert!(!e.dirty);
    });
    cx.executor().advance_clock(Duration::from_millis(1600));
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("callout-body-Inner").is_some(), "remains open after highlight fades");
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn table_preview_enables_horizontal_overflow(cx: &mut TestAppContext) {
    let source = "| Name | 中间 | Count |\n| :--- | :---: | ---: |\n| **bold** | `code` | 42 |\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.run_until_parked();
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert_eq!(e.units.len(), 1);
        let style = e.text_view_style(cx);
        assert_eq!(style.table.overflow.x, Some(gpui_kit::Overflow::Scroll));
        assert_eq!(style.table_head.text.font_weight, Some(gpui_kit::FontWeight::SEMIBOLD));
    });
    focus_unit(&editor, 0, cx);
    cx.update(|_, cx| {
        let e = editor.read(cx);
        let f = e.focused.as_ref().unwrap();
        assert!(f.enter_newline);
        assert_eq!(f.textarea.read(cx).value(), source.trim_end());
    });
    cx.simulate_keystrokes("escape");
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn wide_table_scroll_keeps_vertical_document_navigation(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, ScrollDelta, ScrollWheelEvent, TouchPhase, point, px};
    let mut source = format!(
        "| {} |\n| {} |\n| {} |\n\n",
        ["Heading"; 12].join(" | "),
        ["---"; 12].join(" | "),
        ["wide cell with content"; 12].join(" | ")
    );
    source.push_str(&(0..60).map(|i| format!("paragraph {i}\n\n")).collect::<String>());
    let (_dir, _vault, _path, editor) = open(cx, &source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.simulate_resize(gpui_kit::size(px(420.), px(600.)));
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let bounds = cx.debug_bounds("unit-1").unwrap();
    assert!(bounds.size.width <= px(420.));
    let position = bounds.origin + point(px(20.), px(20.));
    for touch_phase in [TouchPhase::Started, TouchPhase::Ended] {
        cx.simulate_event(ScrollWheelEvent {
            position,
            delta: ScrollDelta::Pixels(point(px(-150.), px(0.))),
            modifiers: Modifiers::default(),
            touch_phase,
        });
    }
    cx.run_until_parked();
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert_eq!(e.scroll.offset().y, px(0.), "horizontal gestures stay in the table");
        assert!(e.focused.is_none());
    });
    cx.simulate_event(ScrollWheelEvent {
        position,
        delta: ScrollDelta::Pixels(point(px(0.), px(-120.))),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Started,
    });
    cx.run_until_parked();
    cx.update(|_, cx| assert!(editor.read(cx).scroll.offset().y < px(0.), "vertical scrolling bubbles out"));
    assert_eq!(text(&editor, cx), source);
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

/// `/` 菜单（7.1）：块首输入 `/` 打开、继续输入过滤、↑↓ 选择、⏎ 确认 = 转换当前块并删掉命令；
/// Esc 只关菜单、保留字符；空段落上确认写出骨架；一次 ⌘Z 撤掉整个动作。
#[gpui_kit::test]
fn slash_menu_converts_block(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "hello\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let menu_open = |cx: &mut VisualTestContext| cx.update(|_, cx| editor.read(cx).slash.is_some());
    let selected_label = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            let m = editor.read(cx).slash.as_ref().expect("menu open");
            super::slash::ITEMS[m.matches[m.selected]].label
        })
    };

    // 词中间的 `/` 不触发；空格后触发
    focus_unit(&editor, 0, cx);
    cx.simulate_input("/");
    assert!(!menu_open(cx));
    cx.simulate_keystrokes("backspace");
    cx.simulate_input(" /");
    assert!(menu_open(cx));
    assert_eq!(selected_label(cx), "文本");
    // 过滤 + ↓ 选择
    cx.simulate_input("h");
    assert_eq!(selected_label(cx), "标题 1");
    cx.simulate_keystrokes("down");
    assert_eq!(selected_label(cx), "标题 2");
    // ⏎ 确认：命令删掉，块变成二级标题
    cx.simulate_keystrokes("enter");
    assert!(!menu_open(cx));
    assert_eq!(text(&editor, cx), "## hello\n");
    // 焦点仍在块尾，可以继续输入
    cx.simulate_input("!");
    assert_eq!(text(&editor, cx), "## hello!\n");
    // 一次撤销回到转换前（命令删除与转换合成一组）
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "## hello\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "hello /h\n");
    cx.simulate_keystrokes("cmd-shift-z");
    assert_eq!(text(&editor, cx), "## hello\n");

    // Esc 只关菜单、保留字符
    cx.simulate_input(" /co");
    assert!(menu_open(cx));
    assert_eq!(selected_label(cx), "代码块");
    cx.simulate_keystrokes("escape");
    assert!(!menu_open(cx));
    assert_eq!(text(&editor, cx), "## hello /co\n");
    cx.update(|_, cx| assert!(editor.read(cx).focused.is_some()));
    cx.simulate_keystrokes("backspace backspace backspace backspace");
    assert_eq!(text(&editor, cx), "## hello\n");

    // 空段落：⏎ 新段落后 `/任务` Tab 确认，写出 `- [ ] ` 并在其后继续输入
    cx.simulate_keystrokes("enter");
    cx.simulate_input("/任务");
    assert_eq!(selected_label(cx), "任务");
    cx.simulate_keystrokes("tab");
    assert_eq!(text(&editor, cx), "## hello\n\n- [ ] \n");
    cx.simulate_input("buy milk");
    assert_eq!(text(&editor, cx), "## hello\n\n- [ ] buy milk\n");

    // 没有匹配时菜单关闭
    cx.simulate_input(" /zzz");
    assert!(!menu_open(cx));
    cx.simulate_keystrokes("backspace backspace backspace backspace backspace");
    assert_eq!(text(&editor, cx), "## hello\n\n- [ ] buy milk\n");
}

/// `⌘⌥0–8` 直接转换当前块。
#[gpui_kit::test]
fn convert_shortcuts(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "a\n\nb\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 1, cx);
    cx.simulate_keystrokes("cmd-alt-4");
    assert_eq!(text(&editor, cx), "a\n\n- b\n");
    cx.simulate_keystrokes("cmd-alt-6");
    assert_eq!(text(&editor, cx), "a\n\n- [ ] b\n");
    cx.simulate_keystrokes("cmd-alt-1");
    assert_eq!(text(&editor, cx), "a\n\n# b\n");
    cx.simulate_keystrokes("cmd-alt-8");
    assert_eq!(text(&editor, cx), "a\n\n```\nb\n```\n");
    // 代码块里光标在正文末：继续输入进正文
    cx.simulate_input("2");
    assert_eq!(text(&editor, cx), "a\n\n```\nb2\n```\n");
    cx.simulate_keystrokes("cmd-alt-0");
    assert_eq!(text(&editor, cx), "a\n\nb2\n");
    cx.simulate_keystrokes("cmd-alt-7");
    assert_eq!(text(&editor, cx), "a\n\n> b2\n");
}

/// 标题编辑（5.1）：没有 front matter 时提交 = 请工作区重命名；有 `title:` 时直接改文档。
#[gpui_kit::test]
fn title_edit_renames_or_edits_front_matter(cx: &mut TestAppContext) {
    use std::cell::RefCell;
    use std::rc::Rc;

    let (_dir, _vault, _path, editor) = open(cx, "body\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let renames: Rc<RefCell<Vec<String>>> = Rc::default();
    let sink = renames.clone();
    cx.update(|_, cx| {
        cx.subscribe(&editor, move |_, ev, _| {
            if let super::EditorEvent::RenameRequested(name) = ev {
                sink.borrow_mut().push(name.clone());
            }
        })
        .detach();
    });

    // 文件名即标题；全选后输入替换，⏎ 提交
    cx.update(|_, cx| assert_eq!(editor.read(cx).title(), "a"));
    cx.update(|window, cx| editor.update(cx, |e, cx| e.start_title_edit(window, cx)));
    cx.run_until_parked();
    cx.simulate_input("新名字");
    cx.simulate_keystrokes("enter");
    assert_eq!(renames.borrow().as_slice(), ["新名字".to_string()]);
    cx.update(|_, cx| assert!(editor.read(cx).title_edit.is_none()));
    // 文档没被动
    assert_eq!(text(&editor, cx), "body\n");

    // Esc 取消：不发事件
    cx.update(|window, cx| editor.update(cx, |e, cx| e.start_title_edit(window, cx)));
    cx.run_until_parked();
    cx.simulate_input("x");
    cx.simulate_keystrokes("escape");
    assert_eq!(renames.borrow().len(), 1);
}

/// 有 front matter `title:` 时，标题提交改的是文档里的值（必要时加引号），不重命名文件。
#[gpui_kit::test]
fn title_edit_with_front_matter_edits_document(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "---\ntitle: 旧题\ntags: [x]\n---\n\nbody\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|_, cx| assert_eq!(editor.read(cx).title(), "旧题"));

    cx.update(|window, cx| editor.update(cx, |e, cx| e.start_title_edit(window, cx)));
    cx.run_until_parked();
    cx.simulate_input("新题");
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "---\ntitle: 新题\ntags: [x]\n---\n\nbody\n");
    cx.update(|_, cx| assert_eq!(editor.read(cx).title(), "新题"));

    // 含冒号的值加引号；再改回去时引号一并替换
    cx.update(|window, cx| editor.update(cx, |e, cx| e.start_title_edit(window, cx)));
    cx.run_until_parked();
    cx.simulate_input("a: b");
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "---\ntitle: \"a: b\"\ntags: [x]\n---\n\nbody\n");
    cx.update(|_, cx| assert_eq!(editor.read(cx).title(), "a: b"));
    cx.update(|window, cx| editor.update(cx, |e, cx| e.start_title_edit(window, cx)));
    cx.run_until_parked();
    cx.simulate_input("plain");
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "---\ntitle: plain\ntags: [x]\n---\n\nbody\n");

    // 改标题也走自动保存
    cx.executor().advance_clock(Duration::from_millis(600));
    cx.run_until_parked();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.md")).unwrap(),
        "---\ntitle: plain\ntags: [x]\n---\n\nbody\n"
    );
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

#[gpui_kit::test]
fn external_reload_resets_callout_fold_choices(cx: &mut TestAppContext) {
    let source = "> [!note]- Before\n> old\n";
    let (dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let toggle = cx.debug_bounds("callout-toggle-Before").unwrap();
    cx.simulate_click(toggle.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("callout-body-Before").is_some());
    let old_id = cx.update(|_, cx| editor.read(cx).units[0].id);

    let replacement = "> [!note]- After\n> new from disk\n";
    let file = dir.path().join("a.md");
    std::fs::write(&file, replacement).unwrap();
    let past = std::time::SystemTime::now() - Duration::from_secs(5);
    std::fs::File::open(&file).unwrap().set_modified(past).unwrap();
    cx.executor().advance_clock(Duration::from_millis(2100));
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(text(&editor, cx), replacement);
    cx.update(|_, cx| assert_eq!(editor.read(cx).units[0].id, old_id));
    assert!(cx.debug_bounds("callout-title-After").is_some());
    assert!(cx.debug_bounds("callout-body-After").is_none(), "new document uses its own fold defaults");
    assert_eq!(std::fs::read_to_string(file).unwrap(), replacement);
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
    let n = std::fs::read_dir(dir.path())
        .unwrap()
        .filter(|e| e.as_ref().is_ok_and(|e| e.file_name().to_string_lossy().starts_with("a.conflict-")));
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

/// 搜索命中定位：第 n 个可索引块高亮 1.5 s，并被滚到视口上 1/3 处（越界时定位到最后一个块）。
#[gpui_kit::test]
fn reveal_indexed_block_scrolls_and_highlights(cx: &mut TestAppContext) {
    // 60 段：测试窗口 1080 px 高，要保证目标块上下都有足够内容可滚
    let body: String = (0..60).map(|i| format!("paragraph {i}\n\n")).collect();
    let (_dir, _vault, _path, editor) = open(cx, &body);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let draw = |cx: &mut VisualTestContext| cx.update(|window, cx| window.draw(cx).clear(cx));

    draw(cx);
    assert_eq!(cx.update(|_, cx| editor.read(cx).scroll.offset().y), gpui_kit::px(0.));
    cx.update(|_, cx| editor.update(cx, |e, cx| e.reveal_indexed_block(25, cx)));
    let target = cx.update(|_, cx| {
        let e = editor.read(cx);
        let leaves = aero_core::index::indexable_leaves(&e.doc);
        assert_eq!(leaves.len(), 60);
        leaves[25].id
    });
    cx.update(|_, cx| assert_eq!(editor.read(cx).highlighted_block(), Some(target)));

    // 第一帧：子元素布局完成后改 offset；第二帧按新 offset 画
    draw(cx);
    cx.run_until_parked();
    let offset = cx.update(|_, cx| editor.read(cx).scroll.offset().y);
    assert!(offset < gpui_kit::px(-100.), "scrolled down, got {offset:?}");
    cx.update(|_, cx| assert!(editor.read(cx).reveal.is_none(), "one-shot"));
    draw(cx);
    let container = cx.update(|_, cx| editor.read(cx).scroll.bounds());
    let selector: &'static str = Box::leak(format!("unit-{}", target.0).into_boxed_str());
    let unit = cx.debug_bounds(selector).expect("target block is laid out");
    let rel = (unit.origin.y - container.origin.y) / container.size.height;
    assert!((0.28..0.38).contains(&rel), "target sits at the upper third of the viewport, got {rel}");

    // 1.5 s 后高亮消失
    cx.executor().advance_clock(Duration::from_millis(1600));
    cx.run_until_parked();
    cx.update(|_, cx| assert_eq!(editor.read(cx).highlighted_block(), None));

    // 越界：落到最后一个块
    cx.update(|_, cx| editor.update(cx, |e, cx| e.reveal_indexed_block(999, cx)));
    let last = cx.update(|_, cx| aero_core::index::indexable_leaves(&editor.read(cx).doc).last().unwrap().id);
    cx.update(|_, cx| assert_eq!(editor.read(cx).highlighted_block(), Some(last)));
}
