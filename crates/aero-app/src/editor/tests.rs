//! 无头 GPUI 测试：真正渲染编辑器、模拟按键，验证"控件 → Document → 磁盘"整条链路。
//!
//! 这里不 `use gpui_kit::*`：它会把 GPUI 的 `test` 属性宏带进作用域，遮住标准 `#[test]`。

use std::sync::Arc;
use std::time::Duration;

use aero_core::{Vault, VaultPath};
use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext};

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

fn wiki_catalog(editor: &Entity<Editor>, recent: &[&str], cx: &mut VisualTestContext) {
    cx.update(|_, cx| {
        editor.update(cx, |e, cx| {
            let notes = e
                .vault
                .notes()
                .unwrap()
                .into_iter()
                .map(|p| {
                    let title = e.vault.read(&p).unwrap().0.title_or(p.stem());
                    (p, title)
                })
                .collect();
            let generation = e.wiki.as_ref().expect("wiki menu").generation;
            let task = cx.background_spawn(async move { Ok(notes) });
            e.load_wiki_notes(
                generation,
                task,
                recent.iter().map(|p| VaultPath::new(p).unwrap()).collect(),
                cx,
            );
        })
    });
    cx.run_until_parked();
}

fn select_inline_text(editor: &Entity<Editor>, text: &str, cx: &mut VisualTestContext) {
    cx.update(|_, cx| {
        editor.read(cx).focused.as_ref().unwrap().textarea.clone().update(cx, |ta, cx| {
            let at = ta.value().find(text).unwrap();
            ta.set_selected_range(at..at + text.len(), cx);
        });
    });
    cx.run_until_parked();
}

fn selected_inline_text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    cx.update(|_, cx| {
        let ta = editor.read(cx).focused.as_ref().unwrap().textarea.read(cx);
        ta.value()[ta.selected_range()].to_owned()
    })
}

#[gpui_kit::test]
fn highlight_color_is_painted_in_source_and_removed_by_toggle(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "正文 中文😀 后文\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    select_inline_text(&editor, "中文😀", cx);
    cx.simulate_keystrokes("cmd-shift-h");
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        let focused = editor.read(cx).focused.as_ref().unwrap();
        let painted = focused.highlight.painted();
        assert!(!painted.is_empty(), "source mode must paint a background, not only insert ==");
        let ta = focused.textarea.read(cx);
        let at = ta.value().find("中文😀").unwrap();
        assert_eq!(painted[0].left(), ta.range_to_bounds(&(at..at)).unwrap().left());
        assert!(painted.iter().all(|r| r.size.width > gpui_kit::px(0.) && r.size.height > gpui_kit::px(0.)));
    });
    cx.simulate_keystrokes("cmd-shift-h");
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        assert!(editor.read(cx).focused.as_ref().unwrap().highlight.painted().is_empty());
    });
    assert_eq!(text(&editor, cx), "正文 中文😀 后文\n");
    cx.simulate_keystrokes("cmd-z");
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        assert!(!editor.read(cx).focused.as_ref().unwrap().highlight.painted().is_empty());
    });
}

#[gpui_kit::test]
fn highlight_source_wraps_by_row_and_does_not_paint_code(cx: &mut TestAppContext) {
    let source = format!("=={}==\n\n```md\n==code==\n```\n", "中文文字 ".repeat(300).trim_end());
    assert_eq!(aero_core::highlight::ranges(&source).len(), 1, "fixture must contain a parsed highlight");
    let (_dir, _vault, _path, editor) = open(cx, &source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        let f = editor.read(cx).focused.as_ref().unwrap();
        let rows = f.highlight.painted();
        assert!(rows.len() > 1, "wrapped highlight must have one rectangle per visual row");
        let bounds = f.textarea.read(cx).text_bounds().unwrap();
        assert!(rows.iter().all(|r| r.size.width <= bounds.size.width));
    });
    focus_unit(&editor, 1, cx);
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        assert!(editor.read(cx).focused.as_ref().unwrap().highlight.painted().is_empty());
    });
}

fn highlight_quads(cx: &mut VisualTestContext) -> Vec<gpui_kit::Quad> {
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        window
            .painted_quads()
            .into_iter()
            .filter(|q| q.background.as_solid() == Some(super::highlight_ui::color(cx)))
            .collect()
    })
}

#[gpui_kit::test]
fn highlight_history_clears_source_and_preview_background(cx: &mut TestAppContext) {
    let source = "正文 中文😀 后文\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    select_inline_text(&editor, "中文😀", cx);
    cx.simulate_keystrokes("cmd-shift-h");
    assert!(!highlight_quads(cx).is_empty());
    for (key, highlighted) in [("cmd-z", false), ("cmd-shift-z", true)] {
        cx.simulate_keystrokes(key);
        assert_eq!(!highlight_quads(cx).is_empty(), highlighted, "source {key}");
    }
    cx.simulate_keystrokes("escape");
    assert!(!highlight_quads(cx).is_empty(), "preview must paint yellow");
    for (key, highlighted) in [("cmd-z", false), ("cmd-shift-z", true)] {
        cx.simulate_keystrokes(key);
        assert_eq!(!highlight_quads(cx).is_empty(), highlighted, "preview {key}");
    }
    assert_eq!(text(&editor, cx), source.replace("中文😀", "==中文😀=="));
}

#[gpui_kit::test]
fn highlight_delimiter_deletion_invalidates_source_background(cx: &mut TestAppContext) {
    let source = "==中文😀==\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    assert!(!highlight_quads(cx).is_empty());
    select_inline_text(&editor, "==", cx);
    cx.simulate_keystrokes("backspace");
    assert_eq!(text(&editor, cx), "中文😀==\n");
    assert!(highlight_quads(cx).is_empty());
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), source);
    assert!(!highlight_quads(cx).is_empty());
    cx.simulate_keystrokes("cmd-shift-z");
    assert!(highlight_quads(cx).is_empty());
}

