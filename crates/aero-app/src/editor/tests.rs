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

fn hover_block(id: aero_core::BlockId, cx: &mut VisualTestContext) {
    let selector = Box::leak(format!("block-body-{}", id.0).into_boxed_str());
    let row = cx.debug_bounds(selector).expect("block row");
    cx.simulate_mouse_move(
        row.origin + gpui_kit::point(gpui_kit::px(2.), gpui_kit::px(2.)),
        None,
        Default::default(),
    );
    settle(cx);
}

#[gpui_kit::test]
fn enter_inside_formatted_heading_preserves_identity_and_saves_both_halves(cx: &mut TestAppContext) {
    let id = aero_core::identity::new_block_id();
    let source = format!("## **ab cd** {{.red}} ^{id}\n");
    let (dir, _vault, _path, editor) = open(cx, &source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.update(|_, cx| {
        editor.read(cx).focused.as_ref().unwrap().textarea.clone().update(cx, |ta, cx| {
            let cursor = ta.value().find(" cd").unwrap();
            ta.set_selected_range(cursor..cursor, cx);
        })
    });
    cx.simulate_keystrokes("enter");
    let split = format!("## **ab** {{.red}} ^{id}\n\n**cd** {{.red}}\n");
    assert_eq!(text(&editor, cx), split);
    cx.simulate_input("新");
    assert_eq!(text(&editor, cx), split.replace("**cd**", "**新cd**"));
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), split);
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), source);
    cx.simulate_keystrokes("cmd-shift-z");
    assert_eq!(text(&editor, cx), split);
    cx.executor().advance_clock(Duration::from_millis(600));
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), split);
}

#[gpui_kit::test]
fn different_styles_backspace_shows_a_warning_without_an_undo_entry(cx: &mut TestAppContext) {
    use gpui_kit::component::WindowExt as _;
    let source = "one {.red}\n\ntwo {.blue}\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| {
        window.replace_root(cx, |window, cx| gpui_kit::component::Root::new(editor.clone(), window, cx));
    });
    focus_unit(&editor, 1, cx);
    cx.update(|_, cx| {
        editor.read(cx).focused.as_ref().unwrap().textarea.clone().update(cx, |ta, cx| {
            ta.set_selected_range(0..0, cx);
        })
    });
    cx.simulate_keystrokes("backspace");
    assert_eq!(text(&editor, cx), source);
    cx.update(|window, cx| {
        assert_eq!(window.notifications(cx).len(), 1);
        let e = editor.read(cx);
        assert_eq!(e.focused.as_ref().unwrap().id, Some(e.units[0].id));
    });
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn empty_split_slot_materializes_explicit_style_with_first_input(cx: &mut TestAppContext) {
    use gpui_kit::EntityInputHandler;
    let id = aero_core::identity::new_block_id();
    let alias = aero_core::identity::new_block_id();
    let style = ".red align=center indent=2 custom=value";
    let source = format!("one {{{style} aero-aliases={alias}}} ^{id}\n");
    let (_dir, _vault, _path, editor) = open(cx, &source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_keystrokes("enter");
    let blank = format!("{source}\n\n");
    assert_eq!(text(&editor, cx), blank);
    cx.update(|_, cx| {
        let e = editor.read(cx);
        let f = e.focused.as_ref().unwrap();
        let attrs = f.empty_attrs.as_ref().expect("style belongs to the empty slot");
        assert!(!attrs.pairs.iter().any(|(key, _)| key == "aero-aliases"));
        assert_eq!(aero_core::properties::Alignment::of(attrs), aero_core::properties::Alignment::Center);
    });
    let input = cx.update(|_, cx| editor.read(cx).focused.as_ref().unwrap().textarea.clone());
    cx.update(|window, cx| {
        input.update(cx, |ta, cx| ta.replace_and_mark_text_in_range(None, "xin", None, window, cx))
    });
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), blank, "preedit cannot materialize text or attributes");
    cx.update(|window, cx| input.update(cx, |ta, cx| ta.replace_text_in_range(None, "新", window, cx)));
    cx.run_until_parked();
    let filled = format!("{}\n\n新 {{{style}}}\n", source.trim_end());
    assert_eq!(text(&editor, cx), filled);
    cx.update(|_, cx| {
        assert_eq!(
            editor.read(cx).focused.as_ref().unwrap().textarea,
            input,
            "first commit retains the native input client"
        )
    });
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), blank);
    cx.simulate_input("再");
    assert_eq!(text(&editor, cx), filled.replace('新', "再"));
    cx.simulate_keystrokes("cmd-z cmd-z");
    assert_eq!(text(&editor, cx), source);
    cx.update(|_, cx| {
        assert!(editor.read(cx).empty_slots.iter().all(|slot| !editor.read(cx).available_empty_slot(slot)))
    });
    cx.simulate_keystrokes("cmd-shift-z");
    assert_eq!(text(&editor, cx), blank);
    cx.simulate_input("重");
    assert_eq!(text(&editor, cx), filled.replace('新', "重"));
}

#[gpui_kit::test]
fn empty_split_style_survives_blur_other_edits_and_multiline_input(cx: &mut TestAppContext) {
    let source = "one {.red align=center indent=2 custom=value}\n\nlast\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_keystrokes("enter escape");
    settle(cx);
    let blank = text(&editor, cx);
    let offset = cx.update(|_, cx| editor.read(cx).empty_slots[0].range.start);
    let bounds = cx
        .debug_bounds(Box::leak(format!("empty-paragraph-{offset}").into_boxed_str()))
        .expect("a blurred empty paragraph remains clickable");
    cx.simulate_click(bounds.center(), Default::default());
    cx.run_until_parked();
    cx.update(|_, cx| assert!(editor.read(cx).focused.as_ref().unwrap().empty_attrs.is_some()));
    focus_unit(&editor, 0, cx);
    cx.simulate_input("before");
    cx.simulate_keystrokes("escape");
    settle(cx);
    let shifted = cx.update(|_, cx| editor.read(cx).empty_slots[0].range.start);
    assert_eq!(shifted, offset + "before".len(), "blank positions follow edits in other paragraphs");
    let bounds = cx.debug_bounds(Box::leak(format!("empty-paragraph-{shifted}").into_boxed_str())).unwrap();
    cx.simulate_click(bounds.center(), Default::default());
    cx.run_until_parked();
    cx.update(|_, cx| {
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
            "第一行\n第二行\n\n末段 {.blue align=right}".into(),
        ))
    });
    cx.simulate_keystrokes("cmd-v");
    let filled = text(&editor, cx);
    assert!(filled.contains("第一行\n第二行 {.red align=center indent=2 custom=value}"), "{filled}");
    assert!(filled.contains("末段 {.blue indent=2 custom=value align=right}"), "{filled}");
    assert!(filled.ends_with("\n\nlast\n"), "{filled}");
    cx.simulate_input("续");
    assert!(text(&editor, cx).contains("末段续 {.blue"), "caret remains inside the final pasted body");
    cx.simulate_keystrokes("cmd-z cmd-z");
    assert_eq!(text(&editor, cx), blank.replace("one", "onebefore"));
    cx.simulate_input("再");
    assert!(text(&editor, cx).contains("再 {.red align=center indent=2 custom=value}"));
}

#[gpui_kit::test]
fn multiple_empty_split_slots_keep_distinct_styles(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "red {.red}\n\nblue {.blue}\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    for ix in [0, 1] {
        focus_unit(&editor, ix, cx);
        cx.simulate_keystrokes("enter escape");
    }
    settle(cx);
    let offsets =
        cx.update(|_, cx| editor.read(cx).empty_slots.iter().map(|s| s.range.start).collect::<Vec<_>>());
    assert_eq!(offsets.len(), 2);
    for (offset, value, color) in [(offsets[1], "乙", "blue"), (offsets[0], "甲", "red")] {
        let bounds =
            cx.debug_bounds(Box::leak(format!("empty-paragraph-{offset}").into_boxed_str())).unwrap();
        cx.simulate_click(bounds.center(), Default::default());
        cx.run_until_parked();
        cx.simulate_input(value);
        cx.simulate_keystrokes("escape");
        settle(cx);
        assert!(text(&editor, cx).contains(&format!("{value} {{.{color}}}")));
    }
}

pub(super) fn open(
    cx: &mut TestAppContext,
    body: &str,
) -> (tempfile::TempDir, Arc<Vault>, VaultPath, Entity<Editor>) {
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

/// 带渲染服务的编辑器：图片块要有服务才画位图，没有就退回源码。
fn open_with_images(
    cx: &mut TestAppContext,
    body: &str,
) -> (tempfile::TempDir, tempfile::TempDir, Arc<Vault>, Entity<Editor>) {
    let (dir, vault, _, editor) = open(cx, body);
    let cache = tempfile::tempdir().unwrap();
    let (v, c) = (vault.clone(), cache.path().to_path_buf());
    cx.update(|cx| {
        let service = cx.new(|_| crate::render::RenderService::new(v, c));
        editor.update(cx, |e, cx| e.connect_images(service, cx));
    });
    (dir, cache, vault, editor)
}

#[gpui_kit::test]
fn rich_mermaid_bypasses_code_presenter_but_other_fences_do_not(cx: &mut TestAppContext) {
    let source = "```mermaid\ngraph LR\n  A[中文] --> B[预览]\n```\n\n```rust\nlet x = 1;\n```\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    settle(cx);
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert!(
            !e.presenters.contains_key(&super::presenter::Key::unit(e.units[0].id)),
            "Mermaid must not be swallowed by the ordinary code presenter"
        );
        assert!(e.presenters.contains_key(&super::presenter::Key::unit(e.units[1].id)));
    });
    focus_unit(&editor, 0, cx);
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert!(e.focused.as_ref().unwrap().textarea.read(cx).value().starts_with("```mermaid"));
    });
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn rich_pdf_bypasses_text_presenter_including_invalid_pages(cx: &mut TestAppContext) {
    let source = "![[paper.pdf]]\n\n![[paper.pdf#page=0|400]]\n\nordinary **text**\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    settle(cx);
    cx.update(|_, cx| {
        let e = editor.read(cx);
        for unit in &e.units[..2] {
            assert!(
                !e.presenters.contains_key(&super::presenter::Key::unit(unit.id)),
                "PDF source and errors must not be swallowed by a text presenter"
            );
        }
        assert!(e.presenters.contains_key(&super::presenter::Key::unit(e.units[2].id)));
    });
    focus_unit(&editor, 0, cx);
    cx.update(|_, cx| {
        assert_eq!(editor.read(cx).focused.as_ref().unwrap().textarea.read(cx).value(), "![[paper.pdf]]");
    });
    assert_eq!(text(&editor, cx), source);
}

fn rich_root(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        window.replace_root(cx, |window, cx| gpui_kit::component::Root::new(editor.clone(), window, cx));
    });
    settle(cx);
}

#[gpui_kit::test]
fn rich_selection_and_highlight_keep_images_and_following_text_still(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, px, size};
    for source in [
        "```mermaid\nflowchart LR\n A[开始] --> B[结束]\n```\n\n后续正文\n",
        "![[paper.pdf|400]]\n\n后续正文\n",
        "![[paper.pdf|400]] {.bg-blue}\n\n后续正文\n",
    ] {
        let (dir, _cache, _vault, editor) = open_with_images(cx, source);
        std::fs::write(dir.path().join("paper.pdf"), crate::render::two_page_pdf()).unwrap();
        let handle = *cx.windows().last().unwrap();
        let cx = &mut VisualTestContext::from_window(handle, cx);
        rich_root(&editor, cx);
        let (id, following) = cx.update(|_, cx| {
            let e = editor.read(cx);
            (e.units[0].id, e.units[1].id)
        });
        let following: &'static str = Box::leak(format!("unit-{}", following.0).into_boxed_str());
        let image = if source.starts_with("```") { "mermaid-image" } else { "pdf-image" };
        for width in [600., 900.] {
            cx.simulate_resize(size(px(width), px(1200.)));
            settle(cx);
            let before = (cx.debug_bounds(image).unwrap(), cx.debug_bounds(following).unwrap());
            cx.simulate_click(before.0.center(), Modifiers::default());
            settle(cx);
            assert_eq!(cx.debug_bounds(image).unwrap(), before.0, "selection: {source}, width {width}");
            assert_eq!(cx.debug_bounds(following).unwrap(), before.1, "following text must not move");
            cx.simulate_keystrokes("escape");
            settle(cx);
            assert_eq!(cx.debug_bounds(image).unwrap(), before.0);
            cx.update(|_, cx| {
                editor.update(cx, |e, cx| {
                    e.highlight = Some(id);
                    cx.notify();
                })
            });
            settle(cx);
            assert_eq!(cx.debug_bounds(image).unwrap(), before.0, "highlight: {source}, width {width}");
            assert_eq!(cx.debug_bounds(following).unwrap(), before.1);
            cx.update(|_, cx| {
                editor.update(cx, |e, cx| {
                    e.highlight = None;
                    cx.notify();
                })
            });
            settle(cx);
        }
        assert_eq!(text(&editor, cx), source);
    }
}

#[gpui_kit::test]
fn rich_long_errors_keep_retry_visible_and_clickable(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, px, size};
    let source = "```mermaid\nthis is not a diagram\n```\n";
    let (_dir, _cache, _vault, editor) = open_with_images(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    rich_root(&editor, cx);
    let id = cx.update(|_, cx| editor.read(cx).units[0].id);
    let unit: &'static str = Box::leak(format!("unit-{}", id.0).into_boxed_str());
    for width in [360., 640.] {
        cx.simulate_resize(size(px(width), px(1200.)));
        settle(cx);
        let frame = cx.debug_bounds(unit).unwrap();
        let error = cx.debug_bounds("mermaid-error").unwrap();
        let retry = cx.debug_bounds("mermaid-retry").unwrap();
        assert!(
            error.left() >= frame.left() && error.right() <= frame.right(),
            "error: {error:?}, frame: {frame:?}"
        );
        assert!(
            retry.left() >= error.right() && retry.right() <= frame.right(),
            "retry: {retry:?}, frame: {frame:?}"
        );
        assert!(retry.size.width > px(20.));
        cx.simulate_click(retry.center(), Modifiers::default());
        settle(cx);
        assert!(cx.update(|_, cx| editor.read(cx).focused.is_none()), "retry must not edit the source");
        assert!(cx.debug_bounds("mermaid-error").is_some());
        assert_eq!(text(&editor, cx), source);
    }
}

#[gpui_kit::test]
fn rich_mermaid_fits_narrow_and_wide_windows(cx: &mut TestAppContext) {
    use gpui_kit::{px, size};
    let source = "```mermaid\ngraph LR\n A[开始] --> B[处理] --> C[检查] --> D[完成]\n```\n";
    let (_dir, _cache, _vault, editor) = open_with_images(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.simulate_resize(size(px(1200.), px(1000.)));
    rich_root(&editor, cx);
    let natural = cx.debug_bounds("mermaid-block").unwrap().size;
    for width in [360., 640., 1200.] {
        cx.simulate_resize(size(px(width), px(1000.)));
        settle(cx);
        assert_rich_image_fits(cx, "mermaid-block", "mermaid-image", natural);
    }
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn rich_pdf_fits_narrow_and_wide_windows_even_at_4096(cx: &mut TestAppContext) {
    use gpui_kit::{px, size};
    for requested in [680, 4096, 240] {
        let source = format!("![[paper.pdf|{requested}]]\n");
        let (dir, _cache, _vault, editor) = open_with_images(cx, &source);
        std::fs::write(dir.path().join("paper.pdf"), crate::render::two_page_pdf()).unwrap();
        let window = *cx.windows().last().unwrap();
        let cx = &mut VisualTestContext::from_window(window, cx);
        rich_root(&editor, cx);
        for width in [360., 640., 1200.] {
            cx.simulate_resize(size(px(width), px(1000.)));
            settle(cx);
            assert!(cx.debug_bounds("pdf-block").is_some(), "PDF width {requested}, window {width}");
            assert_rich_image_fits(
                cx,
                "pdf-block",
                "pdf-image",
                size(px(requested as f32), px(requested as f32 / 2.)),
            );
        }
        assert_eq!(text(&editor, cx), source);
        let next = cx.debug_bounds("pdf-next").unwrap();
        cx.simulate_click(next.center(), gpui_kit::Modifiers::default());
        settle(cx);
        assert!(cx.debug_bounds("pdf-page-2-of-2").is_some(), "width {requested} still supports paging");
        assert_eq!(text(&editor, cx), source.replace("paper.pdf|", "paper.pdf#page=2|"));
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(text(&editor, cx), source);
    }
}

fn assert_rich_image_fits(
    cx: &mut VisualTestContext,
    block: &'static str,
    image: &'static str,
    natural: gpui_kit::Size<gpui_kit::Pixels>,
) {
    use gpui_kit::px;
    let bounds = cx.debug_bounds(block).expect("rendered block");
    let expected_height = bounds.size.width * (natural.height / natural.width);
    assert!(
        (f32::from(bounds.size.height - expected_height)).abs() < 1.,
        "{block} must shrink proportionally, not clip: {bounds:?}, natural {natural:?}"
    );
    let image = cx.debug_bounds(image).expect("rendered image bounds");
    assert_eq!(image, bounds, "the bitmap must follow its container without clipping");
    assert!(bounds.size.width > px(0.) && bounds.size.width <= natural.width + px(1.));
    let viewport = cx.update(|window, _| window.viewport_size());
    assert!(bounds.left() >= px(0.) && bounds.right() <= viewport.width);
    assert!(bounds.top() >= px(0.) && bounds.bottom() <= viewport.height);
}

#[gpui_kit::test]
fn rich_pdf_next_click_commits_other_text_and_has_separate_undo(cx: &mut TestAppContext) {
    let source = "正文\n\n![[paper.pdf|240]]\n";
    let (dir, _cache, _vault, editor) = open_with_images(cx, source);
    std::fs::write(dir.path().join("paper.pdf"), crate::render::two_page_pdf()).unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    rich_root(&editor, cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input("新增");
    settle(cx);
    let before_page = text(&editor, cx);
    let caret =
        cx.update(|_, cx| editor.read(cx).focused.as_ref().unwrap().textarea.read(cx).selected_range());
    cx.simulate_keystrokes("left");
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert_ne!(e.focused.as_ref().unwrap().textarea.read(cx).selected_range(), caret);
    });
    cx.simulate_keystrokes("right");
    assert_eq!(text(&editor, cx), before_page, "text arrows must not page");
    let next = cx.debug_bounds("pdf-next").unwrap();
    cx.simulate_click(next.center(), gpui_kit::Modifiers::default());
    settle(cx);
    let page_two = before_page.replace("paper.pdf|", "paper.pdf#page=2|");
    assert_eq!(text(&editor, cx), page_two, "a direct button click must leave editing and turn the page");
    assert!(cx.debug_bounds("pdf-page-2-of-2").is_some());
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert!(e.focused.is_none());
        assert_eq!(e.selected_blocks(), Some(1..2));
    });
    assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), before_page);
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), before_page, "first undo only reverts paging");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), source, "second undo reverts the text edit");
    cx.simulate_keystrokes("cmd-shift-z cmd-shift-z");
    assert_eq!(text(&editor, cx), page_two);
    cx.update(|window, cx| editor.update(cx, |e, cx| e.select_blocks(0, 1, window, cx)));
    cx.simulate_keystrokes("left right");
    assert_eq!(text(&editor, cx), page_two, "multi-block selection must not page");
}

#[gpui_kit::test]
fn rich_nested_mermaid_uses_read_only_preview(cx: &mut TestAppContext) {
    for source in [
        "> ```mermaid\n> graph LR\n> A --> B\n> ```\n",
        "> [!note] Diagram\n> ```mermaid\n> graph LR\n> A --> B\n> ```\n",
    ] {
        let (_dir, _cache, _vault, editor) = open_with_images(cx, source);
        let window = *cx.windows().last().unwrap();
        let cx = &mut VisualTestContext::from_window(window, cx);
        rich_root(&editor, cx);
        let diagram = cx.debug_bounds("mermaid-block").expect("nested Mermaid must render");
        double_click(cx, diagram.center());
        settle(cx);
        assert!(cx.debug_bounds("mermaid-export-menu").is_none());
        assert!(cx.update(|_, cx| editor.read(cx).focused.is_none()));
        assert_eq!(text(&editor, cx), source);
    }
}

#[gpui_kit::test]
fn rich_inline_pdf_keeps_quote_and_callout_text_presenters(cx: &mut TestAppContext) {
    use super::presenter::{Key, Part};
    let source = "> 普通 **引用**\n\n> [!note] 普通\n> 正文\n\n> 文字与 PDF 同一段\n> ![[paper.pdf]]\n\n> [!note] 混排\n> 文字与 PDF 同一段\n> ![[paper.pdf]]\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    settle(cx);
    cx.update(|_, cx| {
        let e = editor.read(cx);
        for unit in &e.units {
            let part = if matches!(unit.kind, aero_core::BlockKind::Callout { .. }) {
                Part::CalloutBody
            } else {
                Part::Unit
            };
            assert!(e.presenters.contains_key(&Key { id: unit.id, part }));
        }
    });
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn rich_pdf_quote_uses_read_only_preview(cx: &mut TestAppContext) {
    assert_nested_pdf_read_only(cx, "> ![[paper.pdf|240]]\n");
}

#[gpui_kit::test]
fn rich_pdf_callout_uses_read_only_preview(cx: &mut TestAppContext) {
    assert_nested_pdf_read_only(cx, "> [!note] PDF\n> ![[paper.pdf|240]]\n");
}

fn assert_nested_pdf_read_only(cx: &mut TestAppContext, source: &str) {
    let (dir, _cache, _vault, editor) = open_with_images(cx, source);
    std::fs::write(dir.path().join("paper.pdf"), crate::render::two_page_pdf()).unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    rich_root(&editor, cx);
    let page = cx.debug_bounds("pdf-block").expect("nested PDF must reach the preview renderer");
    assert!(cx.debug_bounds("pdf-page-1-of-2").is_some());
    assert!(cx.debug_bounds("pdf-prev").is_none());
    assert!(cx.debug_bounds("pdf-next").is_none());
    cx.simulate_click(page.center(), gpui_kit::Modifiers::default());
    settle(cx);
    cx.update(|_, cx| {
        assert!(editor.read(cx).focused.is_none(), "clicking a nested PDF must not edit its parent");
        assert!(editor.read(cx).selection.is_none());
    });
    cx.update(|window, cx| editor.update(cx, |e, cx| e.select_blocks(0, 0, window, cx)));
    cx.simulate_keystrokes("left right");
    double_click(cx, page.center());
    settle(cx);
    assert_eq!(
        cx.opened_url(),
        Some(crate::paths::file_url(&dir.path().join("paper.pdf").canonicalize().unwrap()))
    );
    assert_eq!(text(&editor, cx), source);
    assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), source);
}

