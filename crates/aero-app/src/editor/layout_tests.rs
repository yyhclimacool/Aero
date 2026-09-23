//! 浏览与编辑同一副样子：点进一块前后，每一块的位置、高度、字形都不变。
//!
//! 无头 GPUI 窗口用的是真实的 macOS 文字系统，测出来的像素与应用里一致。

use std::sync::Arc;

use aero_core::{Vault, VaultPath};
use gpui_kit::{
    Bounds, Entity, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    TestAppContext, VisualTestContext, point, px, size,
};

use super::Editor;
use super::presenter::{Key, Part};

const NOTE: &str = "# 一级标题

第一段有 **加粗文字**、*斜体*、`行内代码`、~~删除线~~、==高亮==，以及 [[目标笔记|双链别名]] 和 [外部链接](https://example.com)，还有 [红色文字]{.red} 这种颜色。

第二段是一段很长的文字，用来测试自动换行时预览和编辑是否在同一个位置折行。这一段需要足够长，才能在正文列里折成至少三行。我们继续写一些内容来填充，让用户完全感知不到 Markdown 语法的存在。

## 二级标题

- 无序列表 **加粗**
- [ ] 待办事项
- [x] 已完成事项

| 状态 | 今天要做的事 | 关联 |
| :--- | :--- | :---: |
| 进行中 | 做一个小型写作计划，这个单元格的文字比较长需要自动换行显示 | 甲 |
| 待整理 | 给重要段落加链接 | 乙 |

> 好笔记不必一次写完；
> 能让未来的自己继续思考就很好。

> [!note] 留下上下文
> 写清楚为什么，比只记一个结论更容易复用。

> [!tip]
> 没有标题的提示。

最后一段。
";

fn open(cx: &mut TestAppContext, body: &str) -> (tempfile::TempDir, Entity<Editor>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::actions::init(cx);
    });
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.md"), body).unwrap();
    let vault = Arc::new(Vault::open(dir.path()).unwrap());
    let path = VaultPath::new("a.md").unwrap();
    let (doc, stamp) = Editor::load(vault.clone(), path.clone()).unwrap();
    let (editor, _) = cx.add_window_view(|_, cx| Editor::new(vault, path, doc, stamp, cx));
    (dir, editor)
}

fn draw(cx: &mut VisualTestContext) {
    for _ in 0..3 {
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }
}