#[gpui_kit::test]
fn highlight_preview_paints_table_and_nested_callout_in_both_themes(cx: &mut TestAppContext) {
    let source = "| 表头 |\n|---|\n| ==单元格== |\n\n> [!note]+ Outer\n> ==正文==\n>\n> > [!tip]+ Inner\n> > ==嵌套==\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.run_until_parked();
    for mode in [gpui_kit::component::ThemeMode::Light, gpui_kit::component::ThemeMode::Dark] {
        cx.update(|window, cx| gpui_kit::component::Theme::change(mode, Some(window), cx));
        cx.run_until_parked();
        assert_eq!(highlight_quads(cx).len(), 3, "one background per highlighted body in {mode:?}");
        let toggle = cx.debug_bounds("callout-toggle-Outer").unwrap();
        cx.simulate_click(toggle.center(), gpui_kit::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(highlight_quads(cx).len(), 1, "folding hides both callout highlights");
        let toggle = cx.debug_bounds("callout-toggle-Outer").unwrap();
        cx.simulate_click(toggle.center(), gpui_kit::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(highlight_quads(cx).len(), 3);
    }
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn highlight_preview_accepts_trailing_whitespace(cx: &mut TestAppContext) {
    let source = "==正文==  \n续行\n\n# ==标题==  \n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.run_until_parked();
    assert_eq!(highlight_quads(cx).len(), 2);
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn highlight_preview_preserves_empty_cells_and_column_alignment(cx: &mut TestAppContext) {
    let source =
        "| A | B | C |\n|:---|---:|:---:|\n| ==left== | ==long== | ==center== |\n| | ==x== | ==中== |\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.run_until_parked();
    let quads = highlight_quads(cx);
    assert_eq!(quads.len(), 5);
    assert!(quads[3].bounds.left() > quads[0].bounds.right(), "empty first cell must keep x in column B");
    assert_eq!(quads[1].bounds.right(), quads[3].bounds.right(), "column B must remain right-aligned");
    assert_eq!(quads[2].bounds.center().x, quads[4].bounds.center().x, "column C must remain centered");
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn highlight_preview_preserves_hard_break_inside_mark(cx: &mut TestAppContext) {
    let source = "==a  \nb==\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.run_until_parked();
    let quads = highlight_quads(cx);
    assert_eq!(quads.len(), 2, "hard break splits highlighted text into two visual rows");
    assert!(quads[1].bounds.top() > quads[0].bounds.top());
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn highlight_preview_preserves_inline_html_break(cx: &mut TestAppContext) {
    let source = "==a==<br>==b==\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.run_until_parked();
    let quads = highlight_quads(cx);
    assert_eq!(quads.len(), 2);
    assert!(quads[1].bounds.top() > quads[0].bounds.top(), "inline HTML break must remain a new row");
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn highlight_preview_wraps_long_text_without_horizontal_clipping(cx: &mut TestAppContext) {
    let source = format!("=={}==\n", "中文 wrap ".repeat(70).trim_end());
    let (_dir, _vault, _path, editor) = open(cx, &source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.run_until_parked();
    let quads = highlight_quads(cx);
    assert!(quads.len() > 1);
    for quad in quads {
        assert!(quad.bounds.size.width > gpui_kit::ScaledPixels(0.));
        assert!(quad.bounds.left() >= quad.content_mask.bounds.left());
        assert!(quad.bounds.right() <= quad.content_mask.bounds.right());
    }
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn highlight_wrapped_paragraph_reserves_height_before_table(cx: &mut TestAppContext) {
    use gpui_kit::{px, size};
    let source = format!(
        "=={}==\n\n| 表头 | 内容 |\n|---|---|\n| 普通 | 单元格 |\n",
        "这是一段用于检查自动换行的黄色高亮文本，包含中文😀，也包含 English words。".repeat(4),
    );
    let (_dir, _vault, _path, editor) = open(cx, &source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    for width in [1280., 620., 420., 1000.] {
        cx.simulate_resize(size(px(width), px(1400.)));
        cx.run_until_parked();
        for return_from_edit in [false, true] {
            if return_from_edit {
                focus_unit(&editor, 0, cx);
                cx.simulate_keystrokes("escape");
                cx.run_until_parked();
            }
            let quads = highlight_quads(cx);
            assert!(quads.len() > 1, "fixture must wrap before the table");
            let scale = cx.update(|window, _| window.scale_factor());
            let paragraph = cx.debug_bounds("unit-1").unwrap().scale(scale);
            let table = cx.debug_bounds("unit-2").unwrap().scale(scale);
            let last_line = quads.iter().map(|q| q.bounds.bottom()).max().unwrap();
            assert!(
                last_line <= paragraph.bottom(),
                "{width}: painted bottom {last_line:?}, paragraph {paragraph:?}"
            );
            assert!(
                last_line <= table.top(),
                "{width}: table {table:?} overlaps highlighted text ending at {last_line:?}"
            );
        }
    }
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn highlight_wrapped_table_cells_reserve_height_before_next_row(cx: &mut TestAppContext) {
    use gpui_kit::{px, size};
    let content = "中文😀 wrapped text ".repeat(8);
    let content = content.trim_end();
    let source = format!(
        "| Left | Center | Right |\n|:---|:---:|---:|\n| =={content}== | =={content}== | =={content}== |\n| next | next | next |\n"
    );
    let (_dir, _vault, _path, editor) = open(cx, &source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    for width in [1280., 620., 420., 1000.] {
        cx.simulate_resize(size(px(width), px(1400.)));
        cx.run_until_parked();
        let quads = highlight_quads(cx);
        assert!(quads.len() > 3, "fixture must wrap in the table cells");
        let scale = cx.update(|window, _| window.scale_factor());
        let cell = cx.debug_bounds("highlight-cell-1-0").unwrap().scale(scale);
        let next = cx.debug_bounds("highlight-cell-2-0").unwrap().scale(scale);
        for quad in quads {
            assert!(
                quad.bounds.bottom() <= cell.bottom(),
                "{width}: painted {:?} outside row {cell:?}",
                quad.bounds
            );
            assert!(quad.bounds.bottom() <= next.top(), "{width}: highlight overlaps next row");
        }
    }
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn highlight_table_preserves_hard_breaks_and_empty_rows(cx: &mut TestAppContext) {
    let source = "| A | B |\n|---|---:|\n| | |\n| | ==a<br><br>b== |\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.run_until_parked();
    let quads = highlight_quads(cx);
    assert_eq!(quads.len(), 2);
    assert_eq!(quads[0].bounds.right(), quads[1].bounds.right());
    assert!(quads[1].bounds.top() >= quads[0].bounds.bottom() + quads[0].bounds.size.height);
    let empty_row = cx.debug_bounds("highlight-cell-1-0").unwrap();
    let body = cx.debug_bounds("highlight-cell-2-1").unwrap();
    assert!(empty_row.size.height > gpui_kit::px(0.));
    assert!(body.top() >= empty_row.bottom());
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn inline_format_shortcuts_toggle_and_keep_selection_with_atomic_history(cx: &mut TestAppContext) {
    let source = "正文 中文😀 末尾\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    for (key, delimiter) in
        [("cmd-b", "**"), ("cmd-i", "*"), ("cmd-e", "`"), ("cmd-shift-s", "~~"), ("cmd-shift-h", "==")]
    {
        select_inline_text(&editor, "中文😀", cx);
        cx.simulate_keystrokes(key);
        let wrapped = source.replace("中文😀", &format!("{delimiter}中文😀{delimiter}"));
        assert_eq!(text(&editor, cx), wrapped, "{key}");
        assert_eq!(selected_inline_text(&editor, cx), "中文😀");
        cx.simulate_keystrokes(key);
        assert_eq!(text(&editor, cx), source);
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(text(&editor, cx), wrapped);
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(text(&editor, cx), source);
        cx.simulate_keystrokes("cmd-shift-z");
        assert_eq!(text(&editor, cx), wrapped);
        cx.simulate_keystrokes("cmd-shift-z");
        assert_eq!(text(&editor, cx), source);
    }
}

#[gpui_kit::test]
fn inline_format_empty_markers_type_inside_and_toggle_off(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| editor.update(cx, |e, cx| e.focus_at(0, None, window, cx)));
    for (key, marker) in
        [("cmd-b", "**"), ("cmd-i", "*"), ("cmd-e", "`"), ("cmd-shift-s", "~~"), ("cmd-shift-h", "==")]
    {
        cx.simulate_keystrokes(key);
        assert_eq!(text(&editor, cx), format!("{marker}{marker}\n"));
        cx.simulate_keystrokes(key);
        assert_eq!(text(&editor, cx), "\n");
        cx.simulate_keystrokes(key);
        cx.simulate_input("中文");
        assert_eq!(text(&editor, cx), format!("{marker}中文{marker}\n"));
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(text(&editor, cx), format!("{marker}{marker}\n"));
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(text(&editor, cx), "\n");
    }
}

#[gpui_kit::test]
fn inline_format_tables_callouts_links_and_whitespace(cx: &mut TestAppContext) {
    let source = "> [!note]- 提示\n> 中文\n\n| 表头 |\n|---|\n| a\\|中文 |\n\n[中文](https://example.com)\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    for ix in 0..3 {
        focus_unit(&editor, ix, cx);
        select_inline_text(&editor, "中文", cx);
        cx.simulate_keystrokes("cmd-b");
    }
    assert_eq!(text(&editor, cx), source.replace("中文", "**中文**"));
    cx.update(|_, cx| assert_eq!(editor.read(cx).units.len(), 3));
}

#[gpui_kit::test]
fn inline_format_invalid_context_keeps_document_and_history(cx: &mut TestAppContext) {
    let source = "```md\ninside\n```\n\n[链接](https://inside)\n\n$inside$\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    for ix in 0..3 {
        focus_unit(&editor, ix, cx);
        select_inline_text(&editor, "inside", cx);
        cx.simulate_keystrokes("cmd-b");
        assert_eq!(text(&editor, cx), source);
        cx.update(|_, cx| assert!(!editor.read(cx).doc.can_undo()));
    }
}

#[gpui_kit::test]
fn inline_format_preedit_is_untouched(cx: &mut TestAppContext) {
    use gpui_kit::EntityInputHandler;
    let (_dir, _vault, _path, editor) = open(cx, "正文\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    let input = cx.update(|_, cx| editor.read(cx).focused.as_ref().unwrap().textarea.clone());
    cx.update(|window, cx| {
        input.update(cx, |ta, cx| ta.replace_and_mark_text_in_range(None, "拼音", None, window, cx));
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("cmd-b");
    assert_eq!(text(&editor, cx), "正文\n");
    cx.update(|window, cx| input.update(cx, |ta, cx| ta.replace_text_in_range(None, "中文", window, cx)));
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "正文中文\n");
    select_inline_text(&editor, "中文", cx);
    cx.simulate_keystrokes("cmd-b");
    assert_eq!(text(&editor, cx), "正文**中文**\n");
}

#[gpui_kit::test]
fn auto_pairs_typing_skips_all_generated_delimiters_and_keeps_caret(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "正文\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input(" (**中** ==文== `码` $x$ \"引\")!");
    assert_eq!(text(&editor, cx), "正文 (**中** ==文== `码` $x$ \"引\")!\n");
    cx.simulate_input("末");
    assert_eq!(text(&editor, cx), "正文 (**中** ==文== `码` $x$ \"引\")!末\n");
}

#[gpui_kit::test]
fn auto_pairs_empty_backspace_and_undo_redo_are_atomic(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "正文\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input(" ");
    let original = text(&editor, cx);
    for (left, right) in
        [("(", ")"), ("\"", "\""), ("`", "`"), ("$", "$"), ("**", "**"), ("==", "=="), ("[[", "]]")]
    {
        cx.simulate_input(left);
        let paired = text(&editor, cx);
        assert_eq!(paired, original.replacen('\n', &format!("{left}{right}\n"), 1));
        cx.simulate_keystrokes("backspace");
        assert_eq!(text(&editor, cx), original, "{left}");
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(text(&editor, cx), paired);
        cx.simulate_keystrokes("cmd-shift-z");
        assert_eq!(text(&editor, cx), original);
    }
}

#[gpui_kit::test]
fn auto_pairs_skip_has_no_undo_entry_and_loaded_closers_are_literal(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "body\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input("(");
    assert_eq!(text(&editor, cx), "body()\n");
    cx.simulate_input(")");
    assert_eq!(text(&editor, cx), "body()\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "body\n");
    cx.simulate_keystrokes("cmd-shift-z");
    cx.simulate_keystrokes("left");
    cx.simulate_input(")");
    assert_eq!(text(&editor, cx), "body())\n"); // 重建编辑控件后不猜测原有符号的来源。
}

#[gpui_kit::test]
fn auto_pairs_work_with_wikilink_embed_completion_and_reuse_brackets(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "正文\n");
    std::fs::write(dir.path().join("b.md"), "# B\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    for prefix in [" [[", " ![["] {
        cx.simulate_input(prefix);
        assert!(text(&editor, cx).ends_with("[[]]\n"));
        cx.simulate_input("b");
        wiki_catalog(&editor, &[], cx);
        cx.simulate_keystrokes("enter");
    }
    assert_eq!(text(&editor, cx), "正文 [[b]] ![[b]]\n");
    cx.simulate_input(" [[missing]]!");
    assert_eq!(text(&editor, cx), "正文 [[b]] ![[b]] [[missing]]!\n");
    cx.update(|_, cx| assert!(editor.read(cx).wiki.is_none()));
}

#[gpui_kit::test]
fn auto_pairs_preserve_paste_selection_and_code_context(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "body\n\n```\ncode\n```\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.update(|_, cx| cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(" ( [[ ** $ `".into())));
    cx.simulate_keystrokes("cmd-v");
    assert!(text(&editor, cx).starts_with("body ( [[ ** $ `\n"));
    select_inline_text(&editor, "body", cx);
    cx.simulate_input("(");
    assert!(text(&editor, cx).starts_with("( ( [[ ** $ `\n"));
    focus_unit(&editor, 1, cx);
    select_inline_text(&editor, "code", cx);
    cx.simulate_keystrokes("right");
    cx.simulate_input(" ( [[ ** $ \"");
    assert!(text(&editor, cx).contains("code ( [[ ** $ \"\n```"));
}

#[gpui_kit::test]
fn auto_pairs_nested_callouts_and_tables_preserve_structure(cx: &mut TestAppContext) {
    let source = "> [!note]\n> 正文\n\n| A | B |\n| --- | --- |\n| 中文 | 末 |\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    for (ix, label) in [(0, "正文"), (1, "中文")] {
        focus_unit(&editor, ix, cx);
        select_inline_text(&editor, label, cx);
        cx.simulate_keystrokes("right");
        cx.simulate_input(" ((文");
        cx.simulate_keystrokes("backspace");
        cx.simulate_keystrokes("backspace");
        cx.simulate_input("中)");
    }
    assert_eq!(text(&editor, cx), source.replace("正文", "正文 (中)").replace("中文", "中文 (中)"));
    cx.update(|_, cx| assert_eq!(editor.read(cx).units.len(), 2));
}

#[gpui_kit::test]
fn auto_pairs_ime_preedit_and_committed_punctuation_stay_literal(cx: &mut TestAppContext) {
    use gpui_kit::EntityInputHandler;
    let (_dir, _vault, _path, editor) = open(cx, "正文\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    let input = cx.update(|_, cx| editor.read(cx).focused.as_ref().unwrap().textarea.clone());
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            e.observe_pair_key(
                &gpui_kit::KeystrokeEvent {
                    keystroke: gpui_kit::Keystroke::parse("(").unwrap(),
                    action: None,
                    context_stack: vec![],
                },
                window,
                cx,
            )
        });
        input.update(cx, |ta, cx| ta.replace_and_mark_text_in_range(None, "(", None, window, cx));
    });
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "正文\n");
    cx.update(|window, cx| input.update(cx, |ta, cx| ta.replace_text_in_range(None, "(", window, cx)));
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "正文(\n");
    cx.simulate_input("(");
    assert_eq!(text(&editor, cx), "正文(()\n");
}

#[gpui_kit::test]
fn inline_link_shortcut_wraps_selection_prefills_url_and_undoes_atomically(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "正文 中文😀 后\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    select_inline_text(&editor, "中文😀", cx);
    cx.update(|_, cx| {
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string("https://example.com/a(b)?a=1&b=2".into()))
    });
    cx.simulate_keystrokes("cmd-k");
    let completed = "正文 [中文😀](https://example.com/a%28b%29?a=1&amp;b=2) 后\n";
    assert_eq!(text(&editor, cx), completed);
    assert_eq!(selected_inline_text(&editor, cx), "https://example.com/a%28b%29?a=1&amp;b=2");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "正文 中文😀 后\n");
    cx.simulate_keystrokes("cmd-shift-z");
    assert_eq!(text(&editor, cx), completed);
    select_inline_text(&editor, "中文", cx);
    cx.simulate_keystrokes("cmd-k");
    cx.simulate_input("https://new.example");
    assert_eq!(text(&editor, cx), "正文 [中文😀](https://new.example) 后\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), completed);
}

#[gpui_kit::test]
fn inline_link_placeholder_is_selected_and_new_input_preserves_markdown(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "正文 **加粗** 后\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    select_inline_text(&editor, "**加粗**", cx);
    cx.update(|_, cx| cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string("不是网址".into())));
    cx.simulate_keystrokes("cmd-k");
    assert_eq!(text(&editor, cx), "正文 [**加粗**](url) 后\n");
    assert_eq!(selected_inline_text(&editor, cx), "url");
    cx.simulate_input("https://example.com");
    cx.simulate_keystrokes("right");
    cx.simulate_input("!");
    assert_eq!(text(&editor, cx), "正文 [**加粗**](https://example.com)! 后\n");
}

#[gpui_kit::test]
fn inline_link_existing_addresses_keep_titles_callouts_and_table_pipes(cx: &mut TestAppContext) {
    let source = "> [!note]\n> [中文](<https://old.example/a b> '标题')\n\n| A | B |\n| --- | --- |\n| a\\|b [中\\|文](https://old.example \"T\") | 末 |\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    for (ix, label, url) in [(0, "中文", "https://old.example/a b"), (1, "中\\|文", "https://old.example")]
    {
        focus_unit(&editor, ix, cx);
        select_inline_text(&editor, label, cx);
        let before = text(&editor, cx);
        cx.simulate_keystrokes("cmd-k");
        assert_eq!(text(&editor, cx), before);
        assert_eq!(selected_inline_text(&editor, cx), url);
        cx.update(|_, cx| assert!(!editor.read(cx).doc.can_undo()));
        cx.simulate_input("https://new.example");
        let mut expected = before.clone();
        let at = if ix == 0 { before.find(url).unwrap() } else { before.rfind(url).unwrap() };
        expected.replace_range(at..at + url.len(), "https://new.example");
        assert_eq!(text(&editor, cx), expected);
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(text(&editor, cx), source);
    }
}

#[gpui_kit::test]
fn inline_link_without_selection_opens_and_reopens_note_completion(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "正文\n");
    std::fs::write(dir.path().join("b.md"), "# B\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_keystrokes("cmd-k");
    assert_eq!(text(&editor, cx), "正文[[]]\n");
    cx.update(|_, cx| assert!(editor.read(cx).wiki.is_some()));
    cx.simulate_keystrokes("escape");
    cx.update(|_, cx| assert!(editor.read(cx).wiki.is_none()));
    cx.simulate_keystrokes("cmd-k");
    assert_eq!(text(&editor, cx), "正文[[]]\n");
    cx.simulate_input("B");
    wiki_catalog(&editor, &[], cx);
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "正文[[b]]\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "正文[[B]]\n");
}

#[gpui_kit::test]
fn inline_link_shortcut_respects_code_math_images_and_multiline_selection(cx: &mut TestAppContext) {
    let source = "正文 `code` $math$ ![[embed]] ![image](url)\n第二行\n\n```\ncodeblock\n```\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    for label in ["ode", "math", "embed", "image", "正文 `code` $math$ ![[embed]] ![image](url)\n第二行"]
    {
        select_inline_text(&editor, label, cx);
        cx.simulate_keystrokes("cmd-k");
        assert_eq!(text(&editor, cx), source);
    }
    focus_unit(&editor, 1, cx);
    select_inline_text(&editor, "codeblock", cx);
    cx.simulate_keystrokes("cmd-k");
    assert_eq!(text(&editor, cx), source);
    cx.update(|_, cx| assert!(!editor.read(cx).doc.can_undo()));
}

#[gpui_kit::test]
fn inline_link_shortcut_does_not_consume_ime_preedit(cx: &mut TestAppContext) {
    use gpui_kit::EntityInputHandler;
    let (_dir, _vault, _path, editor) = open(cx, "正文\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    let input = cx.update(|_, cx| editor.read(cx).focused.as_ref().unwrap().textarea.clone());
    cx.update(|window, cx| {
        input.update(cx, |ta, cx| ta.replace_and_mark_text_in_range(None, "zhong", None, window, cx))
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("cmd-k");
    assert_eq!(text(&editor, cx), "正文\n");
    cx.update(|window, cx| input.update(cx, |ta, cx| ta.replace_text_in_range(None, "中文", window, cx)));
    cx.run_until_parked();
    select_inline_text(&editor, "中文", cx);
    cx.simulate_keystrokes("cmd-k");
    assert_eq!(text(&editor, cx), "正文[中文](url)\n");
}

#[gpui_kit::test]
fn embed_completion_inserts_images_pdf_and_notes_with_one_step_undo(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "正文\n");
    std::fs::create_dir(dir.path().join("a.assets")).unwrap();
    std::fs::create_dir(dir.path().join("docs")).unwrap();
    std::fs::write(dir.path().join("a.assets/图#.png"), b"image bytes").unwrap();
    std::fs::write(dir.path().join("docs/文档.pdf"), b"pdf bytes").unwrap();
    std::fs::write(dir.path().join("b.md"), "---\ntitle: 笔记\n---\n\nbody\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input(" ![[图");
    wiki_catalog(&editor, &[], cx);
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "正文 ![[图\\#.png]]\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "正文 ![[图]]\n");
    cx.simulate_keystrokes("cmd-shift-z");
    cx.simulate_input(" ![[文档");
    wiki_catalog(&editor, &[], cx);
    cx.simulate_keystrokes("shift-enter");
    cx.simulate_input(" ![[笔记");
    wiki_catalog(&editor, &[], cx);
    cx.simulate_keystrokes("tab");
    assert_eq!(text(&editor, cx), "正文 ![[图\\#.png]] ![[文档.pdf]] ![[b]]\n");
    let embeds = aero_core::links::scan_embeds(&text(&editor, cx));
    assert_eq!(embeds.len(), 3);
    assert!(embeds.iter().all(|e| e.parameter.is_none()));
    assert!(aero_core::links::scan(&text(&editor, cx)).is_empty());
    cx.simulate_input(" [[图");
    wiki_catalog(&editor, &[], cx);
    cx.update(|_, cx| {
        let menu = editor.read(cx).wiki.as_ref().unwrap();
        assert!(!menu.trigger.embed);
        assert!(menu.catalog.as_ref().unwrap().attachments.entries.is_empty());
        assert!(menu.matches.is_empty());
    });
}

#[gpui_kit::test]
fn embed_completion_mouse_disambiguates_and_preserves_closing_brackets_and_caret(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "body ![[]] after\n");
    for folder in ["x", "y"] {
        std::fs::create_dir(dir.path().join(folder)).unwrap();
        std::fs::write(dir.path().join(folder).join("photo.png"), b"image").unwrap();
    }
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.update(|_, cx| {
        editor.read(cx).focused.as_ref().unwrap().textarea.clone().update(cx, |ta, cx| {
            let at = "body ![[".len();
            ta.set_selected_range(at..at, cx);
        })
    });
    cx.simulate_input("y/photo");
    wiki_catalog(&editor, &[], cx);
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let row = cx.debug_bounds("wiki-row-0").unwrap();
    cx.simulate_click(row.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "body ![[y/photo.png]] after\n");
    cx.simulate_input("!");
    assert_eq!(text(&editor, cx), "body ![[y/photo.png]]! after\n");
    cx.simulate_keystrokes("cmd-z");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "body ![[y/photo]] after\n");
}

#[gpui_kit::test]
fn embed_completion_supports_unsaved_headings_and_assigns_block_id_atomically(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "# 当前标题\n\n正文\n\n目标正文\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input("新");
    focus_unit(&editor, 1, cx);
    cx.simulate_input(" ![[#当前");
    cx.simulate_keystrokes("tab");
    assert!(text(&editor, cx).contains("![[#当前标题新]]"));
    cx.simulate_input(" ![[#^目标");
    let before = text(&editor, cx);
    cx.simulate_keystrokes("shift-enter");
    let completed = text(&editor, cx);
    let embeds = aero_core::links::scan_embeds(&completed);
    assert_eq!(embeds.len(), 2);
    assert!(embeds[1].parameter.is_none());
    let fragment = embeds[1].fragment.as_deref().unwrap();
    assert!(completed.contains(&format!("目标正文 {fragment}")));
    cx.simulate_input("!");
    assert!(text(&editor, cx).contains(&format!("![[#{fragment}]]!")));
    cx.simulate_keystrokes("cmd-z");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), before);
    cx.simulate_keystrokes("cmd-shift-z");
    assert_eq!(text(&editor, cx), completed);
}

#[gpui_kit::test]
fn embed_completion_loading_errors_stale_replies_and_unresolved_targets(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "body\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input(" [[missing");
    let old = cx.update(|_, cx| editor.read(cx).wiki.as_ref().unwrap().generation);
    cx.update(|_, cx| {
        editor.read(cx).focused.as_ref().unwrap().textarea.clone().update(cx, |ta, cx| {
            ta.set_selected_range(0..ta.value().len(), cx);
        })
    });
    cx.simulate_input("body ![[missing");
    cx.update(|_, cx| {
        editor.update(cx, |e, cx| {
            assert_ne!(old, e.wiki.as_ref().unwrap().generation);
            let task = cx.background_spawn(async { Ok(vec![]) });
            e.load_wiki_notes(old, task, vec![], cx);
        })
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "body ![[missing]]\n");
    cx.update(|_, cx| {
        editor.update(cx, |e, cx| {
            assert!(e.wiki.as_ref().unwrap().catalog.is_none());
            let generation = e.wiki.as_ref().unwrap().generation;
            let task = cx.background_spawn(async { Err("目录不可读".into()) });
            e.load_wiki_notes(generation, task, vec![], cx);
        })
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("tab");
    assert_eq!(text(&editor, cx), "body ![[missing]]\n");
    wiki_catalog(&editor, &[], cx);
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "body ![[missing]]\n");
    assert!(!dir.path().join("missing.md").exists());
    cx.simulate_input(" ![[");
    cx.simulate_keystrokes("escape");
    cx.update(|_, cx| assert!(editor.read(cx).wiki.is_none()));
}

#[gpui_kit::test]
fn embed_completion_ime_in_callout_and_table_does_not_insert_alias(cx: &mut TestAppContext) {
    use gpui_kit::EntityInputHandler;
    let (dir, _vault, _path, editor) = open(cx, "> [!note]\n> 正文\n\n| A |\n| --- |\n| 内容 |\n");
    std::fs::create_dir(dir.path().join("a.assets")).unwrap();
    std::fs::write(dir.path().join("a.assets/中文.png"), b"image").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input(" ![[");
    wiki_catalog(&editor, &[], cx);
    let before = text(&editor, cx);
    let input = cx.update(|_, cx| editor.read(cx).focused.as_ref().unwrap().textarea.clone());
    cx.update(|window, cx| {
        input.update(cx, |ta, cx| ta.replace_and_mark_text_in_range(None, "zhong", None, window, cx))
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| e.on_split_block(&crate::actions::SplitBlock, window, cx))
    });
    assert_eq!(text(&editor, cx), before);
    cx.update(|window, cx| input.update(cx, |ta, cx| ta.replace_text_in_range(None, "中文", window, cx)));
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    assert!(text(&editor, cx).contains("> 正文 ![[中文.png]]"));
    focus_unit(&editor, 1, cx);
    cx.update(|_, cx| {
        editor.read(cx).focused.as_ref().unwrap().textarea.clone().update(cx, |ta, cx| {
            let at = ta.value().find("内容").unwrap() + "内容".len();
            ta.set_selected_range(at..at, cx);
        })
    });
    cx.simulate_input(" ![[中文");
    wiki_catalog(&editor, &[], cx);
    cx.simulate_keystrokes("shift-enter");
    assert!(text(&editor, cx).contains("| 内容 ![[中文.png]] |"));
    cx.update(|_, cx| assert_eq!(editor.read(cx).units.len(), 2));
    assert_eq!(std::fs::read(dir.path().join("a.assets/中文.png")).unwrap(), b"image");
}

#[gpui_kit::test]
fn embed_completion_rejects_removed_attachment_and_pdf_fragment_namespace(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "body\n");
    let asset = dir.path().join("paper.pdf");
    std::fs::write(&asset, b"pdf").unwrap();
    std::fs::write(dir.path().join("paper.pdf.md"), "# Note section\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input(" ![[paper.pdf#");
    wiki_catalog(&editor, &[], cx);
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "body ![[paper.pdf#]]\n");
    cx.update(|_, cx| {
        assert!(editor.read(cx).wiki.as_ref().unwrap().error.as_deref().unwrap().contains("PDF"));
        editor
            .read(cx)
            .focused
            .as_ref()
            .unwrap()
            .textarea
            .clone()
            .update(cx, |ta, cx| ta.set_selected_range(0..ta.value().len(), cx));
    });
    cx.simulate_input("body ![[paper.pdf.md#Note");
    wiki_catalog(&editor, &[], cx);
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "body ![[paper.pdf.md#Note section]]\n");
    cx.simulate_input(" ![[paper.pdf");
    wiki_catalog(&editor, &[], cx);
    // 明确选择附件候选，不能把同名笔记误当成 PDF。
    cx.update(|_, cx| {
        editor.update(cx, |e, _| {
            let menu = e.wiki.as_mut().unwrap();
            menu.selected =
                menu.matches.iter().position(|&i| i >= menu.catalog.as_ref().unwrap().notes.len()).unwrap();
        })
    });
    std::fs::remove_file(&asset).unwrap();
    cx.simulate_keystrokes("tab");
    assert!(text(&editor, cx).ends_with(" ![[paper.pdf]]\n"));
    cx.update(|_, cx| {
        assert!(editor.read(cx).wiki.as_ref().unwrap().error.as_deref().unwrap().contains("附件"))
    });
}

#[gpui_kit::test]
fn block_ref_current_note_assigns_and_inserts_as_one_undo_group(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "目标正文\n\n引用\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input("未保存");
    focus_unit(&editor, 1, cx);
    cx.simulate_input(" [[#^未保存");
    let before = text(&editor, cx);
    cx.update(|_, cx| {
        let e = editor.read(cx);
        let m = e.wiki.as_ref().unwrap();
        assert_eq!(m.matches.len(), 1);
        assert!(e.tag_menu.is_none());
        assert!(!e.doc.to_string().contains(" ^"));
    });
    cx.simulate_keystrokes("tab");
    let completed = text(&editor, cx);
    let span = aero_core::links::scan(&completed).remove(0);
    let fragment = span.fragment.unwrap();
    assert!(fragment.starts_with('^'));
    assert_eq!(fragment.len(), 7);
    assert_eq!(completed, format!("目标正文未保存 {fragment}\n\n引用 [[#{fragment}]]\n"));
    cx.simulate_input("!");
    assert!(text(&editor, cx).contains(&format!("[[#{fragment}]]!")));
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), completed);
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), before);
    cx.simulate_keystrokes("cmd-shift-z");
    assert_eq!(text(&editor, cx), completed);
    cx.update(|_, cx| {
        editor.update(cx, |e, cx| {
            e.reveal_fragment(&fragment, cx).unwrap();
            assert_eq!(e.highlight, Some(e.units[0].id));
        })
    });
}

#[gpui_kit::test]
fn block_ref_mouse_writes_external_id_and_undo_keeps_it(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "body [[]] after\n");
    let target = dir.path().join("C#.md");
    let original = "# Intro\n\n```rs\nneedle 中文\n```\n";
    std::fs::write(&target, original).unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.update(|_, cx| {
        editor.read(cx).focused.as_ref().unwrap().textarea.clone().update(cx, |ta, cx| {
            ta.set_selected_range(7..7, cx);
        })
    });
    cx.simulate_input("C\\##^needle");
    wiki_catalog(&editor, &[], cx);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), original, "loading cannot assign an ID");
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let row = cx.debug_bounds("wiki-row-0").unwrap();
    cx.simulate_click(row.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    let completed = text(&editor, cx);
    let span = aero_core::links::scan(&completed).remove(0);
    assert_eq!(span.target, "C#");
    let id = span.fragment.unwrap().strip_prefix('^').unwrap().to_owned();
    let changed = std::fs::read_to_string(&target).unwrap();
    let doc = aero_core::Document::from_text(&changed);
    let block = aero_core::block_refs::resolve(&doc, &id).unwrap();
    assert!(
        doc.blocks()
            .iter()
            .any(|(_, b)| b.id == block && matches!(b.kind, aero_core::BlockKind::CodeFence { .. }))
    );
    cx.simulate_input("!");
    assert_eq!(text(&editor, cx), completed.replace("]] after", "]]! after"));
    cx.simulate_keystrokes("cmd-z");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "body [[C\\##^needle]] after\n");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), changed);
    cx.simulate_keystrokes("cmd-shift-z");
    assert_eq!(text(&editor, cx), completed);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), changed);
}

#[gpui_kit::test]
fn block_ref_external_stale_file_and_removed_file_never_overwrite(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "body\n");
    let target = dir.path().join("b.md");
    std::fs::write(&target, "needle\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input(" [[b#^needle");
    wiki_catalog(&editor, &[], cx);
    std::fs::write(&target, "external edit\n").unwrap();
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "body [[b#^needle]]\n");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "external edit\n");
    cx.update(|_, cx| {
        assert!(editor.read(cx).wiki.as_ref().unwrap().error.as_deref().unwrap().contains("变化"))
    });
    std::fs::remove_file(&target).unwrap();
    cx.simulate_keystrokes("tab");
    assert_eq!(text(&editor, cx), "body [[b#^needle]]\n");
    assert!(!target.exists());
}

#[gpui_kit::test]
fn block_ref_reuses_existing_id_and_handles_ime_alias_in_table(cx: &mut TestAppContext) {
    use gpui_kit::EntityInputHandler;
    let (_dir, _vault, _path, editor) = open(cx, "| A |\n| --- |\n| 内容 |\n\n中文😀 ^stable-id\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    let input = cx.update(|_, cx| editor.read(cx).focused.as_ref().unwrap().textarea.clone());
    cx.update(|_, cx| {
        input.update(cx, |ta, cx| {
            let at = ta.value().find("内容").unwrap() + "内容".len();
            ta.set_selected_range(at..at, cx);
        })
    });
    cx.simulate_input(" [[#^");
    let before = text(&editor, cx);
    cx.update(|window, cx| {
        input.update(cx, |ta, cx| ta.replace_and_mark_text_in_range(None, "zhong", None, window, cx))
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| e.on_split_block(&crate::actions::SplitBlock, window, cx))
    });
    assert_eq!(text(&editor, cx), before);
    cx.update(|window, cx| input.update(cx, |ta, cx| ta.replace_text_in_range(None, "中文", window, cx)));
    cx.run_until_parked();
    cx.simulate_keystrokes("shift-enter");
    assert_eq!(text(&editor, cx), "| A |\n| --- |\n| 内容 [[#^stable-id\\|中文😀]] |\n\n中文😀 ^stable-id\n");
    cx.simulate_input("别名");
    assert!(text(&editor, cx).contains("[[#^stable-id\\|别名]]"));
    cx.update(|_, cx| assert_eq!(editor.read(cx).units.len(), 2));
}

#[gpui_kit::test]
fn block_ref_mode_transition_empty_and_duplicate_ids_are_safe(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "body\n");
    let target = dir.path().join("b.md");
    let original = "needle ^dup\n\nother ^dup\n";
    std::fs::write(&target, original).unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input(" [[b#");
    let old = cx.update(|_, cx| editor.read(cx).wiki.as_ref().unwrap().generation);
    cx.simulate_input("^");
    cx.update(|_, cx| {
        editor.update(cx, |e, cx| {
            assert_ne!(old, e.wiki.as_ref().unwrap().generation);
            let task = cx.background_spawn(async { Ok(vec![]) });
            e.load_wiki_notes(old, task, vec![], cx);
        })
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "body [[b#^]]\n");
    wiki_catalog(&editor, &[], cx);
    cx.simulate_input("needle");
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "body [[b#^needle]]\n");
    cx.update(|_, cx| {
        assert!(editor.read(cx).wiki.as_ref().unwrap().error.as_deref().unwrap().contains("重复"))
    });
    cx.simulate_input("missing");
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "body [[b#^needlemissing]]\n");
    cx.simulate_keystrokes("escape");
    cx.update(|_, cx| assert!(editor.read(cx).wiki.is_none()));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), original);
}

#[gpui_kit::test]
fn block_ref_reveal_expands_callout_and_ignores_missing_or_ambiguous_id(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) =
        open(cx, "> [!note]- 折叠\n> 内文 ^inside\n\nfirst ^dup\n\nsecond ^dup\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("callout-body-折叠").is_none());
    cx.update(|_, cx| {
        editor.update(cx, |e, cx| {
            e.reveal_fragment("^inside", cx).unwrap();
            assert_eq!(e.highlight, Some(e.units[0].id));
            assert!(e.reveal_fragment("^missing", cx).is_err());
            assert!(e.reveal_fragment("^dup", cx).is_err());
            assert_eq!(e.highlight, Some(e.units[0].id));
        })
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("callout-body-折叠").is_some());
}

#[gpui_kit::test]
fn heading_completion_uses_unsaved_current_note_and_undo(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "# 原标题\n\n正文\n\n## 第二节\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input("未保存");
    focus_unit(&editor, 1, cx);
    cx.simulate_input(" [[#");
    cx.update(|_, cx| {
        let e = editor.read(cx);
        let m = e.wiki.as_ref().unwrap();
        assert_eq!(m.headings.as_ref().unwrap().entries[0].text, "原标题未保存");
        assert!(e.tag_menu.is_none());
    });
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("up");
    cx.simulate_keystrokes("tab");
    assert_eq!(text(&editor, cx), "# 原标题未保存\n\n正文 [[#原标题未保存]]\n\n## 第二节\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "# 原标题未保存\n\n正文 [[#]]\n\n## 第二节\n");
    cx.simulate_keystrokes("cmd-shift-z");
    cx.simulate_input("!");
    assert!(text(&editor, cx).contains("[[#原标题未保存]]!"));
    cx.update(|_, cx| {
        editor.update(cx, |e, cx| {
            assert!(e.reveal_heading("原标题未保存", cx));
            assert_eq!(e.highlight, Some(e.headings()[0].2));
        });
    });
}

#[gpui_kit::test]
fn heading_completion_mouse_resolves_title_and_reuses_closing_brackets(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "正文 [[知识#]] 后\n");
    std::fs::write(dir.path().join("C#.md"), "---\ntitle: 知识\n---\n\n# Intro\n\n## **中文** C#\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.update(|_, cx| {
        editor.read(cx).focused.as_ref().unwrap().textarea.clone().update(cx, |ta, cx| {
            let at = "正文 [[知识#".len();
            ta.set_selected_range(at..at, cx);
        });
    });
    cx.simulate_input("文c");
    wiki_catalog(&editor, &[], cx);
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let row = cx.debug_bounds("wiki-row-0").unwrap();
    cx.simulate_click(row.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "正文 [[C\\##中文 C\\#]] 后\n");
    cx.simulate_input("!");
    assert_eq!(text(&editor, cx), "正文 [[C\\##中文 C\\#]]! 后\n");
    let span = aero_core::links::scan(&text(&editor, cx)).remove(0);
    assert_eq!(span.target, "C#");
    assert_eq!(span.fragment.as_deref(), Some("中文 C#"));
}

#[gpui_kit::test]
fn heading_completion_mode_change_rejects_old_reply_and_blocks_empty_results(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "body\n");
    std::fs::write(dir.path().join("b.md"), "# Target\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input(" [[b");
    let old = cx.update(|_, cx| editor.read(cx).wiki.as_ref().unwrap().generation);
    cx.simulate_input("#");
    cx.update(|_, cx| {
        editor.update(cx, |e, cx| {
            assert_ne!(e.wiki.as_ref().unwrap().generation, old);
            let notes = cx.background_spawn(async { Ok(vec![]) });
            e.load_wiki_notes(old, notes, vec![], cx);
        })
    });
    cx.run_until_parked();
    cx.update(|_, cx| assert!(editor.read(cx).wiki.as_ref().unwrap().catalog.is_none()));
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "body [[b#]]\n");
    wiki_catalog(&editor, &[], cx);
    cx.simulate_input("不存在");
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "body [[b#不存在]]\n");
    cx.simulate_keystrokes("escape");
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert!(e.wiki.is_none());
        let input = e.focused.as_ref().unwrap().textarea.clone();
        input.update(cx, |ta, cx| ta.set_selected_range(0..ta.value().len(), cx));
    });
    cx.simulate_input("body [[missing#");
    wiki_catalog(&editor, &[], cx);
    cx.simulate_keystrokes("tab");
    assert_eq!(text(&editor, cx), "body [[missing#]]\n");
    cx.update(|_, cx| {
        assert!(editor.read(cx).wiki.as_ref().unwrap().error.is_some());
    });
    assert!(!dir.path().join("missing.md").exists());
}