#[gpui_kit::test]
fn rich_pdf_hover_uses_target_note_assets_and_stays_read_only(cx: &mut TestAppContext) {
    use gpui_kit::{Bounds, point, px};
    let source = "见 [[b]]\n";
    let target = "![[paper.pdf#page=2|240]]\n";
    let (dir, _cache, _vault, editor) = open_with_images(cx, source);
    std::fs::write(dir.path().join("b.md"), target).unwrap();
    for note in ["a", "b"] {
        std::fs::create_dir(dir.path().join(format!("{note}.assets"))).unwrap();
        std::fs::write(
            dir.path().join(format!("{note}.assets/paper.pdf")),
            if note == "a" { b"wrong PDF".to_vec() } else { crate::render::two_page_pdf() },
        )
        .unwrap();
    }
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    rich_root(&editor, cx);
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            e.install_test_resolver(&[("a.md", None), ("b.md", None)], cx);
            e.hover_link(
                &aero_core::links::encode_note_url("b", None).into(),
                true,
                Bounds::from_corners(point(px(100.), px(150.)), point(px(180.), px(175.))),
                window,
                cx,
            );
        })
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(500));
    settle(cx);
    assert!(cx.debug_bounds("link-preview-rich").is_some());
    let page = cx.debug_bounds("pdf-block").expect("hover PDF must resolve b.assets, not a.assets");
    assert!(cx.debug_bounds("pdf-page-2-of-2").is_some());
    assert!(cx.debug_bounds("pdf-next").is_none());
    assert!(cx.debug_bounds("pdf-prev").is_none());
    cx.simulate_click(page.center(), gpui_kit::Modifiers::default());
    cx.simulate_keystrokes("left right");
    settle(cx);
    cx.update(|_, cx| {
        assert!(editor.read(cx).focused.is_none());
        assert!(editor.read(cx).selection.is_none());
    });
    assert_eq!(cx.opened_url(), None, "single click only previews");
    double_click(cx, page.center());
    settle(cx);
    assert_eq!(
        cx.opened_url(),
        Some(crate::paths::file_url(&dir.path().join("b.assets/paper.pdf").canonicalize().unwrap()))
    );
    assert_eq!(text(&editor, cx), source);
    assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), source);
    assert_eq!(std::fs::read_to_string(dir.path().join("b.md")).unwrap(), target);
}

#[gpui_kit::test]
fn pdf_reader_toolbar_and_page_input_are_above_the_viewport(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, px, size};
    let source = "![[paper.pdf#page=1|400]]\n";
    let (dir, _cache, _vault, editor) = open_with_images(cx, source);
    std::fs::write(dir.path().join("paper.pdf"), crate::render::two_page_pdf()).unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.simulate_resize(size(px(900.), px(1000.)));
    rich_root(&editor, cx);
    let previous = cx.debug_bounds("pdf-prev").unwrap();
    let image = cx.debug_bounds("pdf-block").unwrap();
    assert!(previous.bottom() <= image.top(), "controls must be above the PDF page");
    let viewport = cx.debug_bounds("pdf-viewport").expect("independent PDF viewport");
    let toolbar = cx.debug_bounds("pdf-toolbar").unwrap();
    assert!(toolbar.bottom() <= viewport.top());
    for invalid in ["", "0", "3", "-1", "1.5", "not-a-page"] {
        let input = cx.debug_bounds("pdf-page-input").unwrap();
        cx.simulate_click(input.center(), Modifiers::default());
        cx.simulate_keystrokes("cmd-a backspace");
        if !invalid.is_empty() {
            cx.simulate_input(invalid);
        }
        cx.simulate_keystrokes("enter");
        settle(cx);
        assert_eq!(text(&editor, cx), source, "invalid page: {invalid}");
        assert!(cx.debug_bounds("pdf-page-1-of-2").is_some());
    }
    let input = cx.debug_bounds("pdf-page-input").unwrap();
    cx.simulate_click(input.center(), Modifiers::default());
    cx.simulate_keystrokes("cmd-a");
    cx.simulate_input("2");
    cx.simulate_keystrokes("enter");
    settle(cx);
    assert!(cx.debug_bounds("pdf-page-2-of-2").is_some());
    assert_eq!(text(&editor, cx), source.replace("page=1", "page=2"));
    cx.simulate_keystrokes("cmd-z");
    settle(cx);
    assert_eq!(text(&editor, cx), source);
    assert!(cx.debug_bounds("pdf-page-1-of-2").is_some());
}

#[gpui_kit::test]
fn pdf_reader_scrolls_across_pages_without_scrolling_or_editing_the_note(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, ScrollDelta, ScrollEvent, TouchPhase, point, px, size};
    let source = "![[paper.pdf|400]]\n\n正文\n";
    let (dir, _cache, _vault, editor) = open_with_images(cx, source);
    let pdf = String::from_utf8(crate::render::two_page_pdf()).unwrap().replace("200 100", "200 500");
    std::fs::write(dir.path().join("paper.pdf"), pdf).unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.simulate_resize(size(px(900.), px(1000.)));
    rich_root(&editor, cx);
    let viewport = cx.debug_bounds("pdf-viewport").expect("PDF scroll viewport");
    let toolbar = cx.debug_bounds("pdf-toolbar").unwrap();
    for (dy, current) in [(-1200., 2), (1200., 1)] {
        cx.simulate_event(ScrollEvent {
            position: viewport.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(dy))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Started,
            source: gpui_kit::ScrollSource::Pointer,
        });
        settle(cx);
        let selector: &'static str = Box::leak(format!("pdf-page-{current}-of-2").into_boxed_str());
        assert!(cx.debug_bounds(selector).is_some(), "wheel must reach page {current}");
        assert_eq!(cx.debug_bounds("pdf-toolbar").unwrap(), toolbar);
        assert_eq!(cx.debug_bounds("pdf-viewport").unwrap(), viewport);
        assert_eq!(text(&editor, cx), source, "passive scrolling must not create document edits");
    }
}

#[gpui_kit::test]
fn pdf_reader_input_does_not_edit_the_previously_focused_note_text(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, px, size};
    let source = "正文\n\n![[paper.pdf|400]]\n";
    let (dir, _cache, _vault, editor) = open_with_images(cx, source);
    std::fs::write(dir.path().join("paper.pdf"), crate::render::two_page_pdf()).unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.simulate_resize(size(px(900.), px(1000.)));
    rich_root(&editor, cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input("新增");
    settle(cx);
    let before = text(&editor, cx);
    let input = cx.debug_bounds("pdf-page-input").unwrap();
    cx.simulate_click(input.center(), Modifiers::default());
    cx.simulate_keystrokes("cmd-a backspace");
    cx.simulate_input("2");
    assert_eq!(text(&editor, cx), before, "page input must not change the note before confirmation");
    cx.simulate_keystrokes("enter");
    settle(cx);
    assert_eq!(text(&editor, cx), before.replace("paper.pdf|", "paper.pdf#page=2|"));
    cx.simulate_keystrokes("cmd-z");
    settle(cx);
    assert_eq!(text(&editor, cx), before, "page navigation is one separate undo group");
}

#[gpui_kit::test]
fn pdf_reader_empty_input_backspace_never_deletes_the_pdf_block(cx: &mut TestAppContext) {
    let source = "![[paper.pdf|400]]\n";
    let (dir, _cache, _vault, editor) = open_with_images(cx, source);
    std::fs::write(dir.path().join("paper.pdf"), crate::render::two_page_pdf()).unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    rich_root(&editor, cx);
    let page = cx.debug_bounds("pdf-block").unwrap().center();
    cx.simulate_click(page, gpui_kit::Modifiers::default());
    let input = cx.debug_bounds("pdf-page-input").unwrap().center();
    cx.simulate_click(input, gpui_kit::Modifiers::default());
    cx.simulate_keystrokes("cmd-a backspace backspace delete");
    settle(cx);
    assert_eq!(text(&editor, cx), source);
    assert!(cx.debug_bounds("pdf-viewport").is_some());
}

#[gpui_kit::test]
fn pdf_reader_input_undo_never_undoes_note_edits(cx: &mut TestAppContext) {
    let source = "正文\n\n![[paper.pdf|400]]\n";
    let (dir, _cache, _vault, editor) = open_with_images(cx, source);
    std::fs::write(dir.path().join("paper.pdf"), crate::render::two_page_pdf()).unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    rich_root(&editor, cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input("新增");
    settle(cx);
    let before = text(&editor, cx);
    let input = cx.debug_bounds("pdf-page-input").unwrap().center();
    cx.simulate_click(input, gpui_kit::Modifiers::default());
    cx.simulate_keystrokes("cmd-a");
    cx.simulate_input("2");
    cx.simulate_keystrokes("cmd-z");
    settle(cx);
    assert_eq!(text(&editor, cx), before);
    cx.simulate_keystrokes("cmd-shift-z");
    settle(cx);
    assert_eq!(text(&editor, cx), before);
}

#[gpui_kit::test]
fn pdf_reader_resizes_from_both_edges_without_changing_the_note(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, point, px, size};
    let source = "![[paper.pdf|400]]\n";
    let (dir, _cache, _vault, editor) = open_with_images(cx, source);
    std::fs::write(dir.path().join("paper.pdf"), crate::render::two_page_pdf()).unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.simulate_resize(size(px(900.), px(1600.)));
    rich_root(&editor, cx);
    for (selector, dy) in [("pdf-resize-bottom", 80.), ("pdf-resize-top", -60.)] {
        let before = cx.debug_bounds("pdf-viewport").unwrap();
        let start = cx.debug_bounds(selector).expect("PDF resize edge").center();
        let end = start + point(px(0.), px(dy));
        cx.simulate_event(MouseDownEvent {
            position: start,
            modifiers: Modifiers::default(),
            button: MouseButton::Left,
            click_count: 1,
            first_mouse: false,
        });
        cx.simulate_event(MouseMoveEvent {
            position: end,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        });
        cx.simulate_event(MouseUpEvent {
            position: end,
            modifiers: Modifiers::default(),
            button: MouseButton::Left,
            click_count: 1,
        });
        settle(cx);
        let after = cx.debug_bounds("pdf-viewport").unwrap();
        assert_eq!(after.size.height, before.size.height + px(dy.abs()));
        assert_eq!(after.size.width, before.size.width);
        assert_eq!(text(&editor, cx), source);
    }
    let before = cx.debug_bounds("pdf-viewport").unwrap();
    let start = cx.debug_bounds("pdf-resize-bottom").unwrap().center();
    let end = start + point(px(0.), px(100.));
    cx.simulate_event(MouseDownEvent {
        position: start,
        modifiers: Modifiers::default(),
        button: MouseButton::Left,
        click_count: 1,
        first_mouse: false,
    });
    cx.simulate_event(MouseMoveEvent {
        position: end,
        pressed_button: Some(MouseButton::Left),
        modifiers: Modifiers::default(),
    });
    cx.simulate_keystrokes("escape");
    cx.simulate_event(MouseUpEvent {
        position: end,
        modifiers: Modifiers::default(),
        button: MouseButton::Left,
        click_count: 1,
    });
    settle(cx);
    assert_eq!(cx.debug_bounds("pdf-viewport").unwrap(), before, "Esc cancels the resize");
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn rich_pdf_paging_controls_keyboard_boundaries_and_undo(cx: &mut TestAppContext) {
    let source = "![[paper.pdf|240]]\n\n后文\n";
    let (dir, _cache, _vault, editor) = open_with_images(cx, source);
    std::fs::write(dir.path().join("paper.pdf"), crate::render::two_page_pdf()).unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    rich_root(&editor, cx);
    let page = cx.debug_bounds("pdf-block").expect("PDF must render its first page");
    assert!((f32::from(page.size.width) - 240.).abs() < 1.);
    assert!(cx.debug_bounds("pdf-page-1-of-2").is_some());
    cx.simulate_click(page.center(), gpui_kit::Modifiers::default());
    settle(cx);
    assert!(cx.update(|_, cx| editor.read(cx).selection.is_some()));
    assert!(cx.update(|_, cx| editor.read(cx).focused.is_none()));
    cx.simulate_keystrokes("left");
    assert_eq!(text(&editor, cx), source, "first-page left is a no-op");
    let prev = cx.debug_bounds("pdf-prev").unwrap();
    cx.simulate_click(prev.center(), gpui_kit::Modifiers::default());
    assert_eq!(text(&editor, cx), source, "first-page button is disabled");
    cx.simulate_keystrokes("right");
    settle(cx);
    let second = "![[paper.pdf#page=2|240]]\n\n后文\n";
    assert_eq!(text(&editor, cx), second);
    assert!(cx.debug_bounds("pdf-page-2-of-2").is_some());
    cx.simulate_keystrokes("right");
    assert_eq!(text(&editor, cx), second, "last-page right is a no-op");
    let next = cx.debug_bounds("pdf-next").unwrap();
    cx.simulate_click(next.center(), gpui_kit::Modifiers::default());
    assert_eq!(text(&editor, cx), second, "last-page button is disabled");
    cx.simulate_keystrokes("cmd-z");
    settle(cx);
    assert_eq!(text(&editor, cx), source, "paging is one independent undo group");
    cx.simulate_keystrokes("cmd-shift-z");
    settle(cx);
    assert_eq!(text(&editor, cx), second);
    let prev = cx.debug_bounds("pdf-prev").unwrap();
    cx.simulate_click(prev.center(), gpui_kit::Modifiers::default());
    settle(cx);
    assert_eq!(text(&editor, cx), "![[paper.pdf#page=1|240]]\n\n后文\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), second);
}

#[gpui_kit::test]
fn clicking_below_a_pdf_keeps_the_rendered_page(cx: &mut TestAppContext) {
    use gpui_kit::Modifiers;
    let source = "# 标题\n\n![[paper.pdf#page=1]]\n\n";
    let (dir, _cache, _vault, editor) = open_with_images(cx, source);
    std::fs::write(dir.path().join("paper.pdf"), crate::render::two_page_pdf()).unwrap();
    let cx = &mut VisualTestContext::from_window(*cx.windows().last().unwrap(), cx);
    rich_root(&editor, cx);
    assert!(cx.debug_bounds("pdf-block").is_some() || cx.debug_bounds("pdf-reader").is_some());
    let tail = cx.debug_bounds("editor-tail").expect("space below the note");
    cx.simulate_click(tail.center(), Modifiers::default());
    settle(cx);
    assert!(
        cx.debug_bounds("pdf-block").is_some() || cx.debug_bounds("pdf-reader").is_some(),
        "the page stays rendered"
    );
    cx.update(|_, cx| {
        let focused = editor.read(cx).focused.as_ref().map(|f| f.textarea.read(cx).value().to_string());
        assert_ne!(focused.as_deref(), Some("![[paper.pdf#page=1]]"));
    });
    assert!(text(&editor, cx).contains("![[paper.pdf#page=1]]"));
}

#[gpui_kit::test]
fn rich_pdf_opens_only_resolved_file_and_source_editing_stays_source(cx: &mut TestAppContext) {
    let source = "![[paper.pdf#page=2|240]]\n";
    let (dir, _cache, _vault, editor) = open_with_images(cx, source);
    std::fs::write(dir.path().join("paper.pdf"), crate::render::two_page_pdf()).unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    rich_root(&editor, cx);
    let page = cx.debug_bounds("pdf-block").unwrap();
    cx.simulate_click(page.center(), gpui_kit::Modifiers::default());
    settle(cx);
    cx.simulate_keystrokes("cmd-enter");
    settle(cx);
    assert_eq!(
        cx.opened_url(),
        Some(crate::paths::file_url(&dir.path().join("paper.pdf").canonicalize().unwrap()))
    );
    double_click(cx, page.center());
    settle(cx);
    assert!(cx.update(|_, cx| editor.read(cx).focused.is_none()));
    cx.simulate_keystrokes("enter");
    settle(cx);
    assert!(cx.debug_bounds("pdf-block").is_none(), "source mode does not render pages");
    assert!(cx.debug_bounds("pdf-next").is_none());
    cx.update(|_, cx| {
        assert_eq!(editor.read(cx).focused.as_ref().unwrap().textarea.read(cx).value(), source.trim());
    });
    cx.simulate_keystrokes("left right");
    assert_eq!(text(&editor, cx), source, "caret keys never page while editing");
}

#[gpui_kit::test]
fn rich_pdf_errors_keep_source_and_retry_recovers_missing_and_corrupt_files(cx: &mut TestAppContext) {
    let source = "![[paper.pdf]]\n";
    let (dir, _cache, _vault, editor) = open_with_images(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    rich_root(&editor, cx);
    for bytes in [b"broken PDF".to_vec(), crate::render::two_page_pdf()] {
        assert!(cx.debug_bounds("pdf-source").is_some());
        assert!(cx.debug_bounds("pdf-error").is_some());
        let retry = cx.debug_bounds("pdf-retry").expect("all PDF errors offer retry");
        std::fs::write(dir.path().join("paper.pdf"), bytes).unwrap();
        cx.simulate_click(retry.center(), gpui_kit::Modifiers::default());
        settle(cx);
    }
    assert!(cx.debug_bounds("pdf-block").is_some());
    assert!(cx.debug_bounds("pdf-error").is_none());
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn rich_pdf_invalid_page_and_external_targets_never_open_urls(cx: &mut TestAppContext) {
    let source = "![[https://example.com/paper.pdf#page=0]]\n";
    let (_dir, _cache, _vault, editor) = open_with_images(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    rich_root(&editor, cx);
    assert!(cx.debug_bounds("pdf-source").is_some());
    assert!(cx.debug_bounds("pdf-error").is_some());
    assert!(cx.debug_bounds("pdf-retry").is_some());
    cx.update(|window, cx| editor.update(cx, |e, cx| e.select_blocks(0, 0, window, cx)));
    cx.simulate_keystrokes("left right cmd-enter");
    settle(cx);
    assert_eq!(cx.opened_url(), None);
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn rich_mermaid_source_preview_is_debounced_and_stale_input_is_cancelled(cx: &mut TestAppContext) {
    let source = "```mermaid\ngraph LR\n A[中文] --> B[预览]\n```\n";
    let (_dir, _cache, _vault, editor) = open_with_images(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    rich_root(&editor, cx);
    assert!(cx.debug_bounds("mermaid-block").is_some());
    focus_unit(&editor, 0, cx);
    settle(cx);
    assert!(cx.debug_bounds("mermaid-live-preview").is_some());
    select_inline_text(&editor, "graph LR\n A[中文] --> B[预览]", cx);
    cx.simulate_input("not a diagram");
    settle(cx);
    assert!(cx.debug_bounds("mermaid-block").is_some(), "typing keeps the previous bitmap");
    assert!(cx.debug_bounds("mermaid-error").is_none());
    cx.executor().advance_clock(Duration::from_millis(299));
    settle(cx);
    assert!(cx.debug_bounds("mermaid-error").is_none(), "no heavy render before 300 ms");
    select_inline_text(&editor, "not a diagram", cx);
    cx.simulate_input("graph TD\n C --> D");
    cx.executor().advance_clock(Duration::from_millis(1));
    settle(cx);
    assert!(cx.debug_bounds("mermaid-error").is_none(), "old timer must not publish stale source");
    cx.executor().advance_clock(Duration::from_millis(299));
    settle(cx);
    assert!(cx.debug_bounds("mermaid-block").is_some());
    select_inline_text(&editor, "graph TD\n C --> D", cx);
    cx.simulate_input("not a diagram");
    cx.executor().advance_clock(Duration::from_millis(300));
    settle(cx);
    assert!(cx.debug_bounds("mermaid-error").is_some(), "current invalid source eventually renders an error");
    assert!(cx.debug_bounds("mermaid-retry").is_some());
}

#[gpui_kit::test]
fn rich_mermaid_export_requires_choice_preserves_source_and_never_overwrites(cx: &mut TestAppContext) {
    let source = "```mermaid\ngraph LR\n A[中文] --> B[完成]\n```\n";
    let (dir, _cache, _vault, editor) = open_with_images(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    rich_root(&editor, cx);
    let diagram = cx.debug_bounds("mermaid-block").unwrap();
    cx.simulate_click(diagram.center(), gpui_kit::Modifiers::default());
    settle(cx);
    assert!(cx.update(|_, cx| editor.read(cx).selection.is_some()));
    assert!(cx.update(|_, cx| editor.read(cx).focused.is_none()));
    double_click(cx, diagram.center());
    settle(cx);
    assert!(cx.debug_bounds("mermaid-export-menu").is_some());
    assert!(!dir.path().join("a.assets").exists(), "double click alone must not export");
    let mut png_files = Vec::new();
    for selector in ["mermaid-export-png", "mermaid-export-png", "mermaid-export-svg"] {
        let choice = cx.debug_bounds(selector).unwrap();
        cx.simulate_click(choice.center(), gpui_kit::Modifiers::default());
        settle(cx);
        assert_eq!(text(&editor, cx), source);
        assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), source);
        assert!(cx.debug_bounds("mermaid-export-result").is_some(), "show the exported path");
        let entries = std::fs::read_dir(dir.path().join("a.assets")).unwrap();
        for entry in entries {
            let path = entry.unwrap().path();
            if path.extension().unwrap() == "png" && !png_files.iter().any(|(p, _)| p == &path) {
                let bytes = std::fs::read(&path).unwrap();
                image::load_from_memory(&bytes).unwrap();
                png_files.push((path, bytes));
            }
        }
        let diagram = cx.debug_bounds("mermaid-block").unwrap();
        double_click(cx, diagram.center());
        settle(cx);
    }
    assert_eq!(png_files.len(), 2, "second export allocates a distinct filename");
    for (path, bytes) in png_files {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    assert_eq!(std::fs::read_dir(dir.path().join("a.assets")).unwrap().count(), 3);
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), source, "export adds no document undo entry");
}

#[gpui_kit::test]
fn rich_mermaid_export_blocks_leaving_until_disk_write_finishes(cx: &mut TestAppContext) {
    let source = "```mermaid\ngraph LR\n A --> B\n```\n";
    let (_dir, _cache, _vault, editor) = open_with_images(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    rich_root(&editor, cx);
    let diagram = cx.debug_bounds("mermaid-block").unwrap();
    double_click(cx, diagram.center());
    settle(cx);
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            e.export_diagram(e.units[0].id, false, window, cx);
            assert!(e.flush_sync().is_err(), "cannot rename or close a vault while exporting");
        })
    });
    settle(cx);
    assert!(cx.update(|_, cx| editor.update(cx, |e, _| e.flush_sync().is_ok())));
}

#[gpui_kit::test]
fn rich_mermaid_export_cancelled_by_rename_does_not_write_to_either_note(cx: &mut TestAppContext) {
    let source = "```mermaid\ngraph LR\n A --> B\n```\n";
    let (dir, _cache, _vault, editor) = open_with_images(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    rich_root(&editor, cx);
    let diagram = cx.debug_bounds("mermaid-block").unwrap();
    double_click(cx, diagram.center());
    settle(cx);
    assert!(cx.debug_bounds("mermaid-export-svg").is_some());
    // simulate_click drains the executor; start and rename in the same update to keep export pending.
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            e.export_diagram(e.units[0].id, false, window, cx);
            e.set_path(VaultPath::new("renamed.md").unwrap(), cx);
        })
    });
    settle(cx);
    assert!(!dir.path().join("a.assets").exists());
    assert!(!dir.path().join("renamed.assets").exists());
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn rich_pdf_nested_note_preview_is_read_only_and_resolves_its_own_assets(cx: &mut TestAppContext) {
    let source = "![[b]]\n";
    let (dir, _cache, _vault, editor) = open_with_images(cx, source);
    std::fs::write(dir.path().join("b.md"), "![[paper.pdf#page=2|240]]\n").unwrap();
    for note in ["a", "b"] {
        std::fs::create_dir(dir.path().join(format!("{note}.assets"))).unwrap();
        std::fs::write(
            dir.path().join(format!("{note}.assets/paper.pdf")),
            if note == "a" { b"wrong PDF".to_vec() } else { crate::render::two_page_pdf() },
        )
        .unwrap();
    }
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|_, cx| {
        editor.update(cx, |e, cx| e.install_test_resolver(&[("a.md", None), ("b.md", None)], cx))
    });
    rich_root(&editor, cx);
    settle(cx);
    let page = cx.debug_bounds("pdf-block").expect("embedded PDF resolves b.assets, not a.assets");
    assert!(cx.debug_bounds("pdf-page-2-of-2").is_some());
    assert!(cx.debug_bounds("pdf-next").is_none());
    assert!(cx.debug_bounds("pdf-prev").is_none());
    cx.simulate_click(page.center(), gpui_kit::Modifiers::default());
    cx.simulate_keystrokes("left right");
    settle(cx);
    assert_eq!(text(&editor, cx), source);
    double_click(cx, page.center());
    settle(cx);
    assert_eq!(
        cx.opened_url(),
        Some(crate::paths::file_url(&dir.path().join("b.assets/paper.pdf").canonicalize().unwrap()))
    );
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn rich_pdf_page_edit_preserves_raw_target_width_and_separate_undo(cx: &mut TestAppContext) {
    let source = "![[ paper\\#v1.pdf #page=1 | 240 ]]\r\n\r\n正文\r\n";
    let (dir, _cache, _vault, editor) = open_with_images(cx, source);
    std::fs::write(dir.path().join("paper#v1.pdf"), crate::render::two_page_pdf()).unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    rich_root(&editor, cx);
    focus_unit(&editor, 1, cx);
    cx.simulate_input("新增");
    cx.simulate_keystrokes("escape");
    settle(cx);
    let before_page = text(&editor, cx);
    let page = cx.debug_bounds("pdf-block").unwrap();
    cx.simulate_click(page.center(), gpui_kit::Modifiers::default());
    cx.simulate_keystrokes("right");
    settle(cx);
    assert_eq!(text(&editor, cx), before_page.replace("#page=1", "#page=2"));
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), before_page);
    cx.simulate_keystrokes("cmd-z");
    // Document uses LF internally; save restores the original file's CRLF.
    assert_eq!(text(&editor, cx), source.replace("\r\n", "\n"));
}