/// 每一块的外框（含 gutter 的那一行）。
fn rows(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Vec<(String, Bounds<Pixels>)> {
    let ids: Vec<u64> = cx.update(|_, cx| editor.read(cx).units.iter().map(|u| u.id.0).collect());
    let mut out = Vec::new();
    for id in ids {
        let selector = format!("row-{id}");
        if let Some(bounds) = cx.debug_bounds(Box::leak(selector.clone().into_boxed_str())) {
            out.push((selector, bounds));
        }
    }
    out
}

fn text_area(editor: &Entity<Editor>, key: Key, cx: &mut VisualTestContext) -> Bounds<Pixels> {
    cx.update(|_, cx| editor.read(cx).presenters[&key].textarea.read(cx).input_bounds())
}

fn press(cx: &mut VisualTestContext, position: Point<Pixels>) {
    let (modifiers, button) = (Modifiers::default(), MouseButton::Left);
    cx.simulate_event(MouseDownEvent { position, modifiers, button, click_count: 1, first_mouse: false });
}

fn release(cx: &mut VisualTestContext, position: Point<Pixels>) {
    let (modifiers, button) = (Modifiers::default(), MouseButton::Left);
    cx.simulate_event(MouseUpEvent { position, modifiers, button, click_count: 1 });
}

fn click(cx: &mut VisualTestContext, position: Point<Pixels>) {
    press(cx, position);
    release(cx, position);
    draw(cx);
}

/// 点进任何一段文字（标题、段落、列表、任务、单元格、引用、callout 标题与正文），所有块都不挪动。
#[gpui_kit::test]
fn clicking_into_any_text_moves_nothing(cx: &mut TestAppContext) {
    let (_dir, editor) = open(cx, NOTE);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.simulate_resize(size(px(900.), px(2400.)));
    draw(cx);
    let before = rows(&editor, cx);
    assert!(before.len() > 10, "fixture renders every block: {before:?}");
    let keys: Vec<Key> = cx.update(|_, cx| editor.read(cx).presenters.keys().copied().collect());
    assert!(keys.iter().any(|k| matches!(k.part, Part::Cell(..))), "table cells are text areas");
    assert!(keys.iter().any(|k| matches!(k.part, Part::CalloutBody)), "callout bodies are text areas");
    for key in keys {
        let area = text_area(&editor, key, cx);
        click(cx, area.origin + point(px(4.), area.size.height / 2.));
        let focused = cx.update(|_, cx| {
            let e = editor.read(cx);
            e.focused.as_ref().map(|f| f.textarea.clone()) == Some(e.presenters[&key].textarea.clone())
        });
        assert!(focused, "{key:?} takes the click");
        assert_eq!(rows(&editor, cx), before, "clicking {key:?} moved a block");
        cx.simulate_keystrokes("escape escape");
        draw(cx);
    }
}

/// 在另一块里按下并拖动，直接选中那一块的文字，不必先点一下。
#[gpui_kit::test]
fn dragging_in_another_block_selects_text_right_away(cx: &mut TestAppContext) {
    let (_dir, editor) = open(cx, NOTE);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.simulate_resize(size(px(900.), px(2400.)));
    draw(cx);
    let (first, second) = cx.update(|_, cx| {
        let e = editor.read(cx);
        (Key::unit(e.units[1].id), Key::unit(e.units[2].id))
    });
    let a = text_area(&editor, first, cx);
    click(cx, a.origin + point(px(10.), px(8.)));
    let b = text_area(&editor, second, cx);
    let start = b.origin + point(px(2.), px(10.));
    let end = b.origin + point(px(200.), px(40.));
    press(cx, start);
    for step in 1..=8 {
        let t = step as f32 / 8.;
        let position = point(start.x + (end.x - start.x) * t, start.y + (end.y - start.y) * t);
        cx.simulate_event(MouseMoveEvent {
            position,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        });
    }
    release(cx, end);
    draw(cx);
    cx.update(|_, cx| {
        let e = editor.read(cx);
        let f = e.focused.as_ref().expect("editing the second block");
        assert_eq!(f.textarea, e.presenters[&second].textarea);
        let selected = f.textarea.read(cx).selected_range();
        assert!(selected.len() > 20, "drag selected {selected:?}");
    });
}

/// 方框点一下就勾上，不进编辑态，别的块纹丝不动。
#[gpui_kit::test]
fn task_boxes_toggle_in_place(cx: &mut TestAppContext) {
    let (dir, editor) = open(cx, NOTE);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.simulate_resize(size(px(900.), px(2400.)));
    draw(cx);
    let before = rows(&editor, cx);
    let item = cx.update(|_, cx| {
        let e = editor.read(cx);
        e.units.iter().find(|u| u.list.as_ref().is_some_and(|l| l.checked == Some(false))).unwrap().id
    });
    let task = cx.update(|_, cx| {
        let e = editor.read(cx);
        e.doc
            .blocks()
            .iter()
            .find(|(_, b)| b.children.iter().any(|c| c.id == item))
            .map(|(_, b)| b.id)
            .unwrap()
    });
    let checkbox = cx.debug_bounds(Box::leak(format!("task-{}", task.0).into_boxed_str())).expect("checkbox");
    click(cx, checkbox.center());
    let text = cx.update(|_, cx| editor.read(cx).doc.to_string());
    assert!(text.contains("- [x] 待办事项"), "{text}");
    assert!(cx.update(|_, cx| editor.read(cx).focused.is_none()), "checking a box is not editing");
    assert_eq!(rows(&editor, cx), before);
    let _ = dir;
}

/// 在单元格里打字：`|` 转义成 `\|`，控件只画在格子里，表格仍是一张表。
#[gpui_kit::test]
fn typing_in_a_cell_stays_in_the_cell(cx: &mut TestAppContext) {
    let (_dir, editor) = open(cx, NOTE);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.simulate_resize(size(px(900.), px(2400.)));
    draw(cx);
    let table = cx.update(|_, cx| {
        editor.read(cx).units.iter().find(|u| u.kind == aero_core::BlockKind::Table).unwrap().id
    });
    let key = Key { id: table, part: Part::Cell(2, 1) };
    let area = text_area(&editor, key, cx);
    click(cx, area.origin + point(area.size.width - px(12.), px(8.)));
    cx.simulate_input("a|b");
    draw(cx);
    let text = cx.update(|_, cx| editor.read(cx).doc.to_string());
    assert!(text.contains("| 给重要段落加链接a\\|b | 乙 |"), "{text}");
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert_eq!(e.focused.as_ref().and_then(|f| f.id), Some(table), "the cell still belongs to the table");
        assert_eq!(e.units.iter().filter(|u| u.kind == aero_core::BlockKind::Table).count(), 1);
    });
}