#[gpui_kit::test]
fn heading_completion_rejects_ambiguous_or_removed_notes(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "body\n");
    for folder in ["x", "y"] {
        std::fs::create_dir(dir.path().join(folder)).unwrap();
        std::fs::write(dir.path().join(folder).join("b.md"), "# Target\n").unwrap();
    }
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input(" [[b#");
    wiki_catalog(&editor, &[], cx);
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "body [[b#]]\n");
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert!(e.wiki.as_ref().unwrap().error.as_deref().unwrap().contains("歧义"));
        e.focused
            .as_ref()
            .unwrap()
            .textarea
            .clone()
            .update(cx, |ta, cx| ta.set_selected_range(0..ta.value().len(), cx));
    });
    cx.simulate_input("body [[y/b#");
    wiki_catalog(&editor, &[], cx);
    std::fs::remove_file(dir.path().join("y/b.md")).unwrap();
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "body [[y/b#]]\n");
    cx.update(|_, cx| {
        assert!(editor.read(cx).wiki.as_ref().unwrap().error.as_deref().unwrap().contains("删除"))
    });
}

#[gpui_kit::test]
fn heading_completion_ime_alias_and_table_structure(cx: &mut TestAppContext) {
    use gpui_kit::EntityInputHandler;
    let (_dir, _vault, _path, editor) = open(cx, "# 中文😀\n\n| A |\n| --- |\n| 内容 |\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 1, cx);
    let input = cx.update(|_, cx| editor.read(cx).focused.as_ref().unwrap().textarea.clone());
    cx.update(|_, cx| {
        input.update(cx, |ta, cx| {
            let at = ta.value().find("内容").unwrap() + "内容".len();
            ta.set_selected_range(at..at, cx);
        })
    });
    cx.simulate_input(" [[#");
    let before = text(&editor, cx);
    cx.update(|window, cx| {
        input.update(cx, |ta, cx| ta.replace_and_mark_text_in_range(None, "zhong", None, window, cx))
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| e.on_split_block(&crate::actions::SplitBlock, window, cx))
    });
    assert_eq!(text(&editor, cx), before);
    cx.update(|window, cx| input.update(cx, |ta, cx| ta.replace_text_in_range(None, "中文", window, cx)));
    cx.run_until_parked();
    cx.simulate_keystrokes("shift-enter");
    assert_eq!(text(&editor, cx), "# 中文😀\n\n| A |\n| --- |\n| 内容 [[#中文😀\\|中文😀]] |\n");
    cx.simulate_input("显示文字");
    assert!(text(&editor, cx).contains("[[#中文😀\\|显示文字]]"));
    cx.update(|_, cx| assert_eq!(editor.read(cx).units.len(), 2));
}