/// 纯色 PNG，宽高可控。
fn image_bytes(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(width, height, image::Rgba([20, 40, 60, 255]));
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

fn write_attachment(dir: &tempfile::TempDir, rel: &str, width: u32, height: u32) {
    let path = dir.path().join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, image_bytes(width, height)).unwrap();
}

/// 渲染要好几跳：绘制才会排队（枚举附件 → 解析 → 渲染），后台完成后又要重绘。
fn settle(cx: &mut VisualTestContext) {
    for _ in 0..4 {
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }
}

fn double_click(cx: &mut VisualTestContext, position: gpui_kit::Point<gpui_kit::Pixels>) {
    use gpui_kit::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent};
    for click_count in [1, 2] {
        let (position, modifiers, button) = (position, Modifiers::default(), MouseButton::Left);
        cx.simulate_event(MouseDownEvent { position, modifiers, button, click_count, first_mouse: false });
        cx.simulate_event(MouseUpEvent { position, modifiers, button, click_count });
    }
}

fn wiki_catalog(editor: &Entity<Editor>, recent: &[&str], cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            let notes = e
                .vault
                .notes()
                .unwrap()
                .into_iter()
                .map(|p| {
                    let doc = e.vault.read(&p).unwrap().0;
                    aero_core::links::NoteSummary::from_document(p, &doc)
                })
                .collect();
            let generation = e.wiki.as_ref().expect("wiki menu").generation;
            let task = cx.background_spawn(async move { Ok(notes) });
            e.load_wiki_notes(
                generation,
                task,
                recent.iter().map(|p| VaultPath::new(p).unwrap()).collect(),
                window,
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

/// 聚焦文档里 `text` 所在的那一段（表格里就是那一格），并选中它。
fn focus_text(editor: &Entity<Editor>, text: &str, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            let at = e.doc.to_string().find(text).expect("text in document");
            e.focus_at(at, None, window, cx);
        })
    });
    cx.run_until_parked();
    select_inline_text(editor, text, cx);
}

fn selected_inline_text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    cx.update(|_, cx| {
        let ta = editor.read(cx).focused.as_ref().unwrap().textarea.read(cx);
        ta.value()[ta.selected_range()].to_owned()
    })
}

/// 聚焦块默认藏起成对标记，源码仍留在文本框里（9.1）。
#[gpui_kit::test]
fn focused_block_hides_inline_markers(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "say **bold** please\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.update(|_, cx| {
        let ta = editor.read(cx).focused.as_ref().unwrap().textarea.clone();
        let text = ta.read(cx).value().to_string();
        assert_eq!(text, "say **bold** please");
        let star = text.find("**").unwrap();
        let word = text.find("bold").unwrap();
        let stars = ta.read(cx).range_to_bounds(&(star..star + 2)).expect("marker bounds");
        let bold = ta.read(cx).range_to_bounds(&(word..word + 4)).expect("word bounds");
        assert!(stars.size.width < gpui_kit::px(1.), "markers collapse");
        assert!(bold.size.width > gpui_kit::px(8.));
    });
}

/// 段首打完 `- ` 后，控件里只剩列表项正文。
#[gpui_kit::test]
fn typing_a_bullet_marker_promotes_the_paragraph(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "hello\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.update(|_, cx| {
        let ta = editor.read(cx).focused.as_ref().unwrap().textarea.clone();
        ta.update(cx, |ta, cx| ta.set_selected_range(0..0, cx));
    });
    cx.simulate_input("- ");
    assert_eq!(text(&editor, cx), "- hello\n");
    cx.update(|_, cx| {
        assert_eq!(editor.read(cx).focused.as_ref().unwrap().textarea.read(cx).value(), "hello");
    });
}

#[gpui_kit::test]
fn backlink_selection_notifies_when_the_caret_moves_between_nested_blocks(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "> first ^one\n>\n> second ^two\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let _subscription = cx.update(|_, cx| {
        let observed = observed.clone();
        cx.observe(&editor, move |editor, cx| {
            observed.lock().unwrap().push(editor.read(cx).selected_reference(cx));
        })
    });
    focus_unit(&editor, 1, cx);
    select_inline_text(&editor, "second", cx);
    cx.update(|_, cx| assert_eq!(editor.read(cx).selected_reference(cx).as_deref(), Some("two")));
    assert_eq!(observed.lock().unwrap().last().and_then(|id| id.as_deref()), Some("two"));
}

#[gpui_kit::test]
fn backlinks_connect_with_an_existing_selection(cx: &mut TestAppContext) {
    let (_dir, vault, path, editor) = open(cx, "first ^one\n");
    let data = tempfile::tempdir().unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    let (search, view) = cx.update(|_, cx| {
        let search = cx.new(|cx| crate::search::SearchService::new(vault, data.path().into(), cx));
        let view = cx.new(|cx| crate::backlinks::BacklinksView::new(search.clone(), cx));
        view.update(cx, |view, cx| view.set_note(Some(path), cx));
        editor.update(cx, |e, cx| e.connect_backlinks(view.clone(), cx));
        (search, view)
    });
    cx.update(|window, cx| {
        window.replace_root(cx, |window, cx| gpui_kit::component::Root::new(view.clone(), window, cx));
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("backlinks-block").is_some(), "connecting must pick up an already focused ^id");
    cx.update(|_, cx| search.read(cx).close_blocking());
}

#[gpui_kit::test]
fn highlight_color_is_painted_while_editing_and_removed_by_toggle(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "正文 中文😀 后文\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    select_inline_text(&editor, "中文😀", cx);
    cx.simulate_keystrokes("cmd-shift-h");
    let quads = highlight_quads(cx);
    assert!(!quads.is_empty(), "editing paints the highlight background, not ==");
    cx.update(|window, cx| {
        let ta = editor.read(cx).focused.as_ref().unwrap().textarea.read(cx);
        let marks = ta.value().find("==").unwrap();
        let hidden = ta.range_to_bounds(&(marks..marks + 2)).unwrap();
        assert!(hidden.size.width.abs() < gpui_kit::px(0.5), "== is not drawn");
        let at = ta.value().find("中文😀").unwrap();
        let left = ta.range_to_bounds(&(at..at)).unwrap().scale(window.scale_factor()).left();
        assert!((quads[0].bounds.left().0 - left.0).abs() < 1.5, "{:?} vs {left:?}", quads[0].bounds);
        assert!(quads.iter().all(|q| q.bounds.size.width.0 > 0. && q.bounds.size.height.0 > 0.));
    });
    cx.simulate_keystrokes("cmd-shift-h");
    assert!(highlight_quads(cx).is_empty());
    assert_eq!(text(&editor, cx), "正文 中文😀 后文\n");
    cx.simulate_keystrokes("cmd-z");
    assert!(!highlight_quads(cx).is_empty());
}

#[gpui_kit::test]
fn highlight_wraps_by_row_and_code_source_paints_none(cx: &mut TestAppContext) {
    let source = format!("=={}==\n\n```md\n==code==\n```\n", "中文文字 ".repeat(300).trim_end());
    assert_eq!(aero_core::highlight::ranges(&source).len(), 1, "fixture must contain a parsed highlight");
    let (_dir, _vault, _path, editor) = open(cx, &source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    let quads = highlight_quads(cx);
    assert!(quads.len() > 1, "wrapped highlight paints every visual row");
    cx.update(|window, cx| {
        let f = editor.read(cx).focused.as_ref().unwrap();
        let bounds = f.textarea.read(cx).text_bounds().unwrap().scale(window.scale_factor());
        assert!(quads.iter().all(|q| q.bounds.size.width <= bounds.size.width));
    });
    focus_unit(&editor, 1, cx);
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        assert!(editor.read(cx).focused.as_ref().unwrap().highlight.painted().is_empty());
    });
}

/// 这一帧里用某个颜色铺过的矩形（块底色、行内背景、选中态都是 quad）。
fn quads_of(cx: &mut VisualTestContext, color: gpui_kit::Hsla) -> Vec<gpui_kit::Quad> {
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        window.painted_quads().into_iter().filter(|q| q.background.as_solid() == Some(color)).collect()
    })
}

fn highlight_quads(cx: &mut VisualTestContext) -> Vec<gpui_kit::Quad> {
    let color = cx.update(|_, cx| super::highlight_ui::color(cx));
    quads_of(cx, color)
}

/// 调色板颜色在当前主题下的色值（测试窗口是浅色主题）。
fn content_color(slot: aero_core::colors::Slot, color: &str) -> gpui_kit::Hsla {
    crate::colors::token(slot, color, false).expect("调色板里有这个颜色")
}

/// 块颜色（6.3.10 / 7.7）：`⌘⇧H` 在块选中态写到块末，预览里铺满块矩形。
#[gpui_kit::test]
fn block_color_is_written_at_the_block_end_and_painted_in_preview(cx: &mut TestAppContext) {
    let source = "正文\n\n第二段\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    // 没用过颜色菜单时「上次使用的颜色」是黄色背景
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            let id = e.units[0].id;
            e.select_block_by_id(id, window, cx);
        })
    });
    cx.simulate_keystrokes("cmd-shift-h");
    assert_eq!(text(&editor, cx), "正文 {.bg-yellow}\n\n第二段\n");
    // 已经是这个颜色：不产生第二次编辑
    cx.simulate_keystrokes("cmd-shift-h");
    assert_eq!(text(&editor, cx), "正文 {.bg-yellow}\n\n第二段\n");
    // 块选中态的底色压过块颜色；退回浏览态才看到块自己的底色
    cx.simulate_keystrokes("escape");
    let quads = quads_of(cx, content_color(aero_core::colors::Slot::Background, "yellow"));
    let first = quads.first().expect("上色的块必须有底色");
    assert!(
        quads.iter().all(|quad| quad.bounds == first.bounds),
        "只有上色的那一块有底色: {quads:?}"
    );
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), source);
    assert!(quads_of(cx, content_color(aero_core::colors::Slot::Background, "yellow")).is_empty());
}

/// 多块选中时对每一块各写一次，一个撤销组；文字色走另一组，与背景色叠加。
#[gpui_kit::test]
fn block_colors_cover_every_selected_block_in_one_undo_group(cx: &mut TestAppContext) {
    let source = "第一段\n\n# 标题\n\n- 列表项\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            let id = e.units[0].id;
            e.select_block_by_id(id, window, cx);
        })
    });
    cx.simulate_keystrokes("shift-down shift-down");
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| e.apply_color(aero_core::colors::Slot::Text, Some("red"), window, cx))
    });
    // 标题与 Pandoc 一致；列表项的属性挂在它的正文段落上（6.3.10）
    let red = "第一段 {.red}\n\n# 标题 {.red}\n\n- 列表项 {.red}\n";
    assert_eq!(text(&editor, cx), red);
    // 另一组颜色叠加，不覆盖文字色
    cx.update(|window, cx| {
        editor
            .update(cx, |e, cx| e.apply_color(aero_core::colors::Slot::Background, Some("blue"), window, cx))
    });
    let stacked = "第一段 {.red .bg-blue}\n\n# 标题 {.red .bg-blue}\n\n- 列表项 {.red .bg-blue}\n";
    assert_eq!(text(&editor, cx), stacked);
    // 一次撤销退掉整组
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), red);
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), source);
    // 「默认」去掉这一组：重做回 `{.red}`，再把三块一起去色
    cx.simulate_keystrokes("cmd-shift-z");
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            e.select_blocks(0, 2, window, cx);
            e.apply_color(aero_core::colors::Slot::Text, None, window, cx)
        })
    });
    assert_eq!(text(&editor, cx), source);
}

fn color_menu_open(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> bool {
    cx.update(|_, cx| editor.read(cx).color_menu.is_some())
}

/// 打开颜色菜单：画一帧，让焦点真的落到菜单的过滤输入框上（键盘事件按上一帧的树派发）。
fn open_color_menu(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| editor.update(cx, |e, cx| e.open_color_menu(window, cx)));
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
    assert!(color_menu_open(editor, cx), "颜色菜单没打开");
}

#[gpui_kit::test]
fn navigation_marks_preview_and_jump_without_entering_edit_mode(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, point, px, size};
    let source =
        format!("# 导航标题\n\n{}最后一段 **保留内容**。\n", "正文段落，保持编辑行为不变。\n\n".repeat(30));
    let (_dir, _vault, _path, editor) = open(cx, &source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.simulate_resize(size(px(1000.), px(700.)));
    settle(cx);
    let rail = cx.debug_bounds("document-navigator").expect("fixed right-edge navigation rail");
    let ids: Vec<_> = cx.update(|_, cx| editor.read(cx).units.iter().map(|u| u.id).collect());
    let last = *ids.last().unwrap();
    let marker = cx.debug_bounds(Box::leak(format!("navigation-mark-{}", last.0).into_boxed_str())).unwrap();
    cx.simulate_mouse_move(marker.center(), None, Modifiers::default());
    cx.executor().advance_clock(Duration::from_millis(250));
    settle(cx);
    let preview = cx.debug_bounds("navigation-preview").expect("hover preview");
    assert!(preview.right() <= rail.left(), "preview stays to the left of the rail");
    assert!(preview.top() >= px(0.) && preview.bottom() <= px(700.));
    assert_eq!(text(&editor, cx), source);
    cx.simulate_click(marker.center(), Modifiers::default());
    settle(cx);
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert!(e.focused.is_none(), "navigation must not enter source editing");
        assert_eq!(e.highlighted_block(), Some(last));
        assert!(e.scroll.offset().y < px(0.));
    });
    assert_eq!(cx.debug_bounds("document-navigator").unwrap(), rail, "rail is fixed while content scrolls");
    assert!(cx.debug_bounds("navigation-preview").is_none());
    cx.simulate_mouse_move(point(px(200.), px(100.)), None, Modifiers::default());
    settle(cx);
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn navigation_marks_magnify_around_the_pointer_then_settle_back(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, point, px};
    let source = format!("# 标题\n\n{}最后一段。\n", "段落。\n\n".repeat(11));
    let (_dir, _vault, _path, editor) = open(cx, &source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.simulate_resize(size_of_navigation());
    settle(cx);
    let ids: Vec<_> = cx.update(|_, cx| editor.read(cx).units.iter().map(|u| u.id).collect());
    let last = ids[ids.len() - 1];
    let prev = ids[ids.len() - 2];
    let rest = navigation_bar_width(cx, last.0);
    let neighbor_rest = navigation_bar_width(cx, prev.0);
    let rail = cx.debug_bounds("document-navigator").unwrap();
    let marker = cx.debug_bounds(Box::leak(format!("navigation-mark-{}", last.0).into_boxed_str())).unwrap();
    cx.simulate_mouse_move(marker.center(), None, Modifiers::default());
    // 悬停当帧只记下目标，弹簧要等下一帧才开始走
    settle(cx);
    assert!(
        (navigation_bar_width(cx, last.0) - rest).abs() < 1.,
        "magnification eases in instead of popping"
    );
    pump_frames(cx, 50);
    let mid = navigation_bar_width(cx, last.0);
    assert!(mid > rest + 1.5 && mid < rest + 12., "partway through the grow: {mid} from {rest}");
    for _ in 0..8 {
        pump_frames(cx, 50);
    }
    let grown = navigation_bar_width(cx, last.0);
    let neighbor = navigation_bar_width(cx, prev.0);
    assert!(grown > rest + 10., "hovered mark grows: {grown} from {rest}");
    assert!(
        neighbor > neighbor_rest + 3. && neighbor < grown - 2.,
        "neighbors grow less: {neighbor} between {neighbor_rest} and {grown}"
    );
    assert_eq!(cx.debug_bounds("document-navigator").unwrap().size, rail.size, "the rail itself stays put");
    cx.simulate_mouse_move(point(px(40.), px(40.)), None, Modifiers::default());
    settle(cx);
    for _ in 0..8 {
        pump_frames(cx, 50);
    }
    let back = navigation_bar_width(cx, last.0);
    assert!(back < rest + 1.5, "the mark shrinks back: {back} toward {rest}");
}

#[gpui_kit::test]
fn navigation_pins_into_an_outline_and_jumps_to_the_heading(cx: &mut TestAppContext) {
    use gpui_kit::Modifiers;
    let source = "# 甲\n\n正文\n\n## 乙\n\n更多\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.simulate_resize(size_of_navigation());
    settle(cx);
    let second = cx.update(|_, cx| editor.read(cx).headings()[1].2);
    let pin = cx.debug_bounds("navigation-pin").expect("pin control on the minimap");
    cx.simulate_click(pin.center(), Modifiers::default());
    settle(cx);
    assert!(cx.debug_bounds("navigation-outline").is_some(), "pinned ticks become an outline");
    assert!(cx.debug_bounds("document-navigator").is_none(), "the thumbnail rail steps aside");
    let row = cx
        .debug_bounds(Box::leak(format!("navigation-heading-{}", second.0).into_boxed_str()))
        .expect("heading row");
    cx.simulate_click(row.center(), Modifiers::default());
    settle(cx);
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert!(e.focused.is_none(), "jumping from the outline does not open source");
        assert_eq!(e.highlighted_block(), Some(second));
    });
    assert_eq!(text(&editor, cx), source);
    let pin = cx.debug_bounds("navigation-pin").unwrap();
    cx.simulate_click(pin.center(), Modifiers::default());
    settle(cx);
    assert!(cx.debug_bounds("navigation-outline").is_none());
    assert!(cx.debug_bounds("document-navigator").is_some());
}

fn size_of_navigation() -> gpui_kit::Size<gpui_kit::Pixels> {
    gpui_kit::size(gpui_kit::px(1000.), gpui_kit::px(700.))
}

fn navigation_bar_width(cx: &mut VisualTestContext, id: u64) -> f32 {
    let selector = Box::leak(format!("navigation-bar-{id}").into_boxed_str());
    f32::from(cx.debug_bounds(selector).expect("navigation bar").size.width)
}

fn pump_frames(cx: &mut VisualTestContext, ms: u64) {
    cx.executor().advance_clock(Duration::from_millis(ms));
    cx.update(|window, cx| window.draw(cx).clear(cx));
}

#[gpui_kit::test]
fn empty_document_has_no_navigation_marks(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    settle(cx);
    assert!(cx.debug_bounds("document-navigator").is_none());
    assert_eq!(text(&editor, cx), "");
}