/// 聚焦后链接的 `](url)`、`[[`、颜色的 `{.red}` 仍然藏着，光标贴着也不露出来。
#[gpui_kit::test]
fn focused_text_never_shows_markdown(cx: &mut TestAppContext) {
    let body = "看 [[目标|别名]] 和 [站点](https://example.com)，[红]{.red} 与 **粗**。\n";
    let (_dir, editor) = open(cx, body);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    draw(cx);
    let key = cx.update(|_, cx| Key::unit(editor.read(cx).units[0].id));
    let area = text_area(&editor, key, cx);
    click(cx, area.origin + point(px(4.), px(8.)));
    for marker in ["[[", "|", "]]", "](https://example.com)", "{.red}", "**"] {
        let at = body.find(marker).unwrap();
        cx.update(|_, cx| {
            let ta = editor.read(cx).presenters[&key].textarea.clone();
            ta.update(cx, |ta, cx| ta.set_selected_range(at..at, cx));
        });
        draw(cx);
        cx.update(|_, cx| {
            let ta = editor.read(cx).presenters[&key].textarea.read(cx);
            let caret = ta.selected_range().start;
            let hidden = ta.range_to_bounds(&(at..at + marker.len())).unwrap();
            assert!(hidden.size.width.abs() < px(0.5), "{marker} shows up with the caret at {caret}");
        });
    }
}

/// ↓ 在折了行的段落里先在行间走，到末行才去下一块；表格里 ↓ 到下一行的同一列。
#[gpui_kit::test]
fn arrow_keys_follow_visual_rows_and_table_columns(cx: &mut TestAppContext) {
    let (_dir, editor) = open(cx, NOTE);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.simulate_resize(size(px(700.), px(2400.)));
    draw(cx);
    let long = cx.update(|_, cx| Key::unit(editor.read(cx).units[2].id));
    let area = text_area(&editor, long, cx);
    assert!(area.size.height > px(40.), "fixture wraps into several rows: {area:?}");
    click(cx, area.origin + point(px(3.), px(6.)));
    cx.simulate_keystrokes("down");
    draw(cx);
    cx.update(|_, cx| {
        let e = editor.read(cx);
        let f = e.focused.as_ref().unwrap();
        assert_eq!(f.textarea, e.presenters[&long].textarea, "still in the wrapped paragraph");
        assert!(f.textarea.read(cx).cursor() > 0);
    });

    let table = cx.update(|_, cx| {
        editor.read(cx).units.iter().find(|u| u.kind == aero_core::BlockKind::Table).unwrap().id
    });
    let cell = |row, col| Key { id: table, part: Part::Cell(row, col) };
    let area = text_area(&editor, cell(0, 1), cx);
    click(cx, area.origin + point(px(3.), px(6.)));
    cx.simulate_keystrokes("down");
    draw(cx);
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert_eq!(e.focused.as_ref().unwrap().textarea, e.presenters[&cell(1, 1)].textarea);
    });
}

/// 每个看得见的字都画在控件里面，不会被挤出右边界裁掉。
#[gpui_kit::test]
fn every_visible_glyph_is_drawn_inside_the_text_area(cx: &mut TestAppContext) {
    let body = "第一段有 **加粗文字**、*斜体*、`行内代码`、~~删除线~~、==高亮==，以及 [[目标笔记|双链别名]] 和 [外部链接](https://example.com)，还有 [红色文字]{.red} 这种颜色。\n";
    let (_dir, editor) = open(cx, body);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    for width in (560..=860).step_by(9) {
        cx.simulate_resize(size(px(width as f32), px(900.)));
        draw(cx);
        cx.update(|_, cx| {
            let e = editor.read(cx);
            let id = e.units[0].id;
            let ta = e.presenters[&Key::unit(id)].textarea.read(cx);
            let text = ta.value().to_string();
            let analysis = aero_core::markup::analyze(&text);
            let area: Bounds<Pixels> = ta.input_bounds();
            for (i, c) in text.char_indices() {
                if analysis.conceals(i) || c == ' ' {
                    continue;
                }
                // 字的起点在控件里、而且右边还放得下它：行尾的那个字不会被裁掉
                let start = ta.range_to_bounds(&(i..i)).expect("laid out");
                let advance = cx.text_system().layout_width(
                    cx.text_system().resolve_font(&gpui_kit::font(".SystemUIFont")),
                    px(super::BODY_FONT_SIZE),
                    c,
                );
                assert!(
                    start.left() >= area.left() && start.left() + advance * 0.9 <= area.right(),
                    "width {width}: {c:?} at {i} starts at {:?}, text area {area:?}",
                    start.left()
                );
            }
        });
    }
}