#[gpui_kit::test]
fn heading_reveal_expands_callout_and_matches_setext(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) =
        open(cx, "标题\n----\n\n> [!note]- 折叠\n> ## **内部** 标题\n>\n> 正文\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("callout-body-折叠").is_none());
    cx.update(|_, cx| {
        editor.update(cx, |e, cx| {
            assert!(e.reveal_heading("标题", cx));
            assert_eq!(e.highlight, Some(e.units[0].id));
            assert!(e.reveal_heading("内部 标题", cx));
            assert_eq!(e.highlight, Some(e.units[1].id));
        })
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("callout-body-折叠").is_some());
}

#[gpui_kit::test]
fn wiki_keyboard_completion_alias_and_undo(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "正文\n");
    std::fs::write(dir.path().join("b.md"), "---\ntitle: 知识库😀\n---\n").unwrap();
    std::fs::write(dir.path().join("z.md"), "Z\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input(" [[");
    wiki_catalog(&editor, &["z.md", "b.md"], cx);
    cx.update(|_, cx| {
        let e = editor.read(cx);
        let m = e.wiki.as_ref().unwrap();
        assert_eq!(m.catalog.as_ref().unwrap().notes[m.matches[0]].0.as_str(), "z.md");
        assert!(e.slash.is_none());
    });
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "正文 [[b]]\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "正文 [[]]\n");
    cx.simulate_keystrokes("cmd-shift-z");
    assert_eq!(text(&editor, cx), "正文 [[b]]\n");
    cx.simulate_input(" [[知");
    wiki_catalog(&editor, &[], cx);
    cx.simulate_input("识");
    cx.update(|_, cx| {
        let e = editor.read(cx);
        let m = e.wiki.as_ref().unwrap();
        assert!(m.catalog.is_some(), "typing filters the loaded catalog without reloading");
        assert_eq!(m.trigger.query, "知识");
        assert_eq!(m.matches.len(), 1);
    });
    cx.simulate_keystrokes("shift-enter");
    assert_eq!(text(&editor, cx), "正文 [[b]] [[b|知识库😀]]\n");
    cx.simulate_input("自定义");
    assert_eq!(text(&editor, cx), "正文 [[b]] [[b|自定义]]\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "正文 [[b]] [[b|知识库😀]]\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "正文 [[b]] [[知识]]\n");
}

#[gpui_kit::test]
fn wiki_mouse_completion_reuses_closing_brackets_and_preserves_caret(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "正文 [[]] 后\n");
    std::fs::create_dir(dir.path().join("x")).unwrap();
    std::fs::create_dir(dir.path().join("y")).unwrap();
    std::fs::write(dir.path().join("x/同名.md"), "X\n").unwrap();
    std::fs::write(dir.path().join("y/同名.md"), "Y\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.update(|_, cx| {
        editor.read(cx).focused.as_ref().unwrap().textarea.clone().update(cx, |ta, cx| {
            let at = "正文 [[".len();
            ta.set_selected_range(at..at, cx);
        });
    });
    cx.simulate_input("y/同");
    wiki_catalog(&editor, &[], cx);
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let row = cx.debug_bounds("wiki-row-0").unwrap();
    cx.simulate_click(row.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "正文 [[y/同名]] 后\n");
    cx.simulate_input("!");
    assert_eq!(text(&editor, cx), "正文 [[y/同名]]! 后\n");
}

#[gpui_kit::test]
fn wiki_loading_failure_escape_and_stale_catalog(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "body\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input(" [[missing");
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "body [[missing]]\n", "loading is not an empty result");
    let old = cx.update(|_, cx| editor.read(cx).wiki.as_ref().unwrap().generation);
    cx.simulate_keystrokes("escape");
    cx.update(|_, cx| assert!(editor.read(cx).focused.is_some()));
    cx.simulate_input("2");
    cx.update(|_, cx| {
        editor.update(cx, |e, cx| {
            let task = cx.background_spawn(async { Ok(vec![]) });
            e.load_wiki_notes(old, task, vec![], cx);
        })
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
        editor.update(cx, |e, cx| {
            let m = e.wiki.as_ref().unwrap();
            assert!(m.catalog.is_none(), "old reply must not replace the new menu");
            let generation = m.generation;
            let task = cx.background_spawn(async { Err("test failure".into()) });
            e.load_wiki_notes(generation, task, vec![], cx);
        })
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "body [[missing2]]\n");
    wiki_catalog(&editor, &[], cx);
    cx.simulate_keystrokes("tab");
    assert_eq!(text(&editor, cx), "body [[missing2]]\n");
    cx.update(|_, cx| assert!(!editor.read(cx).vault.exists(&VaultPath::new("missing2.md").unwrap())));
}

#[gpui_kit::test]
fn wiki_cursor_selection_and_removed_target_do_not_use_stale_menu(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "body\n");
    std::fs::write(dir.path().join("b.md"), "B\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input(" [[b");
    wiki_catalog(&editor, &[], cx);
    std::fs::remove_file(dir.path().join("b.md")).unwrap();
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "body [[b]]\n");
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert!(e.wiki.as_ref().unwrap().error.is_some());
        e.focused.as_ref().unwrap().textarea.clone().update(cx, |ta, cx| ta.set_selected_range(0..4, cx));
    });
    cx.run_until_parked();
    cx.update(|_, cx| assert!(editor.read(cx).wiki.is_none()));
    cx.simulate_input("new");
    assert_eq!(text(&editor, cx), "new [[b]]\n");
}

#[gpui_kit::test]
fn wiki_ime_preedit_does_not_confirm_or_write_source(cx: &mut TestAppContext) {
    use gpui_kit::EntityInputHandler;
    let (dir, _vault, _path, editor) = open(cx, "正文\n");
    std::fs::write(dir.path().join("中文.md"), "Z\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input(" [[");
    wiki_catalog(&editor, &["中文.md"], cx);
    let input = cx.update(|_, cx| editor.read(cx).focused.as_ref().unwrap().textarea.clone());
    cx.update(|window, cx| {
        input.update(cx, |ta, cx| ta.replace_and_mark_text_in_range(None, "zhong", None, window, cx))
    });
    cx.run_until_parked();
    // 平台一般将候选键交给 IME；即使动作冒泡也不能确认旧补全。
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| e.on_split_block(&crate::actions::SplitBlock, window, cx))
    });
    assert_eq!(text(&editor, cx), "正文 [[]]\n");
    cx.update(|window, cx| input.update(cx, |ta, cx| ta.replace_text_in_range(None, "中文", window, cx)));
    cx.run_until_parked();
    wiki_catalog(&editor, &[], cx);
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "正文 [[中文]]\n");
}

#[gpui_kit::test]
fn wiki_completion_in_callouts_and_tables_preserves_markdown(cx: &mut TestAppContext) {
    let source = "> [!note]\n> 正文\n\n| A |\n| --- |\n| 内容 |\n\n```md\n代码\n```\n\n$$\n公式\n$$\n";
    let (dir, _vault, _path, editor) = open(cx, source);
    std::fs::write(dir.path().join("b.md"), "---\ntitle: 知识\n---\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    for (ix, needle) in [(0, "正文"), (1, "内容"), (2, "代码"), (3, "公式")] {
        focus_unit(&editor, ix, cx);
        cx.update(|_, cx| {
            editor.read(cx).focused.as_ref().unwrap().textarea.clone().update(cx, |ta, cx| {
                let at = ta.value().find(needle).unwrap() + needle.len();
                ta.set_selected_range(at..at, cx);
            })
        });
        cx.simulate_input(" [[知识");
        if ix < 2 {
            wiki_catalog(&editor, &[], cx);
            cx.simulate_keystrokes("shift-enter");
            cx.simulate_input("别名");
        } else {
            cx.update(|_, cx| assert!(editor.read(cx).wiki.is_none()));
        }
    }
    assert_eq!(
        text(&editor, cx),
        source
            .replace("正文", "正文 [[b|别名]]")
            .replace("内容", "内容 [[b\\|别名]]")
            .replace("代码", "代码 [[知识")
            .replace("公式", "公式 [[知识")
    );
    cx.update(|_, cx| assert_eq!(editor.read(cx).units.len(), 4, "alias must not split a table cell"));
}

fn tag_catalog(editor: &Entity<Editor>, entries: &[(&str, u64)], cx: &mut VisualTestContext) {
    let candidates = entries
        .iter()
        .map(|(tag, count)| aero_index::TagCount { tag: (*tag).into(), note_count: *count })
        .collect();
    cx.update(|_, cx| {
        editor.update(cx, |e, cx| {
            let generation = e.tag_menu.as_ref().expect("tag menu").generation;
            let task = cx.background_spawn(async move { Ok(candidates) });
            e.load_tags(generation, task, cx);
        })
    });
    cx.run_until_parked();
}

#[gpui_kit::test]
fn tag_completion_keyboard_frequency_filter_and_undo(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "body\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input(" #");
    tag_catalog(&editor, &[("rust", 2), ("中文", 5)], cx);
    cx.update(|_, cx| {
        let m = editor.read(cx).tag_menu.as_ref().unwrap();
        assert_eq!(m.candidates.as_ref().unwrap()[m.matches[0]].tag, "中文");
    });
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("tab");
    assert_eq!(text(&editor, cx), "body #rust\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "body #\n");
    cx.simulate_keystrokes("cmd-shift-z");
    assert_eq!(text(&editor, cx), "body #rust\n");
    cx.simulate_input(" #R");
    tag_catalog(&editor, &[("rust", 2), ("rust/cli", 1), ("other", 8)], cx);
    cx.simulate_input("u");
    cx.update(|_, cx| {
        let e = editor.read(cx);
        let menu = e.tag_menu.as_ref().unwrap();
        assert!(menu.candidates.is_some(), "typing must filter without reloading");
        assert_eq!(menu.matches.len(), 2);
        assert!(e.slash.is_none() && e.wiki.is_none());
    });
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "body #rust #rust/cli\n");
    cx.simulate_input(" next");
    assert_eq!(text(&editor, cx), "body #rust #rust/cli next\n");
}

#[gpui_kit::test]
fn tag_completion_mouse_replaces_suffix_and_preserves_punctuation(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "body #st，tail\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.update(|_, cx| {
        editor
            .read(cx)
            .focused
            .as_ref()
            .unwrap()
            .textarea
            .clone()
            .update(cx, |ta, cx| ta.set_selected_range(6..6, cx))
    });
    cx.simulate_input("ru");
    tag_catalog(&editor, &[("rust/cli", 1)], cx);
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let row = cx.debug_bounds("tag-row-0").unwrap();
    cx.simulate_click(row.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "body #rust/cli，tail\n");
    cx.simulate_input(" next");
    assert_eq!(text(&editor, cx), "body #rust/cli next，tail\n");
}