#[gpui_kit::test]
fn block_handle_click_opens_menu_for_the_captured_block(cx: &mut TestAppContext) {
    let source =
        "# 标题\n\n正文\n\n- 列表项\n\n> 引用\n\n```rust\nlet x = 1;\n```\n\n| 表头 |\n| --- |\n| 单元格 |\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    let ids: Vec<_> = cx.update(|_, cx| editor.read(cx).units.iter().map(|u| u.id).collect());
    for id in ids {
        hover_block(id, cx);
        let selector = Box::leak(format!("gutter-handle-{}", id.0).into_boxed_str());
        let handle = cx.debug_bounds(selector).unwrap();
        cx.simulate_mouse_move(handle.center(), None, gpui_kit::Modifiers::default());
        cx.simulate_click(handle.center(), gpui_kit::Modifiers::default());
        settle(cx);
        cx.update(|_, cx| {
            let e = editor.read(cx);
            assert_eq!(e.block_menu.as_ref().map(|m| m.block), Some(id));
        });
        assert!(cx.debug_bounds("block-menu").is_some());
        assert_eq!(text(&editor, cx), source);
        cx.simulate_keystrokes("escape");
    }
}

#[gpui_kit::test]
fn block_menu_dismisses_on_outside_click_and_keeps_submenu_clicks(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, MouseButton, point, px};
    let source = "first\n\nsecond\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    settle(cx);
    let first_id = cx.update(|_, cx| editor.read(cx).units[0].id);
    let first = cx.debug_bounds(Box::leak(format!("unit-{}", first_id.0).into_boxed_str())).unwrap();
    let at = first.origin + point(px(8.), px(8.));
    cx.simulate_mouse_down(at, MouseButton::Right, Modifiers::default());
    cx.simulate_mouse_up(at, MouseButton::Right, Modifiers::default());
    settle(cx);
    assert!(cx.debug_bounds("block-menu").is_some());
    cx.update(|_, cx| assert_eq!(editor.read(cx).selected_blocks(), Some(0..1)));
    let convert = cx.debug_bounds("block-menu-convert").unwrap();
    cx.simulate_click(convert.center(), Modifiers::default());
    settle(cx);
    let submenu = cx.debug_bounds("block-menu-convert-list").unwrap();
    cx.simulate_click(submenu.origin + point(px(2.), px(2.)), Modifiers::default());
    settle(cx);
    assert!(cx.debug_bounds("block-menu").is_some(), "submenu padding is inside the menu");
    assert!(cx.debug_bounds("block-menu-convert-list").is_some());
    assert_eq!(text(&editor, cx), source);
    let menu = cx.debug_bounds("block-menu").unwrap();
    let outside = point(menu.left() + px(8.), submenu.bottom() - px(8.));
    assert!(!menu.contains(&outside) && !submenu.contains(&outside));
    cx.simulate_click(outside, Modifiers::default());
    settle(cx);
    cx.update(|_, cx| assert!(editor.read(cx).block_menu.is_none(), "outside click must close menu state"));
    assert!(cx.debug_bounds("block-menu").is_none());
    assert!(cx.debug_bounds("block-menu-convert-list").is_none());
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn block_menu_dismisses_when_an_outside_link_captures_the_click(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, MouseButton, point, px};
    let source = "[外部链接](https://example.com)\n\n正文\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    settle(cx);
    let ids: Vec<_> = cx.update(|_, cx| editor.read(cx).units.iter().map(|u| u.id).collect());
    let body = cx.debug_bounds(Box::leak(format!("unit-{}", ids[1].0).into_boxed_str())).unwrap();
    let at = body.origin + point(px(8.), px(8.));
    cx.simulate_mouse_down(at, MouseButton::Right, Modifiers::default());
    cx.simulate_mouse_up(at, MouseButton::Right, Modifiers::default());
    settle(cx);
    assert!(cx.debug_bounds("block-menu").is_some());
    let link = cx.debug_bounds(Box::leak(format!("unit-{}", ids[0].0).into_boxed_str())).unwrap();
    cx.simulate_click(link.origin + point(px(8.), px(8.)), Modifiers::default());
    settle(cx);
    cx.update(|_, cx| assert!(editor.read(cx).block_menu.is_none()));
    assert!(cx.debug_bounds("block-menu").is_none());
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn block_menu_right_click_retargets_and_outside_click_reaches_content(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, MouseButton, point, px};
    let source = "first\n\nsecond\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    settle(cx);
    let ids: Vec<_> = cx.update(|_, cx| editor.read(cx).units.iter().map(|u| u.id).collect());
    for (ix, id) in ids.iter().enumerate() {
        let bounds = cx.debug_bounds(Box::leak(format!("unit-{}", id.0).into_boxed_str())).unwrap();
        let at = if ix == 0 {
            bounds.origin + point(px(8.), px(8.))
        } else {
            point(bounds.right() - px(8.), bounds.center().y)
        };
        cx.simulate_mouse_down(at, MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(at, MouseButton::Right, Modifiers::default());
        settle(cx);
        assert!(cx.debug_bounds("block-menu").is_some());
        cx.update(|_, cx| assert_eq!(editor.read(cx).selected_blocks(), Some(ix..ix + 1)));
    }
    let copy = cx.debug_bounds("block-menu-copy").unwrap();
    cx.simulate_click(copy.center(), Modifiers::default());
    settle(cx);
    assert_eq!(text(&editor, cx), source);
    cx.update(|_, cx| assert_eq!(cx.read_from_clipboard().and_then(|c| c.text()).as_deref(), Some("second")));
    assert!(cx.debug_bounds("block-menu").is_none());
    cx.simulate_keystrokes("escape");
    cx.simulate_keystrokes("cmd-z");
    settle(cx);
    assert_eq!(text(&editor, cx), source);
    let first = cx.debug_bounds(Box::leak(format!("unit-{}", ids[0].0).into_boxed_str())).unwrap();
    let menu_at = point(first.right() - px(8.), first.center().y);
    cx.simulate_mouse_down(menu_at, MouseButton::Right, Modifiers::default());
    cx.simulate_mouse_up(menu_at, MouseButton::Right, Modifiers::default());
    settle(cx);
    let second = cx.debug_bounds(Box::leak(format!("unit-{}", ids[1].0).into_boxed_str())).unwrap();
    cx.simulate_click(second.origin + point(px(8.), px(8.)), Modifiers::default());
    settle(cx);
    assert!(cx.debug_bounds("block-menu").is_none());
    cx.update(|_, cx| assert_eq!(editor.read(cx).focused.as_ref().map(|f| f.id), Some(Some(ids[1]))));
    assert_eq!(text(&editor, cx), source);
}

/// `⌘/` 打开块菜单，从「颜色 ▸」进入颜色菜单。
#[gpui_kit::test]
fn block_menu_opens_with_command_slash_and_reaches_colors(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "段落\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_keystrokes("cmd-/");
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
    assert!(cx.debug_bounds("block-menu").is_some(), "⌘/ 打开块菜单");
    assert!(!color_menu_open(&editor, cx));
    let color = cx.debug_bounds("block-menu-color").unwrap();
    cx.simulate_click(color.center(), gpui_kit::Modifiers::default());
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
    assert!(color_menu_open(&editor, cx), "颜色 ▸ 打开颜色菜单");
}

#[gpui_kit::test]
fn enter_places_an_empty_row_between_container_children(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, ":::highlight\nfirst {.red}\n\nsecond\n:::\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    settle(cx);
    let (first, second) = cx.update(|_, cx| {
        (editor.read(cx).units[0].id, editor.read(cx).units[1].id)
    });
    focus_unit(&editor, 0, cx);
    cx.simulate_keystrokes("enter escape");
    settle(cx);
    let offset = cx.update(|_, cx| editor.read(cx).empty_slots[0].range.start);
    let empty = cx.debug_bounds(Box::leak(format!("empty-paragraph-{offset}").into_boxed_str())).unwrap();
    let first = cx.debug_bounds(Box::leak(format!("unit-{first}", first = first.0).into_boxed_str())).unwrap();
    let second = cx.debug_bounds(Box::leak(format!("unit-{second}", second = second.0).into_boxed_str())).unwrap();
    assert!(first.bottom() <= empty.top(), "空行应在第一段之后: {first:?} {empty:?}");
    assert!(empty.bottom() <= second.top(), "空行不能排到容器或第二段之后: {empty:?} {second:?}");
}

#[gpui_kit::test]
fn trailing_empty_row_stays_inside_its_quote(cx: &mut TestAppContext) {
    for source in ["> first\n>\n> second {.red}\n", ":::highlight\n> first\n>\n> second {.red}\n:::\n"] {
        let (_dir, _vault, _path, editor) = open(cx, source);
        let cx = &mut VisualTestContext::from_window(*cx.windows().last().unwrap(), cx);
        settle(cx);
        let (second, quote) = cx.update(|_, cx| {
            let e = editor.read(cx);
            let quote = e
                .doc
                .blocks()
                .iter()
                .find(|(_, block)| matches!(block.kind, aero_core::BlockKind::BlockQuote))
                .map(|(_, block)| block.id)
                .unwrap();
            (e.units[1].id, quote)
        });
        focus_unit(&editor, 1, cx);
        cx.simulate_keystrokes("enter escape");
        settle(cx);
        let offset = cx.update(|_, cx| editor.read(cx).empty_slots[0].range.start);
        let empty = cx.debug_bounds(Box::leak(format!("empty-paragraph-{offset}").into_boxed_str())).unwrap();
        let second = cx.debug_bounds(Box::leak(format!("unit-{}", second.0).into_boxed_str())).unwrap();
        let quote = cx.debug_bounds(Box::leak(format!("block-body-{}", quote.0).into_boxed_str())).unwrap();
        assert!(second.bottom() <= empty.top(), "空行应在引用正文之后: {second:?} {empty:?}");
        assert!(empty.bottom() <= quote.bottom(), "空行不能落到引用框外: {empty:?} {quote:?}");
    }
}

#[gpui_kit::test]
fn identified_container_unwrap_is_disabled_with_an_explanation(cx: &mut TestAppContext) {
    for marker in ["^box\n", "{aero-aliases=old}\n"] {
        let source = format!(":::highlight\nbody\n:::\n\n{marker}");
        let (_dir, _vault, _path, editor) = open(cx, &source);
        let cx = &mut VisualTestContext::from_window(*cx.windows().last().unwrap(), cx);
        cx.update(|window, cx| {
            editor.update(cx, |e, cx| {
                let id = e.doc.blocks().roots[0].id;
                e.open_block_menu(id, gpui_kit::point(gpui_kit::px(20.), gpui_kit::px(40.)), window, cx);
            })
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let unwrap = cx.debug_bounds("block-menu-unwrap").unwrap();
        assert!(cx.debug_bounds("block-menu-unwrap-disabled-reason").is_some());
        cx.simulate_click(unwrap.center(), gpui_kit::Modifiers::default());
        settle(cx);
        assert_eq!(text(&editor, cx), source);
        cx.update(|_, cx| assert!(editor.read(cx).block_menu.is_some(), "禁用入口不能执行解除或关闭菜单"));
    }
}

#[gpui_kit::test]
fn unidentified_container_unwrap_remains_enabled(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, ":::highlight\nbody\n:::\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            let id = e.doc.blocks().roots[0].id;
            e.open_block_menu(id, gpui_kit::point(gpui_kit::px(20.), gpui_kit::px(40.)), window, cx);
        })
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("block-menu-unwrap-disabled-reason").is_none());
    let unwrap = cx.debug_bounds("block-menu-unwrap").unwrap();
    cx.simulate_click(unwrap.center(), gpui_kit::Modifiers::default());
    settle(cx);
    assert_eq!(text(&editor, cx), "body\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), ":::highlight\nbody\n:::\n");
}

/// 拖动 `⠿` 把块插到另一块前面，并画出插入线。点击手柄只选中块。
#[gpui_kit::test]
fn dragging_a_block_handle_reorders_it(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "aaa\n\nbbb\n\nccc\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let draw = |cx: &mut VisualTestContext| cx.update(|window, cx| window.draw(cx).clear(cx));
    let selector = |name: &str, id: aero_core::BlockId| -> &'static str {
        Box::leak(format!("{name}-{}", id.0).into_boxed_str())
    };
    draw(cx);
    let ids: Vec<aero_core::BlockId> =
        cx.update(|_, cx| editor.read(cx).units.iter().map(|u| u.id).collect());
    hover_block(ids[2], cx);
    let handle = cx.debug_bounds(selector("gutter-handle", ids[2])).expect("第三块的拖动手柄");
    let target = cx.debug_bounds(selector("unit", ids[0])).expect("第一块");
    let drop_at = target.origin + gpui_kit::point(gpui_kit::px(24.), gpui_kit::px(4.));
    cx.simulate_mouse_move(handle.center(), None, gpui_kit::Modifiers::default());
    cx.simulate_mouse_down(handle.center(), gpui_kit::MouseButton::Left, gpui_kit::Modifiers::default());
    cx.simulate_mouse_move(
        handle.center() + gpui_kit::point(gpui_kit::px(12.), gpui_kit::px(0.)),
        gpui_kit::MouseButton::Left,
        gpui_kit::Modifiers::default(),
    );
    cx.simulate_mouse_move(drop_at, gpui_kit::MouseButton::Left, gpui_kit::Modifiers::default());
    draw(cx);
    assert!(cx.debug_bounds("block-drop-line").is_some(), "拖动时显示插入线");
    cx.simulate_mouse_up(drop_at, gpui_kit::MouseButton::Left, gpui_kit::Modifiers::default());
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "ccc\n\naaa\n\nbbb\n");
    cx.update(|_, cx| {
        assert!(editor.read(cx).block_menu.is_none(), "拖完不打开块菜单");
        assert_eq!(editor.read(cx).highlighted_block(), Some(editor.read(cx).units[0].id));
    });
}

#[gpui_kit::test]
fn dragging_into_a_highlight_shows_a_distinct_into_indicator(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, MouseButton, point, px};
    let source = "first\n\n:::highlight\ninside\n:::\n\nlast\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    settle(cx);
    let (first, highlight) = cx.update(|_, cx| {
        let roots = &editor.read(cx).doc.blocks().roots;
        (roots[0].id, roots[1].id)
    });
    hover_block(first, cx);
    let handle = Box::leak(format!("gutter-handle-{}", first.0).into_boxed_str());
    let body = Box::leak(format!("block-body-{}", highlight.0).into_boxed_str());
    let from = cx.debug_bounds(handle).unwrap().center();
    let body = cx.debug_bounds(body).unwrap();
    let to = body.origin + point(px(4.), body.size.height / 2.);
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(from + point(px(12.), px(0.)), MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::default());
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("block-drop-line").is_some());
    assert!(cx.debug_bounds("block-drop-into").is_some(), "Into 必须有区别于同级插入线的反馈");
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
    settle(cx);
    assert_eq!(text(&editor, cx), ":::highlight\ninside\n\nfirst\n:::\n\nlast\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn dragging_an_empty_highlight_moves_its_fences(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, MouseButton, point, px};
    let source = "first\n\n:::highlight\n:::\n\nlast\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    settle(cx);
    let (first, highlight) = cx.update(|_, cx| {
        let roots = &editor.read(cx).doc.blocks().roots;
        (roots[0].id, roots[1].id)
    });
    hover_block(highlight, cx);
    let handle = Box::leak(format!("gutter-handle-{}", highlight.0).into_boxed_str());
    let target = Box::leak(format!("unit-{}", first.0).into_boxed_str());
    let from = cx.debug_bounds(handle).unwrap().center();
    let to = cx.debug_bounds(target).unwrap().origin + point(px(24.), px(4.));
    cx.simulate_mouse_move(from, None, Modifiers::default());
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(from + point(px(12.), px(0.)), MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
    settle(cx);
    assert_eq!(text(&editor, cx), ":::highlight\n:::\n\nfirst\n\nlast\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), source);
}

/// 拖到列表项右侧 24 px 时，块缩进成该项的子项。
#[gpui_kit::test]
fn dragging_onto_the_right_edge_nests_under_the_list_item(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "- a\n- b\n\npara\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let draw = |cx: &mut VisualTestContext| cx.update(|window, cx| window.draw(cx).clear(cx));
    let selector = |name: &str, id: aero_core::BlockId| -> &'static str {
        Box::leak(format!("{name}-{}", id.0).into_boxed_str())
    };
    draw(cx);
    let ids: Vec<aero_core::BlockId> =
        cx.update(|_, cx| editor.read(cx).units.iter().map(|u| u.id).collect());
    hover_block(ids[2], cx);
    let handle = cx.debug_bounds(selector("gutter-handle", ids[2])).unwrap();
    let item = cx.debug_bounds(selector("unit", ids[0])).unwrap();
    let drop_at = gpui_kit::point(item.right() - gpui_kit::px(8.), item.origin.y + gpui_kit::px(4.));
    cx.simulate_mouse_down(handle.center(), gpui_kit::MouseButton::Left, gpui_kit::Modifiers::default());
    cx.simulate_mouse_move(
        handle.center() + gpui_kit::point(gpui_kit::px(12.), gpui_kit::px(0.)),
        gpui_kit::MouseButton::Left,
        gpui_kit::Modifiers::default(),
    );
    cx.simulate_mouse_move(drop_at, gpui_kit::MouseButton::Left, gpui_kit::Modifiers::default());
    cx.simulate_mouse_up(drop_at, gpui_kit::MouseButton::Left, gpui_kit::Modifiers::default());
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "- a\n  - para\n- b\n");
}

/// `Esc` 取消拖拽，松手不再改文档。
#[gpui_kit::test]
fn escape_cancels_a_block_drag(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "aaa\n\nbbb\n\nccc\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let draw = |cx: &mut VisualTestContext| cx.update(|window, cx| window.draw(cx).clear(cx));
    let selector = |name: &str, id: aero_core::BlockId| -> &'static str {
        Box::leak(format!("{name}-{}", id.0).into_boxed_str())
    };
    draw(cx);
    let ids: Vec<aero_core::BlockId> =
        cx.update(|_, cx| editor.read(cx).units.iter().map(|u| u.id).collect());
    hover_block(ids[2], cx);
    let handle = cx.debug_bounds(selector("gutter-handle", ids[2])).unwrap();
    let target = cx.debug_bounds(selector("unit", ids[0])).unwrap();
    let drop_at = target.origin + gpui_kit::point(gpui_kit::px(24.), gpui_kit::px(4.));
    cx.update(|window, cx| {
        editor.update(cx, |editor, cx| editor.focus_handle.focus(window, cx));
    });
    cx.simulate_mouse_down(handle.center(), gpui_kit::MouseButton::Left, gpui_kit::Modifiers::default());
    cx.simulate_mouse_move(drop_at, gpui_kit::MouseButton::Left, gpui_kit::Modifiers::default());
    cx.simulate_keystrokes("escape");
    draw(cx);
    assert!(cx.debug_bounds("block-drop-line").is_none(), "Esc 去掉插入线");
    cx.simulate_mouse_up(drop_at, gpui_kit::MouseButton::Left, gpui_kit::Modifiers::default());
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "aaa\n\nbbb\n\nccc\n");
}

/// 颜色菜单（7.7）：`⌘/` 打开，输入过滤，⏎ 应用到选中的文字，再打开时当前颜色有选中标记。
#[gpui_kit::test]
fn color_menu_filters_and_applies_to_the_selection(cx: &mut TestAppContext) {
    let source = "正文 中文😀 后文\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    select_inline_text(&editor, "中文😀", cx);
    open_color_menu(&editor, cx);
    cx.update(|_, cx| {
        let e = editor.read(cx);
        // 两组 + 默认，一共 20 项；默认选中第一项
        assert_eq!(e.color_menu_names().len(), 20);
        assert_eq!(e.color_menu_selected().as_deref(), Some("默认颜色"));
        assert_eq!(e.color_menu_current(), [None, None]);
    });
    // ⌘/ 是开关：再按一次关掉
    cx.simulate_keystrokes("cmd-/");
    assert!(!color_menu_open(&editor, cx), "⌘/ 再按一次关掉菜单");
    open_color_menu(&editor, cx);
    // 直接输入过滤：中文命中两组的蓝
    cx.simulate_input("蓝");
    cx.update(|_, cx| {
        assert_eq!(editor.read(cx).color_menu_names(), ["蓝色", "蓝色背景"]);
        assert_eq!(editor.read(cx).color_menu_selected().as_deref(), Some("蓝色"));
    });
    // ↓ 换行（过滤后两组各一行），⏎ 应用
    cx.simulate_keystrokes("down");
    cx.update(|_, cx| assert_eq!(editor.read(cx).color_menu_selected().as_deref(), Some("蓝色背景")));
    cx.simulate_keystrokes("up enter");
    assert!(!color_menu_open(&editor, cx), "确认后菜单关掉");
    assert_eq!(text(&editor, cx), "正文 [中文😀]{.blue} 后文\n");
    assert_eq!(selected_inline_text(&editor, cx), "中文😀", "焦点与选区回到正文");
    // 记住了这次的选择：⌘⇧H 接着用它
    cx.update(|_, cx| {
        let last = crate::colors::last(cx);
        assert_eq!((last.slot, last.color.as_str()), (aero_core::colors::Slot::Text, "blue"));
    });
    // 再打开：当前颜色带选中标记；Esc 关掉且不改文档
    open_color_menu(&editor, cx);
    cx.update(|_, cx| {
        assert_eq!(editor.read(cx).color_menu_current(), [Some("blue".into()), None]);
    });
    cx.simulate_keystrokes("escape");
    assert!(!color_menu_open(&editor, cx));
    assert_eq!(text(&editor, cx), "正文 [中文😀]{.blue} 后文\n");
    assert!(cx.update(|_, cx| editor.read(cx).is_editing()), "Esc 只关菜单，还在编辑态");
}

/// 选区盖住整块时，颜色菜单可以改去给块上色，而不是包一层行内属性。
#[gpui_kit::test]
fn color_menu_can_apply_a_full_selection_to_the_whole_block(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "第一段\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    select_inline_text(&editor, "第一段", cx);
    open_color_menu(&editor, cx);
    let switch = cx.debug_bounds("color-apply-block").expect("盖住整块时应有「应用到整块」");
    cx.simulate_click(switch.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    cx.simulate_input("蓝");
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "第一段 {.blue}\n");
}

#[gpui_kit::test]
fn color_menu_whole_block_preview_uses_pending_color(cx: &mut TestAppContext) {
    let source = "第一段\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    select_inline_text(&editor, "第一段", cx);
    open_color_menu(&editor, cx);
    let switch = cx.debug_bounds("color-apply-block").unwrap();
    cx.simulate_click(switch.center(), gpui_kit::Modifiers::default());
    cx.simulate_input("rgba(38,154,109,0.374900)");
    cx.update(|_, cx| {
        let e = editor.read(cx);
        let id = e.doc.blocks().roots[0].id;
        let attrs = e.preview_attrs(id, &Default::default());
        assert_eq!(aero_core::colors::Slot::Background.color_of(&attrs), Some("rgba(38,154,109,0.374900)"));
    });
    assert_eq!(text(&editor, cx), source);
}

/// 编辑态没有选区时菜单改当前块；块选中态一次改选中的每一块（7.7）。
#[gpui_kit::test]
fn color_menu_colors_blocks_when_no_text_is_selected(cx: &mut TestAppContext) {
    let source = "第一段\n\n第二段\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    open_color_menu(&editor, cx);
    cx.simulate_input("bg");
    cx.update(|_, cx| {
        // "bg" 整组命中背景色
        assert_eq!(editor.read(cx).color_menu_names().len(), 10);
        assert_eq!(editor.read(cx).color_menu_selected().as_deref(), Some("默认背景"));
    });
    // → 走到灰色背景再确认
    cx.simulate_keystrokes("right enter");
    assert_eq!(text(&editor, cx), "第一段 {.bg-gray}\n\n第二段\n");
    cx.update(|_, cx| {
        let e = editor.read(cx);
        let f = e.focused.as_ref().expect("还在编辑态");
        assert_eq!(f.textarea.read(cx).value(), "第一段", "块属性是尾部标记，文本框里看不到");
    });
    // 已经是这个颜色：再打开时它带选中标记
    open_color_menu(&editor, cx);
    cx.update(|_, cx| assert_eq!(editor.read(cx).color_menu_current(), [None, Some("gray".into())]));
    cx.simulate_keystrokes("escape");
    // 块选中态：两块一起上色，一次撤销退掉
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            e.select_blocks(0, 1, window, cx);
        })
    });
    open_color_menu(&editor, cx);
    cx.simulate_input("红色");
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "第一段 {.bg-gray .red}\n\n第二段 {.red}\n");
    assert_eq!(cx.update(|_, cx| editor.read(cx).selected_blocks()), Some(0..2), "选区还在");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "第一段 {.bg-gray}\n\n第二段\n");
}

#[gpui_kit::test]
fn color_menu_warns_about_invalid_existing_values(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "段落 {background=\"#bad\"}\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    open_color_menu(&editor, cx);
    assert!(cx.debug_bounds("color-invalid-value-#bad").is_some(), "块旧无效色值应提示");
    cx.simulate_keystrokes("escape");

    let (_dir2, _vault2, _path2, editor) =
        open(cx, "第一段{background=\"rgba(1,2,3,1.5)\"} {background=\"#bad\"}\n");
    let cx = &mut VisualTestContext::from_window(*cx.windows().last().unwrap(), cx);
    focus_unit(&editor, 0, cx);
    select_inline_text(&editor, "第一段", cx);
    open_color_menu(&editor, cx);
    assert!(cx.debug_bounds("color-apply-block").is_some(), "带属性全段也应能切换整块");
    assert!(cx.debug_bounds("color-invalid-value-rgba(1,2,3,1.5)").is_some(), "行内旧无效色值应提示");
    let switch = cx.debug_bounds("color-apply-block").unwrap();
    cx.simulate_click(switch.center(), gpui_kit::Modifiers::default());
    assert!(cx.debug_bounds("color-invalid-value-#bad").is_some(), "切换整块后应显示目标块旧无效值");
    assert!(cx.debug_bounds("color-invalid-value-rgba(1,2,3,1.5)").is_none());

    cx.simulate_keystrokes("escape");
    focus_unit(&editor, 0, cx);
    cx.update(|_, cx| {
        let input = editor.read(cx).focused.as_ref().unwrap().textarea.clone();
        let len = input.read(cx).value().len();
        input.update(cx, |ta, cx| ta.set_selected_range(0..len, cx));
    });
    open_color_menu(&editor, cx);
    assert!(cx.debug_bounds("color-apply-block").is_some(), "全选含属性源码也应能切换整块");
}

#[gpui_kit::test]
fn picker_color_serialization_preserves_source_format_and_alpha_precision(_cx: &mut TestAppContext) {
    let color = |source| {
        let parsed = aero_core::colors::CustomColor::parse(source).unwrap();
        gpui_kit::Hsla::from(gpui_kit::Rgba {
            r: parsed.red as f32 / 255.,
            g: parsed.green as f32 / 255.,
            b: parsed.blue as f32 / 255.,
            a: parsed.alpha,
        })
    };
    let rgba = "rgba(38,154,109,0.374900)";
    assert_eq!(Editor::serialize_picker_color(color(rgba), Some(rgba)), rgba);
    assert_eq!(Editor::serialize_picker_color(color("#269A6D80"), Some("#269A6D80")), "#269A6D80");

    let changed = gpui_kit::Hsla::from(gpui_kit::Rgba { r: 1., g: 0.5, b: 0.25, a: 0.3749 });
    assert_eq!(Editor::serialize_picker_color(changed, Some(rgba)), "rgba(255,128,64,0.374900)");
    let changed_hex = gpui_kit::Hsla::from(gpui_kit::Rgba { r: 1., g: 0.5, b: 0.25, a: 97. / 255. });
    assert_eq!(Editor::serialize_picker_color(changed_hex, Some("#00000061")), "#FF804061");

    assert_eq!(
        Editor::serialize_picker_color(changed, Some("rgba(38,154,109,0.374900) \u{2003}")),
        "rgba(255,128,64,0.374900)"
    );
    assert_eq!(
        Editor::serialize_picker_color(changed, Some("rgba(38,154,109,0.374900) ")),
        "rgba(255,128,64,0.374900)"
    );
    assert_eq!(Editor::serialize_picker_color(changed, Some("#00000000")), "#FF804060");
    let opaque = gpui_kit::Hsla::from(gpui_kit::Rgba { r: 1., g: 0.5, b: 0.25, a: 1. });
    assert_eq!(Editor::serialize_picker_color(opaque, Some("rgba(0,0,0,1)")), "rgba(255,128,64,1)");
}

/// `/` 菜单里的颜色（7.1）：`/颜色` 开菜单，`/蓝色背景` 直接给当前块上色。
#[gpui_kit::test]
fn slash_colors_hit_directly_or_open_the_menu(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "段落\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_keystrokes("end");
    cx.simulate_input(" /蓝色背景");
    cx.update(|_, cx| {
        let m = editor.read(cx).slash.as_ref().expect("菜单开着");
        assert_eq!(m.matches.len(), 1, "颜色是直接命中");
    });
    cx.simulate_keystrokes("enter");
    // 命令本身删掉，属性写到块末
    assert_eq!(text(&editor, cx), "段落 {.bg-blue}\n");
    assert!(!color_menu_open(&editor, cx));
    // `/颜色` 打开菜单
    cx.simulate_input(" /颜色");
    cx.simulate_keystrokes("enter");
    assert!(color_menu_open(&editor, cx), "/颜色 打开颜色菜单");
    assert_eq!(text(&editor, cx), "段落 {.bg-blue}\n", "命令文字不留在文档里");
    cx.update(|_, cx| assert_eq!(editor.read(cx).color_menu_current(), [None, Some("blue".into())]));
    cx.simulate_keystrokes("escape");
    assert!(!color_menu_open(&editor, cx));
}

fn toolbar_open(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> bool {
    cx.update(|_, cx| editor.read(cx).toolbar_open())
}

/// 选区停 300 ms 后工具条才出现；程序化改选区走这条路，不走鼠标松手。
fn appear_toolbar(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.executor().advance_clock(Duration::from_millis(300));
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(toolbar_open(editor, cx), "选区停 300 ms 后工具条该出现");
    assert!(cx.debug_bounds("selection-toolbar").is_some(), "工具条要画出来");
}

fn click_toolbar(id: &'static str, cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let bounds = cx.debug_bounds(id).unwrap_or_else(|| panic!("missing {id}"));
    cx.simulate_click(bounds.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
}

/// `⋯`：给当前块补上 `^id` 并复制 `[[笔记#^id]]`。已有 id 时不重复写。
#[gpui_kit::test]
fn toolbar_more_menu_copies_a_block_link_and_adds_an_id(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "正文\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    select_inline_text(&editor, "正文", cx);
    appear_toolbar(&editor, cx);
    let more = cx.debug_bounds("toolbar-more").expect("toolbar-more");
    cx.simulate_click(more.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.update(|_, cx| editor.read(cx).toolbar_more_open()));
    let copy = cx.debug_bounds("toolbar-copy-link").expect("toolbar-copy-link");
    cx.simulate_click(copy.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    let body = text(&editor, cx);
    let id = body.split_once("^").expect(&body).1.trim();
    let copied = cx.update(|_, cx| {
        let item = cx.read_from_clipboard().expect("clipboard");
        item.text().unwrap_or_default()
    });
    let document = aero_core::identity::read(&aero_core::Document::from_text(&body)).unwrap().id.unwrap();
    assert_eq!(copied, format!("[[id:{document}#^{id}]]"));
    assert_eq!(body.matches('^').count(), 1, "只写一个块 id：{body}");
}

#[gpui_kit::test]
fn copying_a_sync_reference_writes_one_undoable_source_identity(cx: &mut TestAppContext) {
    let original = "source body\n";
    let (_dir, _vault, _path, editor) = open(cx, original);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            let id = e.units[0].id;
            e.open_block_menu(id, gpui_kit::point(gpui_kit::px(20.), gpui_kit::px(40.)), window, cx);
        });
    });
    settle(cx);
    let copy = cx.debug_bounds("block-menu-sync-copy").unwrap();
    cx.simulate_click(copy.center(), Default::default());
    cx.run_until_parked();
    cx.update(|_, cx| {
        let doc = &editor.read(cx).doc;
        let document = aero_core::identity::read(doc).unwrap().id.unwrap();
        let block = doc.blocks().roots[1].persisted_id.as_deref().unwrap();
        let item = cx.read_from_clipboard().unwrap();
        let expected = format!("![[id:{document}#^{block}]]");
        assert_eq!(item.text().as_deref(), Some(expected.as_str()));
        let payload: aero_core::clipboard::Payload = serde_json::from_str(item.metadata().unwrap()).unwrap();
        assert_eq!(payload.document.as_deref(), Some(document.as_str()));
        assert!(doc.to_string().contains(&format!("source body ^{block}")));
    });
    focus_unit(&editor, 0, cx);
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), original);
}

#[gpui_kit::test]
fn enter_in_the_sync_picker_confirms_the_source_without_splitting_the_document(cx: &mut TestAppContext) {
    let original = "Before after.\n";
    let (dir, _vault, _path, editor) = open(cx, original);
    std::fs::write(dir.path().join("Origin.md"), "sync source\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let requests = std::rc::Rc::new(std::cell::Cell::new(0));
    let _subscription = cx.update(|_, cx| {
        let requests = requests.clone();
        cx.subscribe(&editor, move |_, event, _| {
            if matches!(event, super::EditorEvent::TransactionRequested(_)) {
                requests.set(requests.get() + 1);
            }
        })
    });
    focus_unit(&editor, 0, cx);
    cx.update(|window, cx| editor.update(cx, |e, cx| e.open_sync_picker(7..7, false, window, cx)));
    settle(cx);
    cx.simulate_input("Origin");
    cx.simulate_keystrokes("enter");
    settle(cx);
    assert_eq!(text(&editor, cx), original);
    assert_eq!(requests.get(), 0);
    cx.simulate_keystrokes("enter");
    settle(cx);
    assert_eq!(requests.get(), 1);
    assert_eq!(text(&editor, cx), original);
}

/// 选区浮动工具条（5.9）：键盘选区等 300 ms 出现，点粗体加 / 去格式，按下态跟着变。
#[gpui_kit::test]
fn selection_toolbar_appears_after_pause_and_toggles_bold(cx: &mut TestAppContext) {
    let source = "正文 中文😀 后文\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    select_inline_text(&editor, "中文😀", cx);
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(!toolbar_open(&editor, cx), "选区刚停下来时还没有工具条");
    assert!(cx.debug_bounds("selection-toolbar").is_none());
    appear_toolbar(&editor, cx);
    cx.update(|_, cx| assert_eq!(editor.read(cx).toolbar_active(), Some(Vec::new())));
    click_toolbar("toolbar-bold", cx);
    assert_eq!(text(&editor, cx), "正文 **中文😀** 后文\n");
    assert_eq!(selected_inline_text(&editor, cx), "中文😀");
    assert!(toolbar_open(&editor, cx), "加格式后工具条留着");
    cx.update(|_, cx| {
        assert_eq!(editor.read(cx).toolbar_active(), Some(vec![aero_core::inline_format::Format::Bold]))
    });
    click_toolbar("toolbar-bold", cx);
    assert_eq!(text(&editor, cx), source);
    cx.update(|_, cx| assert_eq!(editor.read(cx).toolbar_active(), Some(Vec::new())));
}

/// 工具条 `A ▾` 打开颜色菜单；Esc 先关菜单、再关工具条、再退到块选中态。
#[gpui_kit::test]
fn selection_toolbar_opens_color_menu_and_escapes_in_layers(cx: &mut TestAppContext) {
    let source = "正文 中文😀 后文\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    select_inline_text(&editor, "中文😀", cx);
    appear_toolbar(&editor, cx);
    click_toolbar("toolbar-color", cx);
    assert!(color_menu_open(&editor, cx), "A ▾ 打开颜色菜单");
    assert!(toolbar_open(&editor, cx), "子菜单开着时工具条不消失");
    cx.simulate_input("蓝");
    cx.simulate_keystrokes("enter");
    assert!(!color_menu_open(&editor, cx));
    assert_eq!(text(&editor, cx), "正文 [中文😀]{.blue} 后文\n");
    assert_eq!(selected_inline_text(&editor, cx), "中文😀");
    assert!(toolbar_open(&editor, cx), "上色后工具条留着");
    cx.update(|_, cx| assert!(editor.read(cx).toolbar_colored(), "有颜色时 A ▾ 按下"));
    cx.simulate_keystrokes("escape");
    assert!(!toolbar_open(&editor, cx), "Esc 先收工具条");
    assert!(cx.update(|_, cx| editor.read(cx).is_editing()), "还在编辑态");
    assert_eq!(text(&editor, cx), "正文 [中文😀]{.blue} 后文\n");
    cx.simulate_keystrokes("escape");
    assert!(!cx.update(|_, cx| editor.read(cx).is_editing()), "再 Esc 才进块选中态");
}

/// 选区消失或块选中态都不画工具条；鼠标松手立刻出现，不等 300 ms。
#[gpui_kit::test]
fn selection_toolbar_hides_without_selection_and_shows_on_mouse_up(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "第一段\n\n第二段\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    select_inline_text(&editor, "第一段", cx);
    appear_toolbar(&editor, cx);
    cx.simulate_keystrokes("right");
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(!toolbar_open(&editor, cx), "选区没了工具条立刻收掉");
    select_inline_text(&editor, "第一段", cx);
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(!toolbar_open(&editor, cx), "还没松手 / 还没等到 300 ms");
    // 鼠标松手走 `show_toolbar_now`：立刻出现，不走 300 ms 那条计时
    cx.update(|_, cx| editor.update(cx, |e, cx| e.show_toolbar_now(cx)));
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(toolbar_open(&editor, cx), "鼠标松手立刻出现");
    cx.update(|window, cx| editor.update(cx, |e, cx| e.select_blocks(0, 1, window, cx)));
    cx.executor().advance_clock(Duration::from_millis(300));
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(!toolbar_open(&editor, cx), "块选中态不出现工具条");
}

/// `转换 ▾` 用现有的块类型转换；当前类型按下。
#[gpui_kit::test]
fn selection_toolbar_converts_the_current_block(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "一段话\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    select_inline_text(&editor, "一段话", cx);
    appear_toolbar(&editor, cx);
    click_toolbar("toolbar-convert", cx);
    assert!(cx.update(|_, cx| editor.read(cx).toolbar_convert_open()));
    assert!(cx.debug_bounds("toolbar-convert-menu").is_some());
    click_toolbar("toolbar-convert-item-1", cx);
    assert_eq!(text(&editor, cx), "# 一段话\n");
    assert!(cx.update(|_, cx| editor.read(cx).is_editing()), "转换后还在编辑态");
}

/// 行内颜色（7.7）：`⌘⇧H` 用上次的颜色包住选区，选区保持在原文字上，预览里上色。
#[gpui_kit::test]
fn inline_color_wraps_the_selection_and_paints_it_in_preview(cx: &mut TestAppContext) {
    let source = "正文 中文😀 后文\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|_, cx| crate::colors::remember(aero_core::colors::Slot::Background, "blue", cx));
    focus_unit(&editor, 0, cx);
    select_inline_text(&editor, "中文😀", cx);
    cx.simulate_keystrokes("cmd-shift-h");
    assert_eq!(text(&editor, cx), "正文 [中文😀]{.bg-blue} 后文\n");
    assert_eq!(selected_inline_text(&editor, cx), "中文😀", "选区还在原来那段文字上");
    // 预览里 `[文本]{...}` 的标记不显示，颜色画成行内背景
    cx.simulate_keystrokes("escape");
    let blue = content_color(aero_core::colors::Slot::Background, "blue");
    assert!(!quads_of(cx, blue).is_empty(), "预览要给行内背景上色");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), source);
    assert!(quads_of(cx, blue).is_empty());
    // 文字色是前景色，不铺底色（换一组颜色时不能画成背景）
    cx.update(|_, cx| crate::colors::remember(aero_core::colors::Slot::Text, "red", cx));
    focus_unit(&editor, 0, cx);
    select_inline_text(&editor, "中文😀", cx);
    cx.simulate_keystrokes("cmd-shift-h");
    assert_eq!(text(&editor, cx), "正文 [中文😀]{.red} 后文\n");
    cx.simulate_keystrokes("escape");
    assert!(
        quads_of(cx, content_color(aero_core::colors::Slot::Text, "red")).is_empty(),
        "文字色不该画成底色"
    );
}

#[gpui_kit::test]
fn custom_inline_color_paints_on_preview_commit_and_reload(cx: &mut TestAppContext) {
    use aero_core::colors::Slot;

    let source = "前 [中文😀]{.red .blue .bg-blue} 后\n";
    let (_dir, vault, path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    for (input, stored) in
        [("#269a6d80", "#269A6D80"), ("rgba(38,154,109,0.374900)", "rgba(38,154,109,0.374900)")]
    {
        let color = content_color(Slot::Background, stored);
        focus_unit(&editor, 0, cx);
        select_inline_text(&editor, "中文😀", cx);
        open_color_menu(&editor, cx);
        cx.simulate_input(input);
        assert!(!quads_of(cx, color).is_empty(), "pending preview of {input}");
        assert_eq!(text(&editor, cx), source);
        cx.simulate_keystrokes("escape");
        assert!(quads_of(cx, color).is_empty(), "cancel restores {input}");

        open_color_menu(&editor, cx);
        cx.simulate_input(input);
        cx.simulate_keystrokes("enter");
        let expected = if stored.starts_with('#') {
            format!("前 [中文😀]{{.red .blue background=\"{stored}\"}} 后\n")
        } else {
            format!("前 [中文😀]{{.red .blue background={stored}}} 后\n")
        };
        assert_eq!(text(&editor, cx), expected);
        assert_eq!(selected_inline_text(&editor, cx), "中文😀");
        cx.simulate_keystrokes("escape");
        assert!(!quads_of(cx, color).is_empty(), "confirmed preview of {input}");

        cx.executor().advance_clock(Duration::from_millis(600));
        cx.run_until_parked();
        let (reopened, _) = Editor::load(vault.clone(), path.clone()).unwrap();
        assert_eq!(reopened.to_string(), expected);
        cx.update(|_, cx| {
            let analysis = aero_core::markup::analyze(&expected);
            let markup = editor.read(cx).presenter_markup(&expected, &analysis, &Default::default(), cx);
            assert!(markup.styles.iter().any(|(_, style)| {
                style.background_color == Some(color) && style.color == Some(content_color(Slot::Text, "red"))
            }));
        });
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(text(&editor, cx), source);
    }
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
        let cell = cx.debug_bounds("table-cell-1-0").unwrap().scale(scale);
        let next = cx.debug_bounds("table-cell-2-0").unwrap().scale(scale);
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
    let empty_row = cx.debug_bounds("table-cell-1-0").unwrap();
    let body = cx.debug_bounds("table-cell-2-1").unwrap();
    assert!(empty_row.size.height > gpui_kit::px(0.));
    assert!(body.top() >= empty_row.bottom());
    cx.simulate_click(body.center(), gpui_kit::Modifiers::default());
    settle(cx);
    assert_eq!(cx.debug_bounds("table-cell-2-1").unwrap(), body);
    cx.update(|_, cx| {
        let e = editor.read(cx);
        let focused = e.focused.as_ref().unwrap();
        assert!(matches!(
            e.presenter_key_of(&focused.textarea).unwrap().part,
            super::presenter::Part::Cell(2, 1)
        ));
    });
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
    for label in ["正文", "中文"] {
        focus_text(&editor, label, cx);
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
                    input_preference: gpui_kit::InputPreference::KeyBindings,
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
        focus_text(&editor, label, cx);
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
fn front_matter_alias_completion_inserts_canonical_target_and_display_title(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "body\n");
    std::fs::write(
        dir.path().join("b.md"),
        "---\ntitle: Display\naliases: [Quicksilver]\n---\n\n# Heading\n",
    )
    .unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input(" [[quicksilver");
    wiki_catalog(&editor, &[], cx);
    cx.update(|_, cx| assert_eq!(editor.read(cx).wiki.as_ref().unwrap().matches.len(), 1));
    cx.simulate_keystrokes("shift-enter");
    assert_eq!(text(&editor, cx), "body [[b|Display]]\n");
    assert_eq!(selected_inline_text(&editor, cx), "Display");
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
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            assert_ne!(old, e.wiki.as_ref().unwrap().generation);
            let task = cx.background_spawn(async { Ok(vec![]) });
            e.load_wiki_notes(old, task, vec![], window, cx);
        })
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), "body ![[missing]]\n");
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            assert!(e.wiki.as_ref().unwrap().catalog.is_none());
            let generation = e.wiki.as_ref().unwrap().generation;
            let task = cx.background_spawn(async { Err("目录不可读".into()) });
            e.load_wiki_notes(generation, task, vec![], window, cx);
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
    assert!(aero_core::identity::is_block_id(&fragment[1..]));
    assert!(completed.ends_with(&format!("目标正文未保存 {fragment}\n\n引用 [[#{fragment}]]\n")));
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
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            assert_ne!(old, e.wiki.as_ref().unwrap().generation);
            let task = cx.background_spawn(async { Ok(vec![]) });
            e.load_wiki_notes(old, task, vec![], window, cx);
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
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            assert_ne!(e.wiki.as_ref().unwrap().generation, old);
            let notes = cx.background_spawn(async { Ok(vec![]) });
            e.load_wiki_notes(old, notes, vec![], window, cx);
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
        assert_eq!(m.catalog.as_ref().unwrap().notes[m.matches[0]].path.as_str(), "z.md");
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
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            let task = cx.background_spawn(async { Ok(vec![]) });
            e.load_wiki_notes(old, task, vec![], window, cx);
        })
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            let m = e.wiki.as_ref().unwrap();
            assert!(m.catalog.is_none(), "old reply must not replace the new menu");
            let generation = m.generation;
            let task = cx.background_spawn(async { Err("test failure".into()) });
            e.load_wiki_notes(generation, task, vec![], window, cx);
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
    focus_unit(&editor, 1, cx);
    cx.update(|_, cx| assert!(editor.read(cx).command_available(&crate::actions::ConvertBlock(2))));
    focus_unit(&editor, 2, cx);
    cx.update(|_, cx| assert!(!editor.read(cx).command_available(&crate::actions::ConvertBlock(2))));
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
        let f = e.focused.as_ref().expect("clicking nested content edits the callout body source");
        assert!(f.enter_newline);
        let body = &source[source.find('\n').unwrap() + 1..];
        assert_eq!(f.textarea.read(cx).value(), body.trim_end(), "the title stays a title");
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
        assert_eq!(style.table().overflow.x, Some(gpui_kit::Overflow::Scroll));
        assert_eq!(style.table_head().text.font_weight, Some(gpui_kit::FontWeight::SEMIBOLD));
    });
    // 表格按格编辑：键盘进来落在一格里，控件里只有这一格的文字
    focus_unit(&editor, 0, cx);
    cx.update(|_, cx| {
        let e = editor.read(cx);
        let f = e.focused.as_ref().unwrap();
        assert!(!f.enter_newline);
        assert_eq!(f.textarea.read(cx).value(), "42");
    });
    cx.simulate_keystrokes("escape");
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn wide_table_scroll_keeps_vertical_document_navigation(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, ScrollDelta, ScrollEvent, TouchPhase, point, px};
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
        cx.simulate_event(ScrollEvent {
            position,
            delta: ScrollDelta::Pixels(point(px(-150.), px(0.))),
            modifiers: Modifiers::default(),
            touch_phase,
            source: gpui_kit::ScrollSource::Pointer,
        });
    }
    cx.run_until_parked();
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert_eq!(e.scroll.offset().y, px(0.), "horizontal gestures stay in the table");
        assert!(e.focused.is_none());
    });
    cx.simulate_event(ScrollEvent {
        position,
        delta: ScrollDelta::Pixels(point(px(0.), px(-120.))),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Started,
        source: gpui_kit::ScrollSource::Pointer,
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

    // 代码块：⏎ 只是换行。控件里只有围栏之间的代码，聚焦时光标就在 `x` 之后
    focus_unit(&editor, 1, cx);
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert_eq!(
            e.focused.as_ref().unwrap().textarea.read(cx).value(),
            "x",
            "fences are not in the text area"
        );
    });
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
    // 段首 ⌫：并回上一行。上一行是列表项时，这段文字接到列表项末尾
    cx.simulate_keystrokes("home backspace");
    assert_eq!(text(&editor, cx), "- one\n- two\n\npara\n");
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
            match m.matches[m.selected] {
                super::slash::Match::Item(ix) => super::slash::ITEMS[ix].label.to_owned(),
                super::slash::Match::Color(ix) => super::color_menu::full_name(ix),
            }
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