#[gpui_kit::test]
fn tag_completion_loading_failure_empty_and_stale_reply(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "body\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input(" #r");
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "body #r\n");
    let old = cx.update(|_, cx| editor.read(cx).tag_menu.as_ref().unwrap().generation);
    cx.simulate_keystrokes("escape");
    cx.update(|_, cx| assert!(editor.read(cx).focused.is_some()));
    cx.simulate_input("u");
    cx.update(|_, cx| {
        editor.update(cx, |e, cx| {
            let task = cx.background_spawn(async { Ok(vec![]) });
            e.load_tags(old, task, cx);
        })
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
        editor.update(cx, |e, cx| {
            let menu = e.tag_menu.as_ref().unwrap();
            assert!(menu.candidates.is_none());
            let generation = menu.generation;
            let task = cx.background_spawn(async { Err("test".into()) });
            e.load_tags(generation, task, cx);
        })
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "body #ru\n");
    tag_catalog(&editor, &[], cx);
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "body #ru\n\n\n");
    cx.update(|_, cx| assert!(editor.read(cx).tag_menu.is_none()));
}

#[gpui_kit::test]
fn tag_completion_ime_and_selection_do_not_confirm_stale_menu(cx: &mut TestAppContext) {
    use gpui_kit::EntityInputHandler;
    let (_dir, _vault, _path, editor) = open(cx, "正文\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input(" #");
    tag_catalog(&editor, &[("中文/工具", 1)], cx);
    let input = cx.update(|_, cx| editor.read(cx).focused.as_ref().unwrap().textarea.clone());
    cx.update(|window, cx| {
        input.update(cx, |ta, cx| ta.replace_and_mark_text_in_range(None, "zhong", None, window, cx))
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| e.on_split_block(&crate::actions::SplitBlock, window, cx))
    });
    assert_eq!(text(&editor, cx), "正文 #\n");
    cx.update(|window, cx| input.update(cx, |ta, cx| ta.replace_text_in_range(None, "中文", window, cx)));
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "正文 #中文/工具\n");
    cx.simulate_input(" #");
    tag_catalog(&editor, &[("rust", 1)], cx);
    cx.update(|_, cx| {
        editor
            .read(cx)
            .focused
            .as_ref()
            .unwrap()
            .textarea
            .clone()
            .update(cx, |ta, cx| ta.set_selected_range(0..6, cx))
    });
    cx.run_until_parked();
    cx.update(|_, cx| assert!(editor.read(cx).tag_menu.is_none()));
    cx.simulate_input("新正文");
    assert_eq!(text(&editor, cx), "新正文 #中文/工具 #\n");
}

#[gpui_kit::test]
fn tag_completion_respects_heading_and_works_in_callout_and_table(cx: &mut TestAppContext) {
    let source = "# Heading\n\n> [!note]\n> 引用\n\n| A |\n| --- |\n| 单元 |\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    for (ix, needle) in [(0, "Heading"), (1, "引用"), (2, "单元")] {
        focus_unit(&editor, ix, cx);
        cx.update(|_, cx| {
            editor.read(cx).focused.as_ref().unwrap().textarea.clone().update(cx, |ta, cx| {
                let at = ta.value().find(needle).unwrap() + needle.len();
                ta.set_selected_range(at..at, cx);
            })
        });
        cx.simulate_input(" #ru");
        if ix == 0 {
            cx.update(|_, cx| assert!(editor.read(cx).tag_menu.is_none()));
        } else {
            tag_catalog(&editor, &[("rust", 1)], cx);
            cx.simulate_keystrokes("enter");
        }
    }
    assert_eq!(
        text(&editor, cx),
        source.replace("Heading", "Heading #ru").replace("引用", "引用 #rust").replace("单元", "单元 #rust")
    );
}

#[gpui_kit::test]
fn command_availability_excludes_noop_and_unsupported_conversions(cx: &mut TestAppContext) {
    let source = "# Title\n\n> quote\n\n| A | B |\n| --- | --- |\n| x | y |\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.update(|_, cx| {
        let editor = editor.read(cx);
        assert!(!editor.command_available(&crate::actions::ConvertBlock(1)));
        assert!(editor.command_available(&crate::actions::ConvertBlock(2)));
        assert!(!editor.command_available(&crate::actions::DocUndo));
    });
    for ix in [1, 2] {
        focus_unit(&editor, ix, cx);
        cx.update(|_, cx| assert!(!editor.read(cx).command_available(&crate::actions::ConvertBlock(2))));
    }
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

fn conflict_copies(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("a.conflict-") && n.ends_with(".md"))
        .collect();
    v.sort();
    v
}

#[gpui_kit::test]
fn repeated_conflicts_keep_every_disk_version(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "base\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let first = editor.update(cx, |e, _| e.backup_disk(b"first").unwrap());
    let second = editor.update(cx, |e, _| e.backup_disk(b"second").unwrap());
    assert_ne!(first, second);
    assert_eq!(std::fs::read(dir.path().join(first.as_str())).unwrap(), b"first");
    assert_eq!(std::fs::read(dir.path().join(second.as_str())).unwrap(), b"second");
}