#[gpui_kit::test]
fn typing_hashes_and_a_space_on_a_heading_sets_that_level(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "# 沙箱限制\n\n### 三级\n\n正文\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| editor.update(cx, |e, cx| e.focus_unit(0, 2, window, cx)));
    cx.simulate_input("## ");
    assert_eq!(text(&editor, cx), "## 沙箱限制\n\n### 三级\n\n正文\n");
    cx.update(|_, cx| {
        assert!(matches!(editor.read(cx).units[0].kind, aero_core::BlockKind::Heading { level: 2 }));
    });
    // 没有空格时，井号留在标题文字里
    cx.update(|window, cx| editor.update(cx, |e, cx| e.focus_unit(0, 3, window, cx)));
    cx.simulate_input("##");
    assert_eq!(text(&editor, cx), "## ##沙箱限制\n\n### 三级\n\n正文\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "## 沙箱限制\n\n### 三级\n\n正文\n");
    // 三级标题输入 "## " 改成二级
    cx.update(|window, cx| editor.update(cx, |e, cx| e.focus_unit(1, 4, window, cx)));
    cx.simulate_input("## ");
    assert_eq!(text(&editor, cx), "## 沙箱限制\n\n## 三级\n\n正文\n");
    // 普通段落输入 "## " 仍然变成二级标题
    cx.update(|window, cx| editor.update(cx, |e, cx| e.focus_unit(2, 0, window, cx)));
    cx.simulate_input("## ");
    assert_eq!(text(&editor, cx), "## 沙箱限制\n\n## 三级\n\n## 正文\n");
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

#[gpui_kit::test]
fn conversion_shortcuts_and_slash_reject_lossy_items_and_keep_continuations(cx: &mut TestAppContext) {
    let source = "- first\n\n  continuation\n\n  - nested ^kid\n\nafter\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    rich_root(&editor, cx);
    focus_unit(&editor, 0, cx);
    cx.update(|_, cx| assert!(!editor.read(cx).command_available(&crate::actions::ConvertBlock(1))));
    cx.simulate_keystrokes("cmd-alt-1");
    assert_eq!(text(&editor, cx), source);
    cx.simulate_keystrokes("end");
    cx.simulate_input(" /h1");
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), source.replacen("first", "first /h1", 1));
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), source);
    focus_unit(&editor, 1, cx);
    cx.simulate_keystrokes("cmd-alt-2");
    assert_eq!(text(&editor, cx), source.replace("  continuation", "  ## continuation"));
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn slash_emoji_keeps_the_command_location_for_unicode_input(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "before\n\nafter\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input(" /emoji");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "before \n\nafter\n");
    cx.simulate_input("👩‍👩‍👧‍👦");
    assert_eq!(text(&editor, cx), "before 👩‍👩‍👧‍👦\n\nafter\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "before \n\nafter\n");
}

#[gpui_kit::test]
fn slash_emoji_preserves_surrounding_text_and_spaces(cx: &mut TestAppContext) {
    let source = "before  after\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.update(|_, cx| {
        editor.read(cx).focused.as_ref().unwrap().textarea.clone().update(cx, |ta, cx| {
            ta.set_selected_range(8..8, cx);
        });
    });
    cx.simulate_input("/emoji");
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&editor, cx), source);
    cx.simulate_input("\u{1F44D}\u{1F3FD}");
    assert_eq!(text(&editor, cx), "before  \u{1F44D}\u{1F3FD}after\n");
    cx.simulate_keystrokes("backspace");
    assert_eq!(text(&editor, cx), source);
}

#[cfg(target_os = "macos")]
#[gpui_kit::test]
fn emoji_command_preserves_selection_and_does_not_edit_until_input(cx: &mut TestAppContext) {
    use gpui_kit::Focusable as _;

    let source = "before after\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let command = crate::actions::commands()
        .into_iter()
        .find(|command| command.id == "emoji")
        .expect("system emoji command must be discoverable");
    cx.update(|_, cx| assert!(!editor.read(cx).command_available(command.action.as_ref())));
    focus_unit(&editor, 0, cx);
    let textarea = cx.update(|_, cx| editor.read(cx).focused.as_ref().unwrap().textarea.clone());
    cx.update(|window, cx| {
        textarea.update(cx, |ta, cx| ta.set_selected_range(7..12, cx));
        assert!(editor.read(cx).command_available(command.action.as_ref()));
        window.dispatch_action(command.action.boxed_clone(), cx);
    });
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), source);
    cx.update(|window, cx| {
        assert_eq!(editor.read(cx).focused.as_ref().unwrap().textarea, textarea);
        assert_eq!(textarea.read(cx).selected_range(), 7..12);
        assert!(textarea.focus_handle(cx).is_focused(window));
        assert!(!editor.read(cx).doc.can_undo());
    });
    cx.simulate_input("\u{1F600}");
    assert_eq!(text(&editor, cx), "before \u{1F600}\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn slash_emoji_does_not_open_for_a_different_input_after_refocus(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "/emoji\n\nafter\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.update(|_, cx| editor.update(cx, |e, cx| e.update_slash(cx)));
    cx.simulate_keystrokes("enter");
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
        window.draw(cx).clear(cx);
        editor.update(cx, |e, cx| {
            let id = e.units.last().unwrap().id;
            e.focus_block(id, window, cx);
        });
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
        window.draw(cx).clear(cx);
    });
    cx.simulate_input("!");
    assert!(text(&editor, cx).ends_with("after!\n"));
}

/// 点击浏览态的块：画这一块的就是文本控件，按下的那一刻光标就落在点击处，不等下一帧。
#[gpui_kit::test]
fn click_positions_cursor_in_block(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "hello\n\nworld\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let draw = |cx: &mut VisualTestContext| cx.update(|window, cx| window.draw(cx).clear(cx));
    let cursor = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| editor.read(cx).focused.as_ref().map(|f| f.textarea.read(cx).cursor()))
    };

    draw(cx);
    let bounds = cx.debug_bounds("unit-1").expect("first block is laid out");
    // 点在第一块最左边、垂直居中
    let at = gpui_kit::point(bounds.origin.x + gpui_kit::px(2.), bounds.origin.y + bounds.size.height / 2.);
    cx.simulate_click(at, gpui_kit::Modifiers::default());
    assert_eq!(cursor(cx), Some(0), "cursor lands where the click is, in the same event");
    cx.simulate_input("X");
    assert_eq!(text(&editor, cx), "Xhello\n\nworld\n");

    // 回到浏览态，再点第二块的最右边：光标在该块末尾
    cx.simulate_keystrokes("escape");
    draw(cx);
    let bounds = cx.debug_bounds("unit-2").expect("second block is laid out");
    let at = gpui_kit::point(bounds.right() - gpui_kit::px(2.), bounds.origin.y + bounds.size.height / 2.);
    cx.simulate_click(at, gpui_kit::Modifiers::default());
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

#[gpui_kit::test]
fn external_check_defers_disk_read_and_parse(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "old\n");
    std::fs::write(dir.path().join("a.md"), "new from disk\n").unwrap();
    editor.update(cx, |e, cx| {
        e.check_external(cx);
        assert_eq!(e.doc.to_string(), "old\n", "disk read and parse must not run in the UI callback");
    });
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| editor.read(cx).doc.to_string()), "new from disk\n");
}

#[gpui_kit::test]
fn save_defers_conflict_backup_until_background_work(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "base\n");
    std::fs::write(dir.path().join("a.md"), "theirs\n").unwrap();
    editor.update(cx, |e, cx| {
        e.doc.edit(aero_core::Edit::insert(0, "mine ")).unwrap();
        e.after_doc_change(cx);
        e.save_now(cx);
        assert!(conflict_copies(dir.path()).is_empty(), "conflict backup must not block the UI callback");
    });
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), "mine base\n");
    assert_eq!(conflict_copies(dir.path()).len(), 1);
}

#[gpui_kit::test]
fn editing_during_external_check_preserves_local_changes(cx: &mut TestAppContext) {
    let (dir, _vault, _path, editor) = open(cx, "base\n");
    std::fs::write(dir.path().join("a.md"), "theirs\n").unwrap();
    editor.update(cx, |e, cx| {
        e.check_external(cx);
        e.doc.edit(aero_core::Edit::insert(0, "mine ")).unwrap();
        e.after_doc_change(cx);
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(600));
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| editor.read(cx).doc.to_string()), "mine base\n");
    assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), "mine base\n");
    let copies = conflict_copies(dir.path());
    assert_eq!(copies.len(), 1);
    assert_eq!(std::fs::read_to_string(dir.path().join(&copies[0])).unwrap(), "theirs\n");
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

#[gpui_kit::test]
fn conflict_choice_keeps_backup_when_save_fails(cx: &mut TestAppContext) {
    let (dir, vault, _path, editor) = open(cx, "base\n");
    let copy = VaultPath::new("a.conflict-backup.md").unwrap();
    vault.create_bytes(&copy, b"disk version\n").unwrap();
    std::fs::rename(dir.path().join("a.md"), dir.path().join("original.md")).unwrap();
    std::fs::create_dir(dir.path().join("a.md")).unwrap();
    editor.update(cx, |e, cx| {
        e.set_banner(Some(super::Banner::Conflict { copy: copy.clone() }), cx);
        e.take_disk_version(cx);
    });
    cx.run_until_parked();
    assert_eq!(std::fs::read(vault.fs_path(&copy)).unwrap(), b"disk version\n");
    assert_eq!(std::fs::read(dir.path().join("original.md")).unwrap(), b"base\n");
}

#[gpui_kit::test]
fn conflict_choice_does_not_replace_newer_input(cx: &mut TestAppContext) {
    let (_dir, vault, _path, editor) = open(cx, "base\n");
    let copy = VaultPath::new("a.conflict-backup.md").unwrap();
    vault.create_bytes(&copy, b"disk version\n").unwrap();
    editor.update(cx, |e, cx| {
        e.set_banner(Some(super::Banner::Conflict { copy: copy.clone() }), cx);
        e.take_disk_version(cx);
        e.doc.edit(aero_core::Edit::insert(0, "new ")).unwrap();
        e.after_doc_change(cx);
    });
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| editor.read(cx).doc.to_string()), "new base\n");
    assert!(vault.fs_path(&copy).is_file());
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
            EditorEvent::Saved(_) => sink.borrow_mut().push("saved"),
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

    // 键盘选区命中正文子块。复制正文并删除时保留这两个列表项的标记。
    cx.simulate_keystrokes("escape shift-down cmd-c");
    assert_eq!(cx.read_from_clipboard().and_then(|c| c.text()), Some("one!\n\ntwo".to_owned()));
    cx.simulate_keystrokes("backspace");
    assert_eq!(text(&editor, cx), "# Title\n\n- \n- \n\nlast\n");
    assert_eq!(selected(&editor, cx), Some(1..2));
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), "# Title\n\n- one!\n- two\n\nlast\n");
    assert_eq!(selected(&editor, cx), Some(1..2), "undo re-selects the restored block");

    // ⌘X 剪切最后一块：前面多出的空行一起收掉，选区退到上一块
    cx.simulate_keystrokes("down down cmd-x");
    assert_eq!(cx.read_from_clipboard().and_then(|c| c.text()), Some("last".to_owned()));
    assert!(text(&editor, cx).ends_with("# Title\n\n- one!\n- two\n"));
    assert_eq!(selected(&editor, cx), Some(2..3));
    cx.executor().advance_clock(Duration::from_millis(600));
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "# Title\n\n- one!\n- two\n\nlast\n");
    cx.simulate_keystrokes("cmd-z");
    focus_unit(&editor, 3, cx);
    cx.simulate_keystrokes("escape backspace");

    // 选择列表外层时，删除整个列表，再删标题。
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            let list = e
                .doc
                .blocks()
                .roots
                .iter()
                .find(|b| matches!(b.kind, aero_core::BlockKind::List { .. }))
                .unwrap();
            e.select_block_by_id(list.id, window, cx);
        })
    });
    cx.simulate_keystrokes("backspace backspace");
    cx.update(|_, cx| assert!(editor.read(cx).units.is_empty()));
    assert_eq!(selected(&editor, cx), None);
    cx.executor().advance_clock(Duration::from_millis(600));
    cx.run_until_parked();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), text(&editor, cx));
}

#[gpui_kit::test]
fn selected_container_routes_input_to_its_first_editable_child(cx: &mut TestAppContext) {
    let source = ":::highlight\nfirst ^first\n\nsecond\n:::\n\nafter\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let id = cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            let id = e.doc.blocks().roots[0].id;
            e.select_block_by_id(id, window, cx);
            e.units[0].id
        })
    });
    cx.simulate_input("新");
    assert_eq!(text(&editor, cx), source.replace("first ^first", "first新 ^first"));
    cx.update(|_, cx| assert_eq!(editor.read(cx).focused.as_ref().unwrap().id, Some(id)));
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn first_input_in_a_selected_empty_highlight_keeps_its_closing_fence(cx: &mut TestAppContext) {
    let source = ":::highlight\n:::\n\nafter\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| e.select_block_by_id(e.doc.blocks().roots[0].id, window, cx))
    });
    cx.simulate_input("新");
    assert_eq!(text(&editor, cx), ":::highlight\n新\n:::\n\nafter\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn first_input_in_a_selected_empty_quote_keeps_its_container(cx: &mut TestAppContext) {
    let source = "> \n\nafter\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| e.select_block_by_id(e.doc.blocks().roots[0].id, window, cx))
    });
    cx.simulate_input("新");
    assert_eq!(text(&editor, cx), "> 新\n\nafter\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn dragging_text_across_containers_deletes_only_the_selected_children(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, MouseButton, point, px};
    let source = ":::highlight\nfirst ^first\n:::\n\n> second ^second\n\nafter\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    settle(cx);
    let ids = cx.update(|_, cx| editor.read(cx).units.iter().map(|u| u.id).collect::<Vec<_>>());
    let body = |cx: &mut VisualTestContext, index: usize| {
        let selector = Box::leak(format!("unit-{}", ids[index].0).into_boxed_str());
        let row = cx.debug_bounds(selector).unwrap();
        row.origin + point(px(10.), px(10.))
    };
    let from = body(cx, 0);
    let to = body(cx, 1);
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
    settle(cx);
    assert_eq!(selected(&editor, cx), Some(0..2));
    cx.simulate_keystrokes("backspace");
    assert_eq!(text(&editor, cx), ":::highlight\n:::\n\n> \n\nafter\n");
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn copying_task_body_strips_alignment_spaces_from_the_clipboard(cx: &mut TestAppContext) {
    let source = "- [ ] first\n      continued\n\n  - nested ^nested\n- sibling\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_keystrokes("escape cmd-c");
    assert_eq!(cx.read_from_clipboard().and_then(|c| c.text()).as_deref(), Some("first\ncontinued"));
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn cutting_first_list_body_keeps_its_item_and_continuation(cx: &mut TestAppContext) {
    let source = "- first ^first\n\n  continuation ^cont\n\n  - nested ^nested\n- sibling\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_keystrokes("escape cmd-x");
    assert_eq!(cx.read_from_clipboard().and_then(|c| c.text()).as_deref(), Some("first ^first"));
    assert!(text(&editor, cx).ends_with("- \n  continuation ^cont\n\n  - nested ^nested\n- sibling\n"));
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), source);
}

/// Type labels and six-dot handles open the captured block menu without edits.
#[gpui_kit::test]
fn gutter_type_and_handle_open_menu_without_geometry_or_document_changes(cx: &mut TestAppContext) {
    let source = "intro\n\n- item\n\nend\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let selector = |name: &str, id: aero_core::BlockId| -> &'static str {
        Box::leak(format!("{name}-{}", id.0).into_boxed_str())
    };
    let ids: Vec<_> = cx.update(|_, cx| editor.read(cx).units.iter().map(|u| u.id).collect());
    let intro = cx.debug_bounds(selector("unit", ids[0])).unwrap();
    hover_block(ids[0], cx);
    let type_intro = cx.debug_bounds(selector("gutter-plus", ids[0])).unwrap();
    let handle_intro = cx.debug_bounds(selector("gutter-handle", ids[0])).unwrap();
    hover_block(ids[1], cx);
    let handle_item = cx.debug_bounds(selector("gutter-handle", ids[1])).unwrap();
    assert!(handle_intro.right() <= type_intro.origin.x && type_intro.right() <= intro.origin.x);
    assert_eq!(handle_item.origin.x, handle_intro.origin.x);
    assert_eq!(intro.size.width, gpui_kit::px(720.));
    for id in ids.iter().copied() {
        for name in ["gutter-plus", "gutter-handle"] {
            hover_block(id, cx);
            let button = cx.debug_bounds(selector(name, id)).unwrap();
            cx.simulate_click(button.center(), gpui_kit::Modifiers::default());
            settle(cx);
            cx.update(|_, cx| assert_eq!(editor.read(cx).block_menu.as_ref().map(|m| m.block), Some(id)));
            assert_eq!(text(&editor, cx), source);
            assert_eq!(cx.debug_bounds(selector("unit", ids[0])).unwrap(), intro);
            cx.simulate_keystrokes("escape");
            settle(cx);
            cx.update(|_, cx| assert!(editor.read(cx).block_menu.is_none()));
        }
    }
}

#[gpui_kit::test]
fn tab_and_shift_tab_change_the_list_item_level(cx: &mut TestAppContext) {
    let source = "- first\n- second ^second\n\n  continuation\n\n  - nested ^nested\n- third\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 1, cx);
    let depth = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            let e = editor.read(cx);
            let body = e
                .doc
                .blocks()
                .iter()
                .find(|(_, block)| block.persisted_id.as_deref() == Some("second"))
                .unwrap()
                .1
                .id;
            aero_core::containers::ancestors(&e.doc, body)
                .iter()
                .filter(|block| matches!(block.kind, aero_core::BlockKind::ListItem { .. }))
                .count()
        })
    };
    cx.simulate_keystrokes("tab");
    assert_eq!(depth(&editor, cx), 2);
    assert!(!text(&editor, cx).contains("indent="));
    cx.simulate_keystrokes("shift-tab");
    assert_eq!(depth(&editor, cx), 1);
    cx.simulate_keystrokes("cmd-z cmd-z");
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn visual_indent_increase_enforces_level_and_width_limits(_cx: &mut TestAppContext) {
    assert!(Editor::can_increase_visual_indent(0, gpui_kit::px(184.)));
    assert!(!Editor::can_increase_visual_indent(0, gpui_kit::px(183.)));
    assert!(Editor::can_increase_visual_indent(7, gpui_kit::px(184.)));
    assert!(!Editor::can_increase_visual_indent(7, gpui_kit::px(183.)));
    assert!(!Editor::can_increase_visual_indent(8, gpui_kit::px(1000.)));
}