#[gpui_kit::test]
fn unreadable_disk_state_stops_saving(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "base\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input("!");
    let file = dir.path().join("a.md");
    std::fs::rename(&file, dir.path().join("original.md")).unwrap();
    // 目录不能作为笔记读取：这不是“未修改”或“已删除”。
    std::fs::create_dir(&file).unwrap();
    assert!(editor.update(cx, |e, _| e.disk_state()).is_err());
    editor.update(cx, |e, cx| e.save_now(cx));
    cx.run_until_parked();
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert!(e.dirty);
        assert!(matches!(e.save_state(), super::SaveState::Failed(_)));
        assert!(!e.is_deleted());
    });
    assert!(editor.update(cx, |e, _| e.flush_sync()).is_err());
    assert!(file.is_dir());
    assert_eq!(text(&editor, cx), "base!\n");
}

/// 通过真实控件的 IME 协议验证：预编辑不保存，提交后才进入 Document / undo。
#[gpui_kit::test]
fn ime_preedit_commit_and_undo_preserve_markdown(cx: &mut TestAppContext) {
    use gpui_kit::EntityInputHandler;
    let source = "前😀 **ab** 后\n";
    let (dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    for (preedit, committed) in [("nihao", "你好"), ("にほんご", "日本語")] {
        focus_unit(&editor, 0, cx);
        let input = cx.update(|_, cx| editor.read(cx).focused.as_ref().unwrap().textarea.clone());
        cx.update(|window, cx| {
            input.update(cx, |s, cx| {
                let at = "前😀 **".len();
                s.set_selected_range(at..at, cx);
                s.replace_and_mark_text_in_range(None, preedit, None, window, cx);
            })
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(text(&editor, cx), source, "preedit must not become a document edit");
        assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), source);
        cx.update(|window, cx| {
            input.update(cx, |s, cx| {
                s.replace_text_in_range(None, committed, window, cx);
            })
        });
        cx.run_until_parked();
        assert_eq!(text(&editor, cx), format!("前😀 **{committed}ab** 后\n"));
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(text(&editor, cx), source, "one undo removes the complete composition");
    }
}

#[gpui_kit::test]
fn ime_cancel_does_not_change_document_or_disk(cx: &mut TestAppContext) {
    use gpui_kit::EntityInputHandler;
    let source = "[[目标]]\n";
    let (dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    let input = cx.update(|_, cx| editor.read(cx).focused.as_ref().unwrap().textarea.clone());
    cx.update(|window, cx| {
        input.update(cx, |s, cx| {
            s.set_selected_range(2..2, cx);
            s.replace_and_mark_text_in_range(None, "quxiao", None, window, cx);
        })
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        input.update(cx, |s, cx| {
            s.replace_and_mark_text_in_range(None, "", None, window, cx);
            s.unmark_text(window, cx);
        })
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(3));
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), source);
    assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), source);
    cx.update(|_, cx| assert!(!editor.read(cx).doc.can_undo()));
}

/// 原笔记可写，但冲突副本名称超过文件系统限制：备份失败绝不能继续覆盖原文。
#[gpui_kit::test]
fn failed_conflict_backup_preserves_both_versions_and_can_retry(cx: &mut TestAppContext) {
    use super::{EditorEvent, SaveState};
    let (dir, vault, path, editor) = open(cx, "base\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let long_path = VaultPath::new(&format!("{}.md", "a".repeat(190))).unwrap();
    vault.rename(&path, &long_path).unwrap();
    editor.update(cx, |e, cx| e.set_path(long_path.clone(), cx));
    focus_unit(&editor, 0, cx);
    cx.simulate_input("!");
    let file = vault.fs_path(&long_path);
    std::fs::write(&file, "theirs from disk\n").unwrap();
    let failures = std::rc::Rc::new(std::cell::Cell::new(0));
    let sink = failures.clone();
    cx.update(|_, cx| {
        cx.subscribe(&editor, move |_, event: &EditorEvent, _| {
            if matches!(event, EditorEvent::SaveFailed(_)) {
                sink.set(sink.get() + 1);
            }
        })
        .detach();
    });
    let original_stamp = cx.update(|_, cx| editor.read(cx).stamp);
    // 自动保存与监听触发保存都必须停止；失败不能更新已知磁盘指纹。
    cx.executor().advance_clock(Duration::from_millis(600));
    cx.run_until_parked();
    editor.update(cx, |e, cx| e.check_external(cx));
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs from disk\n");
    assert_eq!(text(&editor, cx), "base!\n");
    assert!(failures.get() > 0);
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert!(e.dirty);
        assert!(matches!(e.save_state(), SaveState::Failed(_)));
        assert_eq!(e.stamp, original_stamp);
        assert!(e.banner().is_none());
    });
    assert!(editor.update(cx, |e, _| e.flush_sync()).is_err());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs from disk\n");
    // 去掉失败原因后重试，仍需先备份磁盘版本。
    vault.rename(&long_path, &path).unwrap();
    editor.update(cx, |e, cx| {
        e.set_path(path, cx);
        e.save_now(cx);
    });
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), "base!\n");
    let copies = conflict_copies(dir.path());
    assert_eq!(copies.len(), 1);
    assert_eq!(std::fs::read_to_string(dir.path().join(&copies[0])).unwrap(), "theirs from disk\n");
}

/// 10.2 的横幅：重载 → 蓝色并 10 s 自动消失；冲突 → 黄色，"使用磁盘版本"把副本换进来并删掉副本。
#[gpui_kit::test]
fn banners_for_reload_and_conflict(cx: &mut TestAppContext) {
    use super::{Banner, EditorEvent};
    let (dir, _vault, _path, editor) = open(cx, "base\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let file = dir.path().join("a.md");
    let touched: std::rc::Rc<std::cell::RefCell<Vec<Vec<String>>>> = Default::default();
    let sink = touched.clone();
    cx.update(|_, cx| {
        cx.subscribe(&editor, move |_, ev: &EditorEvent, _| {
            if let EditorEvent::PathsTouched(paths) = ev {
                sink.borrow_mut().push(paths.iter().map(|p| p.as_str().to_owned()).collect());
            }
        })
        .detach();
    });

    // 干净时外部修改：重载 + 蓝横幅，10 s 后消失
    let past = std::time::SystemTime::now() - Duration::from_secs(5);
    std::fs::write(&file, "theirs\n").unwrap();
    std::fs::File::open(&file).unwrap().set_modified(past).unwrap();
    editor.update(cx, |e, cx| e.check_external(cx));
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "theirs\n");
    cx.update(|_, cx| assert_eq!(editor.read(cx).banner(), Some(&Banner::Reloaded)));
    cx.executor().advance_clock(Duration::from_secs(11));
    cx.run_until_parked();
    cx.update(|_, cx| assert_eq!(editor.read(cx).banner(), None));

    // 有未保存修改时外部修改：冲突副本 + 黄横幅
    focus_unit(&editor, 0, cx);
    cx.simulate_input("!");
    let past = std::time::SystemTime::now() - Duration::from_secs(10);
    std::fs::write(&file, "disk wins\n").unwrap();
    std::fs::File::open(&file).unwrap().set_modified(past).unwrap();
    editor.update(cx, |e, cx| e.check_external(cx));
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs!\n");
    let copies = conflict_copies(dir.path());
    assert_eq!(copies.len(), 1);
    let copy = VaultPath::new(&copies[0]).unwrap();
    cx.update(|_, cx| assert_eq!(editor.read(cx).banner(), Some(&Banner::Conflict { copy: copy.clone() })));
    assert_eq!(touched.borrow().as_slice(), [vec![copies[0].clone()]]);

    // 使用磁盘版本：内容换成副本的、落盘、副本删掉
    editor.update(cx, |e, cx| e.take_disk_version(cx));
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "disk wins\n");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "disk wins\n");
    assert!(conflict_copies(dir.path()).is_empty());
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert_eq!(e.banner(), None);
        assert!(matches!(e.save_state(), super::SaveState::Saved(_)));
    });
    assert_eq!(touched.borrow().len(), 2);
    assert_eq!(touched.borrow()[1], vec![copies[0].clone()]);
}

/// 文件在外部被删：红横幅、自动保存停止（不把文件变回来）；"恢复"写回；"关闭"发事件。
#[gpui_kit::test]
fn external_deletion_stops_autosave_until_restored(cx: &mut TestAppContext) {
    use super::{Banner, EditorEvent};
    let (dir, _vault, _path, editor) = open(cx, "base\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let file = dir.path().join("a.md");
    let events: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>> = Default::default();
    let sink = events.clone();
    cx.update(|_, cx| {
        cx.subscribe(&editor, move |_, ev: &EditorEvent, _| match ev {
            EditorEvent::Deleted => sink.borrow_mut().push("deleted"),
            EditorEvent::CloseRequested => sink.borrow_mut().push("close"),
            EditorEvent::Saved => sink.borrow_mut().push("saved"),
            _ => {}
        })
        .detach();
    });

    focus_unit(&editor, 0, cx);
    cx.simulate_input("!");
    std::fs::remove_file(&file).unwrap();
    // 2 s 轮询发现文件没了；500 ms 的自动保存不该把它写回来
    cx.executor().advance_clock(Duration::from_millis(2100));
    cx.run_until_parked();
    assert!(!file.exists(), "autosave must not resurrect a deleted file");
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert_eq!(e.banner(), Some(&Banner::Deleted));
        assert!(e.is_deleted());
    });
    assert_eq!(events.borrow().as_slice(), ["deleted"]);
    assert_eq!(text(&editor, cx), "base!\n");

    // 继续编辑也不保存
    cx.simulate_input("?");
    cx.executor().advance_clock(Duration::from_millis(3000));
    cx.run_until_parked();
    assert!(!file.exists());
    assert_eq!(text(&editor, cx), "base!?\n");

    // 恢复：写回原路径，横幅消失，之后照常自动保存
    editor.update(cx, |e, cx| e.restore_deleted(cx));
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "base!?\n");
    cx.update(|_, cx| assert_eq!(editor.read(cx).banner(), None));
    assert_eq!(events.borrow().as_slice(), ["deleted", "saved"]);

    // 再删一次，这回"关闭"
    std::fs::remove_file(&file).unwrap();
    editor.update(cx, |e, cx| e.check_external(cx));
    cx.run_until_parked();
    cx.update(|_, cx| assert!(editor.read(cx).is_deleted()));
    // flush_sync 也不写回
    assert_eq!(editor.update(cx, |e, _| e.flush_sync()).unwrap(), None);
    assert!(!file.exists());
    editor.update(cx, |_, cx| cx.emit(EditorEvent::CloseRequested));
    cx.run_until_parked();
    assert_eq!(events.borrow().as_slice(), ["deleted", "saved", "deleted", "close"]);

    // 另存为：写到新路径并改跟它
    let target = VaultPath::new("sub/restored.md").unwrap();
    editor.update(cx, |e, cx| e.save_as(target.clone(), cx)).unwrap();
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(dir.path().join("sub/restored.md")).unwrap(), "base!?\n");
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert_eq!(e.path(), &target);
        assert_eq!(e.banner(), None);
    });
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

fn selected(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Option<std::ops::Range<usize>> {
    cx.update(|_, cx| editor.read(cx).selected_blocks())
}

/// 块选中态（`ui_design.md` 6）：Esc 两段式退出、↑↓ / ⇧↑↓ 移动与扩展、⏎ 回编辑、⌫ 删除可撤销、⌘C / ⌘X。
#[gpui_kit::test]
fn block_selection_keyboard_flow(cx: &mut TestAppContext) {
    let source = "# Title\n\n- one\n- two\n\nlast\n";
    let (dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let file = dir.path().join("a.md");

    // 编辑态 Esc → 当前块被选中、控件消失、未保存内容落盘；再 Esc → 浏览态
    focus_unit(&editor, 1, cx);
    cx.simulate_input("!");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(selected(&editor, cx), Some(1..2));
    cx.update(|window, cx| {
        let e = editor.read(cx);
        assert!(e.focused.is_none());
        assert!(e.focus_handle.is_focused(window), "container keeps keyboard focus in block selection");
    });
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "# Title\n\n- one!\n- two\n\nlast\n");
    cx.simulate_keystrokes("escape");
    assert_eq!(selected(&editor, cx), None);

    // 浏览态 ↑↓ 不产生选区
    cx.simulate_keystrokes("down");
    assert_eq!(selected(&editor, cx), None);

    // ↓ 移动、⇧↓ 扩展、⇧↑ 收回、到头停住
    focus_unit(&editor, 0, cx);
    cx.simulate_keystrokes("escape");
    assert_eq!(selected(&editor, cx), Some(0..1));
    cx.simulate_keystrokes("down");
    assert_eq!(selected(&editor, cx), Some(1..2));
    cx.simulate_keystrokes("shift-down shift-down");
    assert_eq!(selected(&editor, cx), Some(1..4));
    cx.simulate_keystrokes("shift-down");
    assert_eq!(selected(&editor, cx), Some(1..4), "extending past the last block stays put");
    cx.simulate_keystrokes("shift-up");
    assert_eq!(selected(&editor, cx), Some(1..3));
    cx.simulate_keystrokes("up up up up");
    assert_eq!(selected(&editor, cx), Some(0..1));

    // 选中块画成高亮的同一批 unit 元素
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let title_id = cx.update(|_, cx| editor.read(cx).units[0].id);
    let selector: &'static str = Box::leak(format!("unit-{}", title_id.0).into_boxed_str());
    assert!(cx.debug_bounds(selector).is_some());

    // ⏎：编辑 head 那块，光标在块末
    cx.simulate_keystrokes("down enter");
    assert_eq!(selected(&editor, cx), None);
    cx.update(|_, cx| {
        let e = editor.read(cx);
        let f = e.focused.as_ref().expect("enter re-enters edit mode");
        assert_eq!(f.textarea.read(cx).value(), "one!");
        assert_eq!(f.textarea.read(cx).cursor(), 4);
    });

    // ⌘C 复制选中块的源码（列表项含标记）；⌫ 删除；选区落到下一块；⌘Z 撤销并重新选中恢复的块
    cx.simulate_keystrokes("escape shift-down cmd-c");
    assert_eq!(cx.read_from_clipboard().and_then(|c| c.text()), Some("- one!\n- two".to_owned()));
    cx.simulate_keystrokes("backspace");
    assert_eq!(text(&editor, cx), "# Title\n\nlast\n");
    assert_eq!(selected(&editor, cx), Some(1..2));
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "# Title\n\n- one!\n- two\n\nlast\n");
    assert_eq!(selected(&editor, cx), Some(1..2), "undo re-selects the restored block");

    // ⌘X 剪切最后一块：前面多出的空行一起收掉，选区退到上一块
    cx.simulate_keystrokes("down down cmd-x");
    assert_eq!(cx.read_from_clipboard().and_then(|c| c.text()), Some("last".to_owned()));
    assert_eq!(text(&editor, cx), "# Title\n\n- one!\n- two\n");
    assert_eq!(selected(&editor, cx), Some(2..3));

    // 删到空文档：回浏览态，文件也随之为空
    cx.simulate_keystrokes("shift-up shift-up backspace");
    assert_eq!(text(&editor, cx), "");
    assert_eq!(selected(&editor, cx), None);
    cx.executor().advance_clock(Duration::from_millis(600));
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "");
}

/// 块 gutter（`ui_design.md` 5.3）：每个单位左侧有 `⠿` / `+`，各层级左对齐、不占正文宽度；
/// `+` 在下方新开段落并打开 `/` 菜单，`⠿` 进入块选中态；点按钮不会顺带把块置为编辑态。
#[gpui_kit::test]
fn gutter_inserts_below_and_selects(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "intro\n\n- item\n\nend\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let draw = |cx: &mut VisualTestContext| cx.update(|window, cx| window.draw(cx).clear(cx));
    let selector = |name: &str, id: aero_core::BlockId| -> &'static str {
        Box::leak(format!("{name}-{}", id.0).into_boxed_str())
    };
    let ids: Vec<aero_core::BlockId> =
        cx.update(|_, cx| editor.read(cx).units.iter().map(|u| u.id).collect());
    assert_eq!(ids.len(), 3);
    draw(cx);

    // 布局：gutter 在正文左侧、不同层级左对齐；正文列仍是 720 宽
    let intro = cx.debug_bounds(selector("unit", ids[0])).unwrap();
    let plus_intro = cx.debug_bounds(selector("gutter-plus", ids[0])).unwrap();
    let handle_intro = cx.debug_bounds(selector("gutter-handle", ids[0])).unwrap();
    let handle_item = cx.debug_bounds(selector("gutter-handle", ids[1])).unwrap();
    assert!(
        handle_intro.right() <= plus_intro.origin.x && plus_intro.right() <= intro.origin.x,
        "gutter sits left of the block"
    );
    assert_eq!(handle_item.origin.x, handle_intro.origin.x, "list item gutter aligns with top-level gutter");
    assert_eq!(intro.size.width, gpui_kit::px(720.));

    // `+`：下方多出 "/" 段落，菜单打开，焦点在 "/" 之后
    cx.simulate_click(plus_intro.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "intro\n\n/\n\n- item\n\nend\n");
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert!(e.slash.is_some(), "slash menu opens");
        let f = e.focused.as_ref().expect("new paragraph is being edited");
        assert_eq!(f.textarea.read(cx).value(), "/");
        assert_eq!(f.textarea.read(cx).cursor(), 1);
    });
    // 菜单里选标题 2：`/` 被删掉，新块变成 "## "
    cx.simulate_input("h2");
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "intro\n\n## \n\n- item\n\nend\n");
    cx.simulate_input("New");
    assert_eq!(text(&editor, cx), "intro\n\n## New\n\n- item\n\nend\n");

    // `⠿`：进入块选中态（而不是编辑态）
    cx.simulate_keystrokes("escape escape");
    draw(cx);
    let ids: Vec<aero_core::BlockId> =
        cx.update(|_, cx| editor.read(cx).units.iter().map(|u| u.id).collect());
    let handle_end = cx.debug_bounds(selector("gutter-handle", ids[3])).unwrap();
    cx.simulate_click(handle_end.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    assert_eq!(selected(&editor, cx), Some(3..4));
    cx.update(|_, cx| assert!(editor.read(cx).focused.is_none()));

    // 列表项的 `+`：段落落在项之后
    draw(cx);
    let plus_item = cx.debug_bounds(selector("gutter-plus", ids[2])).unwrap();
    cx.simulate_click(plus_item.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "intro\n\n## New\n\n- item\n\n/\n\nend\n");
    assert_eq!(selected(&editor, cx), None);
    // Esc 关菜单，`/` 留在文档里（与手敲一致）
    cx.simulate_keystrokes("escape");
    cx.update(|_, cx| assert!(editor.read(cx).slash.is_none() && editor.read(cx).focused.is_some()));
    assert_eq!(text(&editor, cx), "intro\n\n## New\n\n- item\n\n/\n\nend\n");
}

/// 编辑态里 ⇧↑ / ⇧↓ 在首 / 末行升级为块选区；不在边上时仍是控件内的字符选区。
#[gpui_kit::test]
fn shift_arrows_upgrade_to_block_selection_at_block_edges(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "first\n\nline a\nline b\n\nthird\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);

    // 多行段落的末行：⇧↑ 只扩展字符选区
    focus_unit(&editor, 1, cx);
    cx.simulate_keystrokes("shift-up");
    assert_eq!(selected(&editor, cx), None);
    cx.update(|_, cx| {
        let e = editor.read(cx);
        let ta = e.focused.as_ref().unwrap().textarea.read(cx);
        assert_ne!(
            ta.selected_range().start,
            ta.selected_range().end,
            "character selection inside the block"
        );
    });
    // 再一次已在首行：升级为块选区（当前块 + 上一块）
    cx.simulate_keystrokes("shift-up");
    assert_eq!(selected(&editor, cx), Some(0..2));
    cx.update(|_, cx| assert!(editor.read(cx).focused.is_none()));

    // 末行 ⇧↓：当前块 + 下一块
    focus_unit(&editor, 1, cx);
    cx.simulate_keystrokes("shift-down");
    assert_eq!(selected(&editor, cx), Some(1..3));
    // 最后一块末行 ⇧↓：只选中自己
    focus_unit(&editor, 2, cx);
    cx.simulate_keystrokes("shift-down");
    assert_eq!(selected(&editor, cx), Some(2..3));
    assert_eq!(text(&editor, cx), "first\n\nline a\nline b\n\nthird\n");
}

/// wikilink 标题片段跳转：滚到对应标题并高亮；找不到的标题什么都不动。
#[gpui_kit::test]
fn reveal_heading_jumps_and_ignores_missing(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) =
        open(cx, "# 甲\n\n段落\n\n## 乙\n\n段落二\n\n## **粗** `码` [[链接|别名]]  Case\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let ids =
        cx.update(|_, cx| editor.read(cx).headings().into_iter().map(|(_, _, id)| id).collect::<Vec<_>>());
    assert_eq!(ids.len(), 3, "语料里要有三个标题");
    assert!(cx.update(|_, cx| editor.update(cx, |e, cx| e.reveal_heading("乙", cx))));
    assert_eq!(cx.update(|_, cx| editor.read(cx).highlighted_block()), Some(ids[1]));
    // 找不到：返回 false，高亮与定位保持不变
    assert!(!cx.update(|_, cx| editor.update(cx, |e, cx| e.reveal_heading("不存在", cx))));
    assert_eq!(cx.update(|_, cx| editor.read(cx).highlighted_block()), Some(ids[1]));
    // 片段写的是渲染后的文字（无行内标记、大小写 / 空白不同）也能命中
    for fragment in ["粗 码 别名 case", "  粗 码 别名 CASE ", "**粗** `码` [[链接|别名]]  Case"]
    {
        assert!(cx.update(|_, cx| editor.update(cx, |e, cx| e.reveal_heading(fragment, cx))), "{fragment:?}");
        assert_eq!(cx.update(|_, cx| editor.read(cx).highlighted_block()), Some(ids[2]));
    }
}