#[gpui_kit::test]
fn visual_indent_bounds_show_the_current_available_width(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "plain\n\nindented {indent=2}\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    settle(cx);
    let (plain, indented) = cx.update(|_, cx| {
        let e = editor.read(cx);
        let plain = e.unit_bounds.iter().find(|(id, _)| *id == e.units[0].id).unwrap().1;
        let indented = e.unit_bounds.iter().find(|(id, _)| *id == e.units[1].id).unwrap().1;
        (plain, indented)
    });
    assert_eq!(plain.size.width - indented.size.width, gpui_kit::px(48.));
    assert!(indented.origin.x > plain.origin.x);
}

#[gpui_kit::test]
fn paragraph_menu_applies_visual_indent_with_boundaries(cx: &mut TestAppContext) {
    for (source, row, expected) in [
        ("para\n", 4, "para {indent=1}\n"),
        ("para {indent=8}\n", 4, "para {indent=8}\n"),
        ("para\n", 5, "para\n"),
    ] {
        let (_dir, _vault, _path, editor) = open(cx, source);
        let cx = &mut VisualTestContext::from_window(*cx.windows().last().unwrap(), cx);
        cx.update(|window, cx| {
            editor.update(cx, |e, cx| {
                let id = e.doc.blocks().roots[0].id;
                e.open_block_menu(id, gpui_kit::point(gpui_kit::px(20.), gpui_kit::px(40.)), window, cx);
            })
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        let layout = cx.debug_bounds("block-menu-layout").unwrap().center();
        cx.simulate_click(layout, gpui_kit::Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        let action =
            cx.debug_bounds(Box::leak(format!("block-layout-{row}").into_boxed_str())).unwrap().center();
        cx.simulate_click(action, gpui_kit::Modifiers::default());
        settle(cx);
        assert_eq!(text(&editor, cx), expected);
        if expected == source {
            cx.update(|_, cx| assert!(editor.read(cx).block_menu.is_some(), "禁用项点击不能穿透或关闭菜单"));
        }
        assert!(!expected.starts_with(' '));
    }
}

#[gpui_kit::test]
fn list_menu_changes_structure_instead_of_visual_indent(cx: &mut TestAppContext) {
    let source = "- first\n- second ^second\n\n  continuation\n\n  - nested ^nested\n- third\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            let item = e.doc.blocks().roots[0].children[1].id;
            e.open_block_menu(item, gpui_kit::point(gpui_kit::px(20.), gpui_kit::px(40.)), window, cx);
        })
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
    let layout = cx.debug_bounds("block-menu-layout").unwrap().center();
    cx.simulate_click(layout, gpui_kit::Modifiers::default());
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
    let increase = cx.debug_bounds("block-layout-4").unwrap().center();
    cx.simulate_click(increase, gpui_kit::Modifiers::default());
    settle(cx);
    let depth = cx.update(|_, cx| {
        let e = editor.read(cx);
        let body = e
            .doc
            .blocks()
            .iter()
            .find(|(_, block)| block.persisted_id.as_deref() == Some("second"))
            .unwrap()
            .1
            .id;
        aero_core::containers::ancestors(&e.doc, body)
            .iter()
            .filter(|block| matches!(block.kind, aero_core::BlockKind::ListItem { .. }))
            .count()
    });
    assert_eq!(depth, 2);
    assert!(!text(&editor, cx).contains("indent="));
    cx.simulate_keystrokes("cmd-z");
    assert_eq!(text(&editor, cx), source);
}

#[gpui_kit::test]
fn tab_handles_empty_items_and_unavailable_list_targets(cx: &mut TestAppContext) {
    let source = "- first\n- \n- third\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 1, cx);
    cx.simulate_keystrokes("tab");
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert!(
            e.doc.blocks().roots[0].children[0]
                .children
                .iter()
                .any(|block| { matches!(block.kind, aero_core::BlockKind::List { .. }) })
        );
    });
    cx.simulate_keystrokes("shift-tab");
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert!(
            e.doc.blocks().roots[0].children[0]
                .children
                .iter()
                .all(|block| { !matches!(block.kind, aero_core::BlockKind::List { .. }) })
        );
    });
    cx.simulate_keystrokes("cmd-z cmd-z");
    assert_eq!(text(&editor, cx), source);

    let (_dir2, _vault2, _path2, editor) = open(cx, "- only\n");
    let cx = &mut VisualTestContext::from_window(*cx.windows().last().unwrap(), cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_keystrokes("tab");
    assert_eq!(text(&editor, cx), "- only\n");
}

#[gpui_kit::test]
fn tab_inside_a_nested_code_block_is_not_list_indent(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "- ```\n  code\n  ```\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_keystrokes("tab shift-tab");
    cx.update(|_, cx| {
        let e = editor.read(cx);
        assert_eq!(e.doc.blocks().roots.len(), 1);
        assert!(matches!(e.doc.blocks().roots[0].kind, aero_core::BlockKind::List { .. }));
    });
}

#[gpui_kit::test]
fn list_operation_targets_remain_reachable_with_and_without_virtualization(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, point, px, size};
    let source = "before\n\n- first\n\n  continuation\n\n  - nested\n- second\n\nafter\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.simulate_resize(size(px(1000.), px(900.)));
    for virtualized in [false, true] {
        if virtualized {
            let source = format!("{source}{}", "\n\nparagraph".repeat(80));
            cx.update(|_, cx| {
                editor.update(cx, |editor, cx| {
                    editor.install_document(aero_core::Document::from_text(&source), editor.stamp, cx);
                    cx.notify();
                })
            });
        }
        settle(cx);
        let (list, item, paragraph) = cx.update(|_, cx| {
            let list = editor
                .read(cx)
                .doc
                .blocks()
                .roots
                .iter()
                .find(|b| matches!(b.kind, aero_core::BlockKind::List { .. }))
                .unwrap();
            (list.id, list.children[0].id, list.children[0].children[0].id)
        });
        let selector = |name: &str, id: aero_core::BlockId| -> &'static str {
            Box::leak(format!("{name}-{}", id.0).into_boxed_str())
        };
        let row = cx.debug_bounds(selector("row", item)).unwrap();
        // Enter directly from outside the list, without first focusing any text.
        cx.simulate_mouse_move(point(row.left() - px(66.), row.top() + px(12.)), None, Modifiers::default());
        settle(cx);
        cx.update(|_, cx| assert_eq!(editor.read(cx).hovered_block, Some(list), "virtualized={virtualized}"));
        let list_handle = cx.debug_bounds(selector("gutter-handle", list)).unwrap();
        cx.simulate_click(list_handle.center(), Modifiers::default());
        settle(cx);
        cx.update(|_, cx| assert_eq!(editor.read(cx).block_menu.as_ref().map(|m| m.block), Some(list)));
        cx.simulate_keystrokes("escape");
        settle(cx);
        // The marker column belongs to the item, the text body to its paragraph.
        cx.simulate_mouse_move(point(row.left() + px(12.), row.top() + px(12.)), None, Modifiers::default());
        settle(cx);
        cx.update(|_, cx| assert_eq!(editor.read(cx).hovered_block, Some(item)));
        let item_handle = cx.debug_bounds(selector("gutter-handle", item)).unwrap();
        assert!(list_handle.right() <= item_handle.left());
        cx.simulate_click(item_handle.center(), Modifiers::default());
        settle(cx);
        cx.update(|_, cx| assert_eq!(editor.read(cx).block_menu.as_ref().map(|m| m.block), Some(item)));
        cx.simulate_keystrokes("escape");
        hover_block(paragraph, cx);
        cx.update(|_, cx| assert_eq!(editor.read(cx).hovered_block, Some(paragraph)));
        cx.simulate_mouse_move(point(row.left() + px(12.), row.top() + px(12.)), None, Modifiers::default());
        settle(cx);
        cx.update(|_, cx| assert_eq!(editor.read(cx).hovered_block, Some(item)));
    }
    // A one-line list is shorter than its vertical operation pair.
    cx.update(|_, cx| {
        editor.update(cx, |editor, cx| {
            editor.install_document(aero_core::Document::from_text("- only\n"), editor.stamp, cx);
        })
    });
    settle(cx);
    let list = cx.update(|_, cx| editor.read(cx).doc.blocks().roots[0].id);
    let row = cx.debug_bounds(Box::leak(format!("row-{}", list.0).into_boxed_str())).unwrap();
    cx.simulate_mouse_move(point(row.left() - px(66.), row.top() + px(38.)), None, Modifiers::default());
    settle(cx);
    cx.update(|_, cx| assert_eq!(editor.read(cx).hovered_block, Some(list)));
    let kind = cx.debug_bounds(Box::leak(format!("gutter-plus-{}", list.0).into_boxed_str())).unwrap();
    cx.simulate_click(kind.center(), Modifiers::default());
    settle(cx);
    cx.update(|_, cx| assert_eq!(editor.read(cx).block_menu.as_ref().map(|m| m.block), Some(list)));
}

#[gpui_kit::test]
fn container_handles_stay_in_the_editor_gutter_and_target_the_container(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, point, px, size};
    let source = "before\n\n::::highlight\nouter\n\n> quote\n\n:::highlight\ninner\n:::\n::::\n\nafter\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.simulate_resize(size(px(1000.), px(900.)));
    settle(cx);
    let ids = cx.update(|_, cx| {
        editor
            .read(cx)
            .doc
            .blocks()
            .iter()
            .filter(|(_, b)| aero_core::containers::is_editable(&b.kind))
            .map(|(_, b)| b.id)
            .collect::<Vec<_>>()
    });
    let outer = cx.debug_bounds(Box::leak(format!("row-{}", ids[0].0).into_boxed_str())).unwrap();
    for id in ids {
        hover_block(id, cx);
        let selector =
            |name: &str| -> &'static str { Box::leak(format!("{name}-{}", id.0).into_boxed_str()) };
        let row = cx.debug_bounds(selector("row")).unwrap();
        let handle = cx.debug_bounds(selector("gutter-handle")).unwrap();
        let kind = cx.debug_bounds(selector("gutter-plus")).unwrap();
        // A container does not consume an extra gutter width: both buttons
        // must fit in the 48pt operation area immediately before its content.
        assert_eq!(handle.left(), outer.left() - px(super::GUTTER), "{id:?}: {handle:?}, {row:?}");
        assert!(kind.right() <= row.left());
        cx.simulate_mouse_move(point(row.left() + px(2.), row.top() + px(2.)), None, Modifiers::default());
        cx.simulate_mouse_move(handle.center(), None, Modifiers::default());
        settle(cx);
        cx.update(|_, cx| assert_eq!(editor.read(cx).hovered_block, Some(id)));
        cx.simulate_click(handle.center(), Modifiers::default());
        settle(cx);
        cx.update(|_, cx| assert_eq!(editor.read(cx).block_menu.as_ref().map(|m| m.block), Some(id)));
        assert_eq!(text(&editor, cx), source);
        cx.simulate_keystrokes("escape");
        settle(cx);
    }
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

/// 图片嵌入：占位 → 位图；`|宽度` 生效；与文字混排的嵌入仍是源码。
#[gpui_kit::test]
fn image_embeds_render_as_bitmaps_and_honour_the_width_parameter(cx: &mut TestAppContext) {
    let body = "![[wide.png]]\n\n![[thumb.png|120]]\n\n前面 ![[wide.png]] 后面\n\n![[missing.png]]\n";
    let (dir, _cache, _vault, editor) = open_with_images(cx, body);
    write_attachment(&dir, "a.assets/wide.png", 600, 300);
    write_attachment(&dir, "a.assets/thumb.png", 600, 300);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| {
        window.replace_root(cx, |window, cx| gpui_kit::component::Root::new(editor.clone(), window, cx));
        window.draw(cx).clear(cx);
    });
    // 附件目录与首次渲染都在后台：先画占位，不阻塞 UI
    assert!(cx.debug_bounds("image-placeholder-wide.png").is_some(), "首帧先占住版面");
    assert!(cx.debug_bounds("image-wide.png").is_none(), "这一帧还没有位图");
    settle(cx);
    let bounds = cx.debug_bounds("image-wide.png").expect("渲染完成后应有位图");
    assert!(cx.debug_bounds("image-placeholder-wide.png").is_none(), "占位应被替换");
    // 600×300 的图按默认列宽显示：等比，且不放大到超过原始宽度
    assert!(f32::from(bounds.size.width) <= 600., "不放大: {:?}", bounds.size);
    let ratio = f32::from(bounds.size.height) / f32::from(bounds.size.width);
    assert!((ratio - 0.5).abs() < 0.02, "应保持 2:1: {:?}", bounds.size);
    // `|120`：按参数宽度显示，同样等比
    let thumb = cx.debug_bounds("image-thumb.png").expect("带宽度参数的图也要渲染");
    assert!((f32::from(thumb.size.width) - 120.).abs() < 1., "应按 |120 显示: {:?}", thumb.size);
    assert!((f32::from(thumb.size.height) - 60.).abs() < 1., "应保持 2:1: {:?}", thumb.size);
    assert!(f32::from(bounds.size.width) > f32::from(thumb.size.width), "默认宽度应更大");
    // 与文字混排的嵌入不画图，保持源码
    assert!(cx.debug_bounds("image-source-missing.png").is_some(), "找不到附件时显示源码");
    assert!(cx.debug_bounds("image-error").is_some(), "并给出一行错误");
    assert!(cx.debug_bounds("image-retry").is_some(), "以及重试入口");
}

/// 同一段里的多张图并排，按宽度比例分掉行宽，不叠成上下两张。
#[gpui_kit::test]
fn images_in_one_paragraph_share_the_line(cx: &mut TestAppContext) {
    let (dir, _cache, _vault, editor) = open_with_images(cx, "![[wide.png|400]] ![[tall.png|200]]\n");
    write_attachment(&dir, "a.assets/wide.png", 400, 200);
    write_attachment(&dir, "a.assets/tall.png", 200, 400);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| {
        window.replace_root(cx, |window, cx| gpui_kit::component::Root::new(editor.clone(), window, cx));
    });
    settle(cx);
    let wide = cx.debug_bounds("image-wide.png").expect("wide image");
    let tall = cx.debug_bounds("image-tall.png").expect("tall image");
    assert!(tall.origin.x >= wide.right() - gpui_kit::px(2.), "两张图在同一行: {wide:?} {tall:?}");
    assert!((f32::from(tall.origin.y) - f32::from(wide.origin.y)).abs() < 2., "顶部对齐");
    let ratio = f32::from(wide.size.width) / f32::from(tall.size.width);
    assert!((ratio - 2.).abs() < 0.15, "400:200 的宽度比: {ratio} {wide:?} {tall:?}");
    cx.simulate_click(tall.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.update(|_, cx| {
        let editor = editor.read(cx);
        assert!(editor.selection.is_none(), "并排时也不选中整块");
        assert_eq!(editor.selected_image_index(editor.units[0].id), Some(1));
    });
    assert!(cx.debug_bounds("image-resize-se").is_some(), "只给点中的那张画选中框");
}

/// 附件是后来才放进来的：点「重试」就能出图，不用重开笔记。
#[gpui_kit::test]
fn image_failures_recover_after_retry(cx: &mut TestAppContext) {
    let (dir, _cache, _vault, editor) = open_with_images(cx, "![[later.png]]\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| {
        window.replace_root(cx, |window, cx| gpui_kit::component::Root::new(editor.clone(), window, cx));
    });
    settle(cx);
    let retry = cx.debug_bounds("image-retry").expect("附件不存在时有重试按钮");
    write_attachment(&dir, "a.assets/later.png", 200, 100);
    cx.simulate_click(retry.center(), gpui_kit::Modifiers::default());
    settle(cx);
    assert!(cx.debug_bounds("image-later.png").is_some(), "重试后应渲染出图片");
    assert!(cx.debug_bounds("image-error").is_none(), "错误提示应消失");
}

/// 点图片旁边、按回车、或在文末空白处点击，都继续显示同一张图，不露出 `![[…]]`。
#[gpui_kit::test]
fn editing_an_image_keeps_the_preview_size(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, point, px};
    let (dir, _cache, _vault, editor) = open_with_images(cx, "正文\n\n![[photo.png]]\n");
    write_attachment(&dir, "a.assets/photo.png", 200, 100);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| {
        window.replace_root(cx, |window, cx| gpui_kit::component::Root::new(editor.clone(), window, cx));
    });
    settle(cx);
    let before = cx.debug_bounds("image-photo.png").expect("预览应画出图片");
    let id = cx.update(|_, cx| editor.read(cx).units[1].id);
    let selector: &'static str = Box::leak(format!("unit-{}", id.0).into_boxed_str());
    let unit = cx.debug_bounds(selector).expect("image unit");
    let beside = point(unit.right() - px(4.), before.center().y);
    assert!(beside.x > before.right(), "图片右侧还有可点的空白");
    cx.simulate_click(beside, Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let after = cx.debug_bounds("image-photo.png").expect("点进块后图片还在");
    assert!((f32::from(after.size.width) - f32::from(before.size.width)).abs() < 1.);
    assert!((f32::from(after.size.height) - f32::from(before.size.height)).abs() < 1.);
    cx.update(|_, cx| {
        let editor = editor.read(cx);
        assert!(editor.focused.is_none(), "不打开源码框");
        assert!(editor.selection.is_none(), "不给整块上选中底色");
        assert!(editor.selected_image_index(id).is_some());
    });
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let entered = cx.debug_bounds("image-photo.png").expect("回车后仍是图片");
    assert!((f32::from(entered.size.width) - f32::from(before.size.width)).abs() < 1.);
    cx.update(|_, cx| assert!(editor.read(cx).focused.is_none()));
    let tail = cx.debug_bounds("editor-tail").expect("文末空白");
    cx.simulate_click(tail.center(), Modifiers::default());
    settle(cx);
    assert!(cx.debug_bounds("image-photo.png").is_some(), "点文末空白也不换成源码");
    cx.update(|_, cx| {
        let focused = editor.read(cx).focused.as_ref().map(|f| f.textarea.read(cx).value().to_string());
        assert_ne!(focused.as_deref(), Some("![[photo.png]]"));
    });
}

/// 单击图片只选中这张图（不进编辑态，也不给整块上底色）；双击全屏看图，`Esc` / `Space` 关掉。
#[gpui_kit::test]
fn clicking_an_image_selects_its_block_and_double_click_opens_the_viewer(cx: &mut TestAppContext) {
    let (dir, _cache, _vault, editor) = open_with_images(cx, "![[photo.png]]\n");
    write_attachment(&dir, "a.assets/photo.png", 400, 200);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| {
        window.replace_root(cx, |window, cx| gpui_kit::component::Root::new(editor.clone(), window, cx));
    });
    settle(cx);
    let image = cx.debug_bounds("image-photo.png").unwrap();
    cx.simulate_click(image.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    cx.update(|_, cx| {
        let editor = editor.read(cx);
        assert!(editor.focused.is_none(), "单击不进编辑态");
        assert!(editor.selection.is_none(), "不选中整块");
        assert_eq!(editor.selected_image_index(editor.units[0].id), Some(0));
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("image-photo.png").is_some(), "选中后图片仍在");

    // 双击：全屏看图
    let image = cx.debug_bounds("image-photo.png").unwrap();
    double_click(cx, image.center());
    settle(cx);
    assert!(cx.debug_bounds("image-viewer").is_some(), "双击应打开全屏看图");
    cx.simulate_keystrokes("escape");
    settle(cx);
    assert!(cx.debug_bounds("image-viewer").is_none(), "Esc 关闭看图");
    // Space 也能关（只在看图浮层的键位上下文里绑定）
    let image = cx.debug_bounds("image-photo.png").unwrap().center();
    double_click(cx, image);
    settle(cx);
    assert!(cx.debug_bounds("image-viewer").is_some());
    cx.simulate_keystrokes("space");
    settle(cx);
    assert!(cx.debug_bounds("image-viewer").is_none(), "Space 关闭看图");
}

/// 选中图片后拖右下角，按原图比例改宽度，松手写回 `|宽度`，一次撤销。
#[gpui_kit::test]
fn dragging_an_image_corner_rewrites_the_width(cx: &mut TestAppContext) {
    use gpui_kit::{Modifiers, MouseButton, point, px};
    let (dir, _cache, _vault, editor) = open_with_images(cx, "![[photo.png]]\n");
    write_attachment(&dir, "a.assets/photo.png", 800, 400);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| {
        window.replace_root(cx, |window, cx| gpui_kit::component::Root::new(editor.clone(), window, cx));
    });
    settle(cx);
    assert!(cx.debug_bounds("image-resize-se").is_none(), "没选中时没有手柄");
    let image = cx.debug_bounds("image-photo.png").unwrap();
    let before = f32::from(image.size.width);
    cx.simulate_click(image.center(), Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let handle = cx.debug_bounds("image-resize-se").expect("选中后有右下角手柄");
    assert!(cx.debug_bounds("image-size").is_some(), "选中后显示尺寸");
    let start = handle.center();
    let end = start - point(px(160.), px(0.));
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(start + point(px(12.), px(0.)), MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::default());
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let live = cx.debug_bounds("image-photo.png").expect("拖动中图片还在");
    assert!(f32::from(live.size.width) < before - 40., "拖动时宽度跟着变: {:?}", live.size);
    let ratio = f32::from(live.size.height) / f32::from(live.size.width);
    assert!((ratio - 0.5).abs() < 0.05, "拖动时保持原图比例: {:?}", live.size);
    cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
    settle(cx);
    let body = text(&editor, cx);
    let width: u32 = body
        .split_once("|")
        .and_then(|(_, rest)| rest.split("]]").next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("应写回宽度: {body}"));
    assert!(width < before as u32 - 40, "松手写入更小的宽度: {body}");
    let shown = cx.debug_bounds("image-photo.png").unwrap();
    assert!((f32::from(shown.size.width) - width as f32).abs() < 2., "显示宽度与源码一致: {:?}", shown.size);
    cx.simulate_keystrokes("cmd-z");
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "![[photo.png]]\n");
}

/// 粘贴位图：写进 `<note>.assets/`、插入嵌入、单步撤销；纯文字粘贴仍走文本控件。
#[gpui_kit::test]
fn pasting_a_bitmap_imports_it_and_leaves_text_paste_alone(cx: &mut TestAppContext) {
    let (dir, _cache, _vault, editor) = open_with_images(cx, "正文\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.update(|_, cx| {
        let image = gpui_kit::Image { format: gpui_kit::ImageFormat::Png, bytes: image_bytes(40, 20), id: 1 };
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_image(&image));
    });
    cx.simulate_keystrokes("cmd-v");
    cx.run_until_parked();
    let body = text(&editor, cx);
    let name = body
        .split_once("![[")
        .and_then(|(_, rest)| rest.split_once("]]"))
        .map(|(name, _)| name.to_owned())
        .unwrap_or_else(|| panic!("粘贴应插入图片嵌入，实际是 {body:?}"));
    assert!(name.ends_with(".png"), "{name}");
    let file = dir.path().join("a.assets").join(&name);
    assert_eq!(std::fs::read(&file).unwrap(), image_bytes(40, 20), "原始字节写进 .assets/");
    // 一步撤销回到粘贴前：文件留着（磁盘写入不参与撤销），源码回退
    cx.simulate_keystrokes("cmd-z");
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "正文\n");
    assert!(file.is_file());

    // 剪贴板是文字：交还给文本控件，正常插入
    cx.update(|_, cx| cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string("粘来的文字".into())));
    cx.simulate_keystrokes("cmd-v");
    cx.run_until_parked();
    assert!(text(&editor, cx).contains("粘来的文字"), "{:?}", text(&editor, cx));
}

/// 单方括号链接交给浏览器；没有协议时补上 `http://`。
#[gpui_kit::test]
fn markdown_links_open_in_the_browser(cx: &mut TestAppContext) {
    let (_dir, _cache, _vault, editor) = open_with_images(cx, "[url](www.google.com)\n");
    let visual = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    visual.update(|window, cx| {
        editor.update(cx, |e, cx| e.open_external_link("https://example.com", window, cx));
    });
    assert_eq!(cx.opened_url().as_deref(), Some("https://example.com"));
    visual.update(|window, cx| {
        editor.update(cx, |e, cx| e.open_external_link("www.google.com", window, cx));
    });
    assert_eq!(cx.opened_url().as_deref(), Some("http://www.google.com"));
    visual.update(|window, cx| {
        editor.update(cx, |e, cx| e.open_external_link("rustup.rs", window, cx));
    });
    assert_eq!(cx.opened_url().as_deref(), Some("http://rustup.rs"));
}

#[gpui_kit::test]
fn attachment_import_defers_file_copy(cx: &mut TestAppContext) {
    let (dir, _cache, _vault, editor) = open_with_images(cx, "正文\n");
    let outside = tempfile::tempdir().unwrap();
    let source = outside.path().join("shot.png");
    std::fs::write(&source, image_bytes(8, 8)).unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            e.drop_external_files(&gpui_kit::ExternalPaths(vec![source].into()), window, cx);
            assert!(
                !dir.path().join("a.assets/shot.png").exists(),
                "copy must happen outside the UI callback"
            );
        });
    });
    cx.run_until_parked();
    assert!(dir.path().join("a.assets/shot.png").is_file());
    assert!(text(&editor, cx).contains("![[shot.png]]"));
}

/// 拖到编辑区的文件和 `⌘V` 同一套规则：图片变嵌入，其他文件变库内指针。
#[gpui_kit::test]
fn dropping_files_onto_the_editor_imports_them(cx: &mut TestAppContext) {
    let (dir, _cache, _vault, editor) = open_with_images(cx, "正文\n");
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("shot.png"), image_bytes(8, 8)).unwrap();
    std::fs::write(outside.path().join("notes.zip"), b"archive").unwrap();
    std::fs::create_dir(outside.path().join("跳过")).unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let position = cx.debug_bounds("editor").unwrap().center();
    let paths = gpui_kit::ExternalPaths(
        [outside.path().join("shot.png"), outside.path().join("notes.zip"), outside.path().join("跳过")]
            .into_iter()
            .collect(),
    );
    cx.simulate_event(gpui_kit::FileDropEvent::Entered { position, paths });
    cx.simulate_event(gpui_kit::FileDropEvent::Submit { position });
    cx.run_until_parked();
    let body = text(&editor, cx);
    assert!(body.contains("![[shot.png]]"), "{body}");
    assert!(body.contains("[[notes.zip]](a.assets/notes.zip)"), "{body}");
    assert!(!body.contains("跳过"), "文件夹不导入: {body}");
    assert_eq!(std::fs::read(dir.path().join("a.assets/shot.png")).unwrap(), image_bytes(8, 8));
    assert_eq!(std::fs::read(dir.path().join("a.assets/notes.zip")).unwrap(), b"archive");
}

/// Finder 里复制的文件：图片 / PDF 变嵌入，其他文件变库内指针，原名保留且不覆盖同名文件。
#[gpui_kit::test]
fn pasting_external_files_imports_them_with_their_names(cx: &mut TestAppContext) {
    let (dir, _cache, _vault, editor) = open_with_images(cx, "正文\n");
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("图 1.png"), image_bytes(10, 10)).unwrap();
    std::fs::write(outside.path().join("notes.zip"), b"archive").unwrap();
    std::fs::create_dir_all(dir.path().join("a.assets")).unwrap();
    std::fs::write(dir.path().join("a.assets/图 1.png"), b"already here").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.update(|_, cx| {
        let paths = gpui_kit::ExternalPaths(
            [outside.path().join("图 1.png"), outside.path().join("notes.zip")].into_iter().collect(),
        );
        cx.write_to_clipboard(gpui_kit::ClipboardItem {
            entries: vec![gpui_kit::ClipboardEntry::ExternalPaths(paths)],
        });
    });
    cx.simulate_keystrokes("cmd-v");
    cx.run_until_parked();
    let body = text(&editor, cx);
    assert!(body.contains("![[图 1-2.png]]"), "同名不覆盖，改名导入: {body:?}");
    assert!(body.contains("[[notes.zip]](a.assets/notes.zip)"), "非图片 / PDF 插入库内指针: {body:?}");
    assert_eq!(std::fs::read(dir.path().join("a.assets/图 1.png")).unwrap(), b"already here");
    assert_eq!(std::fs::read(dir.path().join("a.assets/图 1-2.png")).unwrap(), image_bytes(10, 10));
    assert_eq!(std::fs::read(dir.path().join("a.assets/notes.zip")).unwrap(), b"archive");
}

/// 用户路径：点图选中 → `⌫` 删除。曾在"删末块时收掉前面的空行"处按字节切片，
/// 上一行以中文结尾就 panic（`ops::delete_units`）。附件本身留在磁盘上。
#[gpui_kit::test]
fn backspace_deletes_a_selected_image_block_after_multibyte_text(cx: &mut TestAppContext) {
    let (dir, _cache, _vault, editor) = open_with_images(cx, "上一段中文。\n\n![[photo.png]]\n");
    write_attachment(&dir, "a.assets/photo.png", 80, 40);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| {
        window.replace_root(cx, |window, cx| gpui_kit::component::Root::new(editor.clone(), window, cx));
    });
    settle(cx);
    let image = cx.debug_bounds("image-photo.png").expect("图片应已渲染");
    cx.simulate_click(image.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "上一段中文。\n");
    assert!(dir.path().join("a.assets/photo.png").is_file(), "删除嵌入不删附件文件");
}

/// 笔记 / 章节 / 块嵌入：只读渲染目标源码，来源变化跟着更新，自嵌套不会转不出来。
#[gpui_kit::test]
fn note_embeds_render_their_target_and_follow_changes(cx: &mut TestAppContext) {
    use super::embeds_ui::EmbedView;
    let body = "![[b]]\n\n![[b#二]]\n\n![[b#^k]]\n\n![[缺失的笔记]]\n\n![[a]]\n";
    let (dir, _cache, _vault, editor) = open_with_images(cx, body);
    std::fs::write(
        dir.path().join("b.md"),
        "---\ntitle: 乙\n---\n\n# 一\n\n**甲正文**\n\n## 二\n\n二正文 ^k\n",
    )
    .unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| {
        window.replace_root(cx, |window, cx| gpui_kit::component::Root::new(editor.clone(), window, cx));
        editor.update(cx, |e, cx| {
            e.install_test_resolver(&[("a.md", None), ("b.md", Some("乙"))], cx);
        });
    });
    settle(cx);

    // 四处嵌入各自渲染成带左竖线的容器（自嵌套的 a 也在，只是不再往下递归）
    for selector in ["embed-b", "embed-b#二", "embed-b#^k", "embed-缺失的笔记", "embed-a"] {
        assert!(cx.debug_bounds(selector).is_some(), "{selector} 应有嵌入容器");
    }
    assert!(cx.debug_bounds("embed-error").is_some(), "解析不到的嵌入显示源码 + 错误");

    cx.update(|_, cx| {
        let editor = editor.read(cx);
        // 整篇：去掉 front matter，标题取目标笔记的 title
        let EmbedView::Ready { title, source, .. } = editor.embed_view("b", None) else {
            panic!("整篇嵌入应就绪")
        };
        assert_eq!(title, "乙");
        assert!(source.starts_with("# 一") && source.contains("**甲正文**"), "{source:?}");
        assert!(!source.contains("title: 乙"), "front matter 不带进来");
        // 章节与块：只取对应片段
        let EmbedView::Ready { source, .. } = editor.embed_view("b", Some("二")) else {
            panic!("章节嵌入应就绪")
        };
        assert!(source.starts_with("## 二") && !source.contains("甲正文"), "{source:?}");
        let EmbedView::Ready { source, .. } = editor.embed_view("b", Some("^k")) else {
            panic!("块嵌入应就绪")
        };
        assert_eq!(source, "二正文 ^k");
        // 同篇嵌入用未保存的文档，不读盘
        assert!(matches!(editor.embed_view("a", None), EmbedView::Ready { .. }));
        assert!(matches!(editor.embed_view("缺失的笔记", None), EmbedView::Failed(_)));
    });

    // 来源笔记改了：失效后重读，嵌入跟着变
    std::fs::write(dir.path().join("b.md"), "# 一\n\n改过的正文\n").unwrap();
    cx.update(|_, cx| {
        let changed = [aero_core::VaultPath::new("b.md").unwrap()];
        editor.update(cx, |e, cx| e.invalidate_embeds(&changed, cx));
    });
    settle(cx);
    cx.update(|_, cx| {
        let EmbedView::Ready { source, .. } = editor.read(cx).embed_view("b", None) else {
            panic!("重读后应就绪")
        };
        assert!(source.contains("改过的正文"), "{source:?}");
        // 片段没了：变成错误而不是拿旧内容糊弄
        assert!(matches!(editor.read(cx).embed_view("b", Some("^k")), EmbedView::Failed(_)));
    });
}

/// `$$…$$` 块渲染成位图；语法错误显示源码 + 错误，内容不丢。
#[gpui_kit::test]
fn math_blocks_render_as_bitmaps_and_fall_back_to_source(cx: &mut TestAppContext) {
    let (_dir, _cache, _vault, editor) = open_with_images(cx, "$$\n\\int_0^1 x\\,dx\n$$\n\n正文\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| {
        window.replace_root(cx, |window, cx| gpui_kit::component::Root::new(editor.clone(), window, cx));
        window.draw(cx).clear(cx);
    });
    assert!(cx.debug_bounds("math-placeholder").is_some(), "首帧先占位");
    settle(cx);
    let bounds = cx.debug_bounds("math-block").expect("公式应渲染成位图");
    assert!(f32::from(bounds.size.height) > 4., "位图要有高度: {:?}", bounds.size);
    assert!(cx.debug_bounds("math-error").is_none());

    // 换成写坏的公式：源码 + 错误
    let (_dir, _cache, _vault, broken) = open_with_images(cx, "$$\n\\frac{\n$$\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[1], cx);
    cx.update(|window, cx| {
        window.replace_root(cx, |window, cx| gpui_kit::component::Root::new(broken.clone(), window, cx));
    });
    settle(cx);
    assert!(cx.debug_bounds("math-source").is_some(), "渲染不了要显示源码");
    assert!(cx.debug_bounds("math-error").is_some(), "并给出错误");
    assert!(cx.debug_bounds("math-block").is_none());
}

/// 段落里的 `$…$` 渲染成行内位图，代码里的美元符号保持源码。
#[gpui_kit::test]
fn inline_math_renders_inside_a_paragraph(cx: &mut TestAppContext) {
    let (_dir, _cache, _vault, editor) = open_with_images(cx, "能量 $E=mc^2$ 守恒\n\n` $E$ `\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| {
        window.replace_root(cx, |window, cx| gpui_kit::component::Root::new(editor.clone(), window, cx));
    });
    settle(cx);
    let inline = cx.debug_bounds("math-inline-0").expect("行内公式应渲染成位图");
    assert!(cx.debug_bounds("math-inline-1").is_none(), "代码里的 $ 不是公式");
    cx.simulate_click(inline.center(), gpui_kit::Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("math-editor").is_some(), "点公式位图在弹窗里改");
    let again = cx.debug_bounds("math-inline-0").expect("行内位图还在原来的段落里");
    assert!((again.origin.y - inline.origin.y).abs() < gpui_kit::px(1.));

    // 点文字只编辑公式外面的字，源码不进段落。
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    focus_unit(&editor, 0, cx);
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("math-inline-0").is_some(), "点文字后公式仍是位图");
    let prose = cx.debug_bounds("math-text").expect("公式旁边的文字要留在版面上");
    assert!(f32::from(prose.size.width) > 8., "文字不能被挤成 0 宽: {:?}", prose.size);
    assert!(f32::from(prose.size.height) > 8., "文字不能被挤成 0 高: {:?}", prose.size);
    cx.update(|_, cx| {
        let value = editor.read(cx).focused.as_ref().unwrap().textarea.read(cx).value().to_string();
        assert!(!value.contains('$'), "段落文本框里不能出现公式源码: {value}");
        assert!(!value.contains("E=mc^2"), "段落文本框里不能出现公式源码: {value}");
        assert!(value.contains('能') || value.contains('恒'), "文本框里应仍是公式外面的字: {value}");
    });
}

/// 点进标题、段落、引用或列表时，其它块的位置不能变。
#[gpui_kit::test]
fn focusing_a_block_keeps_the_following_block_in_place(cx: &mut TestAppContext) {
    use gpui_kit::{Bounds, Pixels, px, size};
    let source = "# 标题一\n\n这是一段很长的正文，用来看点击之后会不会跳。这是一段很长的正文，用来看点击之后会不会跳。这是一段很长的正文，用来看点击之后会不会跳。这是一段很长的正文，用来看点击之后会不会跳。\n\n## 小标题\n\n> 引用一行\n\n- 列表项\n";
    let (_dir, _vault, _path, editor) = open(cx, source);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.simulate_resize(size(px(1280.), px(900.)));
    cx.update(|window, cx| {
        window.replace_root(cx, |window, cx| gpui_kit::component::Root::new(editor.clone(), window, cx));
    });
    settle(cx);
    let ids: Vec<u64> = cx.update(|_, cx| editor.read(cx).units.iter().map(|u| u.id.0).collect());
    let bounds = |cx: &mut VisualTestContext, id: u64| -> Option<Bounds<Pixels>> {
        let selector: &'static str = Box::leak(format!("unit-{id}").into_boxed_str());
        cx.debug_bounds(selector)
    };
    let before: Vec<_> = ids.iter().map(|id| bounds(cx, *id).expect("unit")).collect();
    for ix in 0..ids.len() {
        focus_unit(&editor, ix, cx);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        for (i, id) in ids.iter().enumerate() {
            if i == ix {
                continue;
            }
            let after = bounds(cx, *id).expect("unit");
            assert!(
                (after.origin.y - before[i].origin.y).abs() < px(1.),
                "聚焦第 {ix} 块后，第 {i} 块从 {:?} 跳到了 {:?}",
                before[i].origin.y,
                after.origin.y
            );
        }
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }
}

/// 编辑公式时正文里仍是原来的位图，源码在弹窗里，后面的段落不挪位。
#[gpui_kit::test]
fn editing_a_math_block_keeps_a_live_preview(cx: &mut TestAppContext) {
    use gpui_kit::px;
    let (_dir, _cache, _vault, editor) = open_with_images(cx, "$$\n\\int_0^1 x\\,dx\n$$\n\n后面的段落\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.update(|window, cx| {
        window.replace_root(cx, |window, cx| gpui_kit::component::Root::new(editor.clone(), window, cx));
    });
    settle(cx);
    let formula = cx.debug_bounds("math-block").expect("公式应渲染成位图");
    let ids: Vec<u64> = cx.update(|_, cx| editor.read(cx).units.iter().map(|unit| unit.id.0).collect());
    let next_selector: &'static str = Box::leak(format!("unit-{}", ids[1]).into_boxed_str());
    let next = cx.debug_bounds(next_selector).expect("公式后面的段落");
    focus_unit(&editor, 0, cx);
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.update(|_, cx| editor.read(cx).is_editing()), "应进入编辑");
    assert!(cx.debug_bounds("math-editor").is_some(), "源码在弹窗里");
    let again = cx.debug_bounds("math-block").expect("文档里仍是位图");
    assert!((again.origin.y - formula.origin.y).abs() < px(1.), "公式垂直位置不变");
    assert!((again.size.height - formula.size.height).abs() < px(1.), "公式高度不变");
    let next_again = cx.debug_bounds(next_selector).expect("段落还在");
    assert!((next_again.origin.y - next.origin.y).abs() < px(1.), "后面的段落不能因为弹窗而挪动");
    cx.simulate_input("+1");
    assert!(text(&editor, cx).contains("+1"), "弹窗里的修改写回公式");
}

#[gpui_kit::test]
fn local_parse_retains_units_and_drifts_nested_list_ranges(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "first paragraph\n\n- item\n  - nested\n\nlast paragraph\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    editor.update(cx, |e, cx| {
        let units = e.units.as_ptr();
        let structure = e.unit_structure.as_ptr();
        e.doc.edit(aero_core::Edit::insert(5, "中文")).unwrap();
        e.focused.as_mut().unwrap().range.end += "中文".len();
        e.after_doc_change(cx);
        assert!(!e.doc.is_parse_stale());
        assert_eq!(e.units.as_ptr(), units);
        assert_eq!(e.unit_structure.as_ptr(), structure);
        assert_eq!(e.units, super::units::units(&e.doc));
    });
}

fn large_source() -> String {
    format!("intro\n\n{}", ("other paragraph with words ".repeat(40) + "\n\n").repeat(300))
}

#[gpui_kit::test]
fn large_syntax_edits_coalesce_and_keep_the_input_entity(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, &large_source());
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    let input = editor.read_with(cx, |e, _| e.focused.as_ref().unwrap().textarea.clone());
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            e.parse_window = Some(window.window_handle());
            e.doc.edit(aero_core::Edit::insert(0, "## ")).unwrap();
            e.focused.as_mut().unwrap().range.end += 3;
            e.after_doc_change(cx);
            assert!(e.doc.is_parse_stale(), "the input callback does not perform full parsing");
            assert!(e.parse_task.is_some());
            e.doc.edit(aero_core::Edit::insert(3, "中文")).unwrap();
            e.focused.as_mut().unwrap().range.end += "中文".len();
            e.after_doc_change(cx);
            assert!(e.doc.is_parse_stale());
        })
    });
    cx.run_until_parked();
    editor.read_with(cx, |e, _| {
        assert!(!e.doc.is_parse_stale());
        assert!(e.parse_task.is_none());
        assert_eq!(e.units[0].kind, aero_core::BlockKind::Heading { level: 2 });
        assert_eq!(e.units, super::units::units(&e.doc));
        assert_eq!(e.focused.as_ref().unwrap().textarea, input);
        assert_eq!(e.focused.as_ref().unwrap().id, Some(e.units[0].id));
        assert!(e.doc.to_string().starts_with("## 中文intro"));
    });
}

#[gpui_kit::test]
fn reload_rejects_a_parse_from_the_previous_session(cx: &mut TestAppContext) {
    let (_dir, vault, path, editor) = open(cx, &large_source());
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    editor.update(cx, |e, cx| {
        e.doc.edit(aero_core::Edit::insert(0, "## ")).unwrap();
        e.focused.as_mut().unwrap().range.end += 3;
        e.after_doc_change(cx);
        assert!(e.parse_task.is_some());
        let stamp = vault.write_bytes(&path, b"replacement\n").unwrap();
        e.install_document(aero_core::Document::from_text("replacement\n"), stamp, cx);
    });
    cx.run_until_parked();
    assert_eq!(text(&editor, cx), "replacement\n");
    editor.read_with(cx, |e, _| assert!(!e.doc.is_parse_stale()));
}

#[gpui_kit::test]
fn large_wiki_query_cancels_old_work_and_defers_enter(cx: &mut TestAppContext) {
    let (_dir, _vault, _path, editor) = open(cx, "body\n");
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    focus_unit(&editor, 0, cx);
    cx.simulate_input(" [[Target");
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            let mut notes = (0..300)
                .map(|i| aero_core::links::NoteSummary {
                    path: VaultPath::new(&format!("note-{i}.md")).unwrap(),
                    title: format!("Target {i}"),
                    aliases: vec![],
                })
                .collect::<Vec<_>>();
            notes.push(aero_core::links::NoteSummary {
                path: e.path.clone(),
                title: "Target Z".into(),
                aliases: vec![],
            });
            let task = cx.background_spawn(async move { Ok(notes) });
            e.load_wiki_notes(e.wiki.as_ref().unwrap().generation, task, vec![], window, cx);
        })
    });
    cx.run_until_parked();
    let cancelled =
        editor.read_with(cx, |e, _| e.wiki.as_ref().unwrap().filter_cancel.as_ref().unwrap().0.clone());
    cx.simulate_input(" Z");
    assert!(cancelled.load(std::sync::atomic::Ordering::Relaxed));
    cx.simulate_keystrokes("enter");
    editor.read_with(cx, |e, _| assert!(e.wiki.as_ref().unwrap().pending_confirmation.is_some()));
    cx.executor().advance_clock(Duration::from_millis(50));
    cx.run_until_parked();
    // Current unsaved metadata replaces the supplied title, so no result is a
    // deliberate unresolved link; Enter must wait for this query, never Target 0.
    assert_eq!(text(&editor, cx), "body [[Target Z]]\n");
}
