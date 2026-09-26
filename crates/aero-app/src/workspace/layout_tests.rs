//! 标签页、收藏、三栏宽度与分割线的回归（`ui_design.md` 2.1 / 2.3 / 4.1）。

use aero_core::VaultPath;
use gpui_kit::{
    App, Entity, Modifiers, MouseButton, MouseDownEvent, TestAppContext, VisualTestContext, point, px, size,
};

use super::tests::{open, wait_until};
use super::{SidebarTab, Workspace};
use crate::actions::{
    ActivateTab, CloseOtherTabs, CloseTab, NewTab, NextTab, PrevTab, ReopenClosedTab, ToggleSidebar,
};

fn note(p: &str) -> VaultPath {
    VaultPath::new(p).unwrap()
}

fn tab_paths(w: &Workspace, cx: &App) -> Vec<String> {
    let open = w.open.as_ref().unwrap();
    open.tabs.iter().map(|t| t.path(cx).map(|p| p.as_str().to_owned()).unwrap_or_default()).collect()
}

fn active(w: &Workspace) -> usize {
    w.open.as_ref().unwrap().active
}

fn with_root(cx: &mut VisualTestContext, workspace: &Entity<Workspace>) {
    cx.update(|window, cx| {
        window.replace_root(cx, |window, cx| gpui_kit::component::Root::new(workspace.clone(), window, cx));
    });
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
}

fn close_index(cx: &mut VisualTestContext, workspace: &Entity<Workspace>) {
    with_root(cx, workspace);
    cx.run_until_parked();
    cx.update(|_, cx| {
        if let Some(open) = workspace.read(cx).open.as_ref() {
            open.search.read(cx).close_blocking();
        }
    });
}

#[gpui_kit::test]
fn opening_tabs_defers_io_and_discards_replaced_loads(cx: &mut TestAppContext) {
    let (dir, _data, workspace) = open(cx, Some("# A\n"));
    std::fs::write(dir.path().join("b.md"), "# B\n\nsecond\n").unwrap();
    std::fs::write(dir.path().join("c.md"), "# C\n\nthird\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    workspace.update_in(cx, |w, window, cx| {
        w.open_hit(note("b.md"), 1, true, window, cx);
        assert!(w.open.as_ref().unwrap().editor().is_none(), "opening must not read/parse on UI");
        w.open_hit(note("c.md"), 1, true, window, cx);
        assert_eq!(tab_paths(w, cx), ["a.md", "c.md"]);
        w.activate_tab(0, window, cx);
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
        let w = workspace.read(cx);
        assert_eq!(active(w), 0, "background completion must not steal the active tab");
        let loaded = w.open.as_ref().unwrap().tabs[1].editor.as_ref().unwrap().read(cx);
        assert_eq!(loaded.path(), &note("c.md"));
        assert!(loaded.highlighted_block().is_some(), "search jump survives asynchronous loading");
    });
    close_index(cx, &workspace);
}

#[gpui_kit::test]
fn closing_a_dirty_tab_waits_for_the_latest_save(cx: &mut TestAppContext) {
    let (dir, _data, workspace) = open(cx, Some("body\n"));
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    with_root(cx, &workspace);
    draw(cx);
    let editor = cx.update(|window, cx| {
        let editor = workspace.read(cx).open.as_ref().unwrap().editor().unwrap().clone();
        editor.update(cx, |e, cx| e.start_writing(window, cx));
        editor
    });
    cx.simulate_input("first");
    workspace.update_in(cx, |w, window, cx| {
        w.close_tab(0, window, cx);
        assert_eq!(w.open.as_ref().unwrap().editor(), Some(&editor), "retain editor until save completes");
    });
    cx.simulate_input("latest");
    cx.run_until_parked();
    cx.update(|_, cx| assert_eq!(tab_paths(workspace.read(cx), cx), [""]));
    assert!(std::fs::read_to_string(dir.path().join("a.md")).unwrap().contains("latest"));
    close_index(cx, &workspace);
}

#[gpui_kit::test]
fn vault_close_keeps_the_session_until_saves_and_indexes_finish(cx: &mut TestAppContext) {
    let (dir, _data, workspace) = open(cx, Some("body\n"));
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    with_root(cx, &workspace);
    draw(cx);
    cx.update(|window, cx| {
        let editor = workspace.read(cx).open.as_ref().unwrap().editor().unwrap().clone();
        editor.update(cx, |e, cx| e.start_writing(window, cx));
    });
    cx.simulate_input("saved before leaving");
    workspace.update_in(cx, |w, window, cx| {
        w.on_close_vault(&crate::actions::CloseVault, window, cx);
        assert!(w.open.is_some(), "the session must own all outstanding saves and native cleanup");
    });
    cx.run_until_parked();
    cx.update(|_, cx| assert!(workspace.read(cx).open.is_none()));
    assert!(std::fs::read_to_string(dir.path().join("a.md")).unwrap().contains("saved before leaving"));
}

#[gpui_kit::test]
fn load_completion_cannot_resurrect_a_closed_tab(cx: &mut TestAppContext) {
    let (dir, _data, workspace) = open(cx, Some("body\n"));
    std::fs::write(dir.path().join("b.md"), "# B\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    workspace.update_in(cx, |w, window, cx| {
        w.open_note(note("b.md"), window, cx);
        w.close_tab(1, window, cx);
        assert_eq!(tab_paths(w, cx), ["a.md"]);
    });
    cx.run_until_parked();
    cx.update(|_, cx| assert_eq!(tab_paths(workspace.read(cx), cx), ["a.md"]));
    close_index(cx, &workspace);
}

#[gpui_kit::test]
fn document_references_follow_body_instead_of_header_or_sidebar(cx: &mut TestAppContext) {
    let (_dir, _data, workspace) = open(cx, Some("# A\n\n正文尾部\n"));
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    with_root(cx, &workspace);
    draw(cx);
    let footer = cx.debug_bounds("document-references");
    let old_count = cx.debug_bounds("backlink-count");
    let last_block = cx.debug_bounds("unit-2").unwrap();
    close_index(cx, &workspace);
    assert!(old_count.is_none(), "no backlink count above the document");
    let footer = footer.expect("reference information belongs below the document");
    assert!(footer.top() >= last_block.bottom());
    assert!(cx.debug_bounds("context-blank").is_some(), "the right pane stays, with nothing in it yet");
    assert!(cx.debug_bounds("outline-rows").is_none(), "the outline list has moved off this pane");
}

#[gpui_kit::test]
fn long_document_scrolls_to_references_and_opens_outgoing_note(cx: &mut TestAppContext) {
    let source = format!("# A\n\n{}[去目标](b.md#目标:说明)\n", "正文段落\n\n".repeat(40));
    let (dir, _data, workspace) = open(cx, Some(&source));
    std::fs::write(dir.path().join("b.md"), "# 目标:说明\n\n[[a]]\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    with_root(cx, &workspace);
    cx.simulate_resize(size(px(1100.), px(640.)));
    draw(cx);
    let (search, backlinks) = cx.update(|_, cx| {
        let open = workspace.read(cx).open.as_ref().unwrap();
        (open.search.clone(), open.backlinks.clone())
    });
    wait_until(cx, |cx| cx.update(|_, cx| search.read(cx).phase() == crate::search::Phase::Idle));
    cx.update(|_, cx| search.update(cx, |search, cx| search.note_changed(note("b.md"), cx)));
    wait_until(cx, |cx| cx.update(|_, cx| backlinks.read(cx).total() == Some(1)));
    draw(cx);
    let viewport = cx.debug_bounds("editor").unwrap();
    let footer = cx.debug_bounds("document-references").unwrap();
    assert!(footer.top() > viewport.bottom(), "long notes keep references below the fold");
    workspace.update_in(cx, |w, window, cx| w.on_show_backlinks(&crate::actions::ShowBacklinks, window, cx));
    draw(cx);
    let footer = cx.debug_bounds("document-references").unwrap();
    assert!(footer.top() < viewport.bottom() && footer.top() >= viewport.top());
    let tab = cx.debug_bounds("references-tab-outgoing").unwrap();
    cx.simulate_click(tab.center(), Modifiers::default());
    wait_until(cx, |cx| {
        cx.executor().advance_clock(std::time::Duration::from_millis(200));
        draw(cx);
        cx.debug_bounds("outgoing-link-0").is_some()
    });
    let outgoing = cx.debug_bounds("outgoing-link-0").unwrap();
    assert!(outgoing.top() < viewport.bottom());
    cx.simulate_click(outgoing.center(), Modifiers::default());
    wait_until(cx, |cx| {
        cx.update(|_, cx| {
            workspace.read(cx).open.as_ref().unwrap().editor().unwrap().read(cx).path() == &note("b.md")
        })
    });
    assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), source);
    close_index(cx, &workspace);
}

#[gpui_kit::test]
fn status_vault_path_click_copies_the_full_directory(cx: &mut TestAppContext) {
    let (dir, _data, workspace) = open(cx, Some("# A\n"));
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    with_root(cx, &workspace);
    close_index(cx, &workspace);
    let expected = dir.path().canonicalize().unwrap().display().to_string();
    for width in [1000., 480.] {
        cx.simulate_resize(size(px(width), px(640.)));
        draw(cx);
        cx.update(|_, cx| cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string("unchanged".into())));
        let status = cx.debug_bounds("status-bar").unwrap();
        cx.simulate_click(point(status.left() + px(20.), status.center().y), Modifiers::default());
        draw(cx);
        let copied = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(copied.as_deref(), Some(expected.as_str()), "copy the full path even when truncated");
    }
    assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), "# A\n");
}

#[gpui_kit::test]
fn each_note_opens_in_its_own_tab_and_opening_it_again_switches_back(cx: &mut TestAppContext) {
    let (dir, _data, workspace) = open(cx, Some("# A\n"));
    std::fs::write(dir.path().join("b.md"), "# B\n").unwrap();
    std::fs::write(dir.path().join("c.md"), "# C\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    workspace.update_in(cx, |w, window, cx| {
        w.open_note(note("b.md"), window, cx);
        assert_eq!(tab_paths(w, cx), ["a.md", "b.md"], "新笔记在右边新开一页");
        assert_eq!(active(w), 1);
        w.open_note(note("a.md"), window, cx);
        assert_eq!(tab_paths(w, cx), ["a.md", "b.md"], "已经打开的不再开一页");
        assert_eq!(active(w), 0);

        w.on_new_tab(&NewTab, window, cx);
        assert_eq!(tab_paths(w, cx), ["a.md", "", "b.md"]);
        assert!(w.open.as_ref().unwrap().editor().is_none(), "新标签页是空白页");
        w.open_note(note("b.md"), window, cx);
        assert_eq!((tab_paths(w, cx).len(), active(w)), (3, 2), "空白页开着时，已打开的仍然切过去");
        w.activate_tab(1, window, cx);
        w.open_note(note("c.md"), window, cx);
        assert_eq!(tab_paths(w, cx), ["a.md", "c.md", "b.md"], "新笔记放进空白页");

        w.on_activate_tab(&ActivateTab(usize::MAX), window, cx);
        assert_eq!(active(w), 2, "⌘9 是最后一页");
        w.on_next_tab(&NextTab, window, cx);
        assert_eq!(active(w), 0, "下一页从最后一页绕回第一页");
        w.on_prev_tab(&PrevTab, window, cx);
        assert_eq!(active(w), 2);
    });
    close_index(cx, &workspace);
}

#[gpui_kit::test]
fn search_preview_reuses_one_tab_until_it_is_confirmed_or_edited(cx: &mut TestAppContext) {
    let (dir, _data, workspace) = open(cx, Some("# A\n"));
    for name in ["b", "c", "d"] {
        std::fs::write(dir.path().join(format!("{name}.md")), format!("# {name}\n")).unwrap();
    }
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    with_root(cx, &workspace);
    workspace.update_in(cx, |w, window, cx| {
        w.open_hit(note("b.md"), 0, true, window, cx);
        w.open_hit(note("c.md"), 0, true, window, cx);
        assert_eq!(tab_paths(w, cx), ["a.md", "c.md"], "↑↓ 预览一直用同一页");
        assert!(w.open.as_ref().unwrap().tabs[1].preview);
        w.open_hit(note("c.md"), 0, false, window, cx);
        assert!(!w.open.as_ref().unwrap().tabs[1].preview, "⏎ 之后预览页变成普通页");
        w.open_hit(note("d.md"), 0, true, window, cx);
        assert_eq!(tab_paths(w, cx), ["a.md", "c.md", "d.md"]);
    });
    cx.run_until_parked();
    workspace.update_in(cx, |w, window, cx| {
        let editor = w.open.as_ref().unwrap().editor().cloned().unwrap();
        editor.update(cx, |e, cx| e.start_writing(window, cx));
    });
    cx.simulate_input("!");
    cx.run_until_parked();
    workspace.update_in(cx, |w, window, cx| {
        assert!(!w.open.as_ref().unwrap().tabs[2].preview, "编辑过的预览页留下来");
        w.open_hit(note("b.md"), 0, true, window, cx);
        assert_eq!(tab_paths(w, cx), ["a.md", "c.md", "d.md", "b.md"]);
    });
    cx.run_until_parked();
    assert!(std::fs::read_to_string(dir.path().join("d.md")).unwrap().contains('!'), "离开的那页已异步落盘");
    close_index(cx, &workspace);
}

#[gpui_kit::test]
fn closing_tabs_moves_to_the_neighbour_and_reopen_brings_them_back(cx: &mut TestAppContext) {
    let (dir, _data, workspace) = open(cx, Some("# A\n"));
    std::fs::write(dir.path().join("b.md"), "# B\n").unwrap();
    std::fs::write(dir.path().join("c.md"), "# C\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    workspace.update_in(cx, |w, window, cx| {
        w.open_note(note("b.md"), window, cx);
        w.open_note(note("c.md"), window, cx);
        w.activate_tab(1, window, cx);
        w.on_close_tab(&CloseTab, window, cx);
        assert_eq!(tab_paths(w, cx), ["a.md", "c.md"]);
        assert_eq!(active(w), 1, "关掉当前页停在右边那页");
        w.on_reopen_closed_tab(&ReopenClosedTab, window, cx);
        assert_eq!(tab_paths(w, cx), ["a.md", "c.md", "b.md"]);
        assert_eq!(active(w), 2);

        w.on_close_other_tabs(&CloseOtherTabs, window, cx);
        assert_eq!(tab_paths(w, cx), ["b.md"]);
        w.on_close_tab(&CloseTab, window, cx);
        assert_eq!(tab_paths(w, cx), [""], "关掉最后一页留一张空白页");
        w.on_close_tab(&CloseTab, window, cx);
        assert_eq!(tab_paths(w, cx), [""]);
        w.on_reopen_closed_tab(&ReopenClosedTab, window, cx);
        assert_eq!(tab_paths(w, cx), ["b.md"], "重开最近关的，空白页被用掉");
    });
    close_index(cx, &workspace);
}

#[gpui_kit::test]
fn switching_tabs_saves_the_one_left_and_keeps_its_editor(cx: &mut TestAppContext) {
    let (dir, _data, workspace) = open(cx, Some("# A\n"));
    std::fs::write(dir.path().join("b.md"), "# B\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    with_root(cx, &workspace);
    let first = cx.update(|window, cx| {
        let editor = workspace.read(cx).open.as_ref().unwrap().editor().cloned().unwrap();
        editor.update(cx, |e, cx| e.start_writing(window, cx));
        editor
    });
    cx.simulate_input("!");
    workspace.update_in(cx, |w, window, cx| w.open_note(note("b.md"), window, cx));
    cx.run_until_parked();
    assert!(std::fs::read_to_string(dir.path().join("a.md")).unwrap().contains('!'), "切走后异步落盘");
    workspace.update_in(cx, |w, window, cx| {
        assert_eq!(w.open.as_ref().unwrap().tabs[0].editor.as_ref(), Some(&first), "没显示的页保留编辑器");
        w.activate_tab(0, window, cx);
        assert_eq!(w.open.as_ref().unwrap().editor(), Some(&first));
    });
    close_index(cx, &workspace);
}

#[gpui_kit::test]
fn tabs_and_favorites_come_back_after_a_restart(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::actions::init(cx);
    });
    let data = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    for name in ["a.md", "b.md", "sub/c.md"] {
        std::fs::write(dir.path().join(name), "# 笔记\n").unwrap();
    }
    // 启动时打开最近修改的一篇：让它确定是 a.md
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    std::fs::File::options().write(true).open(dir.path().join("a.md")).unwrap().set_modified(later).unwrap();
    let root = dir.path().to_path_buf();
    let (first, cx) = cx.add_window_view({
        let (root, data) = (root.clone(), data.path().to_path_buf());
        move |window, cx| Workspace::new(Some(root), data, window, cx)
    });
    cx.run_until_parked();
    let (tabs, favorites) = first.update_in(cx, |w, window, cx| {
        w.open_note(note("b.md"), window, cx);
        w.open_note(note("sub/c.md"), window, cx);
        w.activate_tab(1, window, cx);
        w.toggle_favorite(note("sub/c.md"), cx);
        w.flush_all(cx);
        (tab_paths(w, cx), w.favorites())
    });
    assert_eq!(tabs, ["a.md", "b.md", "sub/c.md"]);

    let (second, cx) = cx.add_window_view({
        let data = data.path().to_path_buf();
        move |window, cx| Workspace::new(Some(root), data, window, cx)
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
        let w = second.read(cx);
        assert_eq!(tab_paths(w, cx), tabs);
        assert_eq!(active(w), 1);
        assert_eq!(w.favorites(), favorites);
        let open = w.open.as_ref().unwrap();
        assert!(open.tabs[0].editor.is_none() && open.tabs[2].editor.is_none(), "没切到的页不加载");
        open.search.read(cx).close_blocking();
    });
}

#[gpui_kit::test]
fn favorites_follow_renames_and_drop_trashed_notes(cx: &mut TestAppContext) {
    let (dir, _data, workspace) = open(cx, Some("# A\n"));
    std::fs::write(dir.path().join("b.md"), "# B\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    with_root(cx, &workspace);
    workspace.update_in(cx, |w, _, cx| {
        let tree = w.open.as_ref().unwrap().tree.clone();
        tree.update(cx, |t, _| t.rescan());
        w.toggle_favorite(note("a.md"), cx);
        w.toggle_favorite(note("b.md"), cx);
        assert_eq!(w.favorites(), [note("a.md"), note("b.md")]);
    });
    draw(cx);
    assert!(cx.debug_bounds("file-tree-favorite:b.md").is_some());
    workspace.update_in(cx, |w, window, cx| w.move_entry(note("a.md"), note("renamed.md"), window, cx));
    wait_until(cx, |cx| cx.update(|_, cx| workspace.read(cx).favorites().contains(&note("renamed.md"))));
    workspace.update_in(cx, |w, window, cx| {
        assert_eq!(tab_paths(w, cx), ["renamed.md"], "标签页跟着改名");
        w.trash_note(&note("b.md"), window, cx);
        assert_eq!(w.favorites(), [note("renamed.md")], "移进废纸篓的不再留在收藏里");
    });
    close_index(cx, &workspace);
}

fn open_long_title_tabs(
    cx: &mut TestAppContext,
) -> (tempfile::TempDir, tempfile::TempDir, Entity<Workspace>) {
    let source = format!("# {}\n", "很长的标签标题 Long-unbroken-document-title ".repeat(12));
    let (dir, data, workspace) = open(cx, Some(&source));
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    with_root(cx, &workspace);
    for ix in 1..12 {
        let path = format!("long-title-{ix}.md");
        std::fs::write(dir.path().join(&path), &source).unwrap();
        workspace.update_in(cx, |w, window, cx| w.open_note(note(&path), window, cx));
    }
    workspace.update_in(cx, |w, window, cx| w.activate_tab(0, window, cx));
    close_index(cx, &workspace);
    (dir, data, workspace)
}

fn assert_title_alignment(cx: &mut VisualTestContext, case: &str) {
    let (viewport, fullscreen) = cx.update(|window, _| (window.viewport_size(), window.is_fullscreen()));
    let title = cx.debug_bounds("title-content").unwrap();
    let right = cx.debug_bounds("title-panel").unwrap();
    let strip = cx.debug_bounds("tab-strip").unwrap();
    let tabs = cx.debug_bounds("tabs-viewport").unwrap();
    let new_tab = cx.debug_bounds("new-tab-button").unwrap();
    if let Some(panel) = cx.debug_bounds("context-panel") {
        let editor = cx.debug_bounds("editor-column").unwrap();
        assert_eq!(right.left(), panel.left(), "{case}: title/body panel boundary");
        assert_eq!(right.left(), editor.right(), "{case}: title/editor boundary");
        assert_eq!(right.size.width, panel.size.width, "{case}: panel widths");
    }
    assert_eq!(right.right(), viewport.width, "{case}: right controls stay at window edge");
    assert_eq!(title.right(), viewport.width, "{case}: title content stays inside window");
    // gpui-component's inner `bar` adds pl_3 in fullscreen (12 px at the default rem size).
    assert_eq!(title.left(), px(if fullscreen { 12. } else { 0. }), "{case}: native bar inset");
    if let Some(sidebar) = cx.debug_bounds("sidebar") {
        let left = cx.debug_bounds("title-left").unwrap();
        assert_eq!(left.right(), sidebar.right(), "{case}: sidebar boundary");
    }
    assert_eq!(strip.right(), right.left(), "{case}: tabs cannot push the panel");
    assert!(tabs.size.width > px(0.) && tabs.right() <= new_tab.left(), "{case}: tabs scroll separately");
    assert!(new_tab.right() <= right.left(), "{case}: new-tab does not overlap panel");
    for selector in ["new-tab-button", "panel-toggle-button"] {
        let button = cx.debug_bounds(selector).unwrap();
        assert!(button.size.width > px(0.) && button.size.height > px(0.), "{case}: {selector} has size");
        assert!(
            button.left() >= px(0.)
                && button.right() <= viewport.width
                && button.top() >= title.top()
                && button.bottom() <= title.bottom(),
            "{case}: {selector} is fully inside the title bar: {button:?}"
        );
    }
}

#[gpui_kit::test]
fn title_panel_stays_aligned_with_many_long_tabs_across_window_sizes(cx: &mut TestAppContext) {
    let (_dir, _data, workspace) = open_long_title_tabs(cx);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    for fullscreen in [false, true, false] {
        cx.update(|window, _| {
            if window.is_fullscreen() != fullscreen {
                window.toggle_fullscreen();
            }
        });
        for sidebar in [true, false] {
            for panel in [220., 300., 380.] {
                workspace.update_in(cx, |w, _, cx| {
                    w.sidebar_visible = sidebar;
                    w.panel_width = px(panel);
                    cx.notify();
                });
                for width in [1400., 1100., 800., 640., 1400.] {
                    cx.simulate_resize(size(px(width), px(800.)));
                    draw(cx);
                    assert_title_alignment(
                        cx,
                        &format!("width={width}, sidebar={sidebar}, panel={panel}, fullscreen={fullscreen}"),
                    );
                }
            }
        }
    }
}

#[gpui_kit::test]
fn title_tabs_scroll_and_controls_work_while_the_panel_is_resized(cx: &mut TestAppContext) {
    use gpui_kit::{ScrollDelta, ScrollWheelEvent, TouchPhase};

    let (_dir, _data, workspace) = open_long_title_tabs(cx);
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    cx.simulate_resize(size(px(1100.), px(800.)));
    draw(cx);
    assert_title_alignment(cx, "before scrolling");
    let tabs = cx.debug_bounds("tabs-viewport").unwrap();
    for (dx, selector) in [(-5000., "tab:11"), (5000., "tab:0")] {
        cx.simulate_event(ScrollWheelEvent {
            position: tabs.center(),
            delta: ScrollDelta::Pixels(point(px(dx), px(0.))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Started,
        });
        draw(cx);
        let tab = cx.debug_bounds(selector).expect("scrolled tab is rendered");
        assert!(tab.left() >= tabs.left() && tab.right() <= tabs.right(), "{selector} is reachable");
        assert_title_alignment(cx, "after scrolling");
    }
    for sidebar in [true, false] {
        workspace.update_in(cx, |w, _, cx| {
            w.sidebar_visible = sidebar;
            cx.notify();
        });
        draw(cx);
        let start = cx.debug_bounds("split-panel").unwrap().center();
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        draw(cx);
        for dx in [-40., -80., 80., 0.] {
            cx.simulate_mouse_move(start + point(px(dx), px(0.)), MouseButton::Left, Modifiers::default());
            draw(cx);
            assert_title_alignment(cx, &format!("drag dx={dx}, sidebar={sidebar}"));
        }
        cx.simulate_mouse_up(start, MouseButton::Left, Modifiers::default());
        draw(cx);
    }
    cx.simulate_resize(size(px(640.), px(800.)));
    draw(cx);
    let new_tab = cx.debug_bounds("new-tab-button").unwrap();
    cx.simulate_click(new_tab.center(), Modifiers::default());
    draw(cx);
    assert_eq!(cx.update(|_, cx| workspace.read(cx).open.as_ref().unwrap().tabs.len()), 13);
    assert_title_alignment(cx, "after new tab");
    for visible in [false, true] {
        let toggle = cx.debug_bounds("panel-toggle-button").unwrap();
        cx.simulate_click(toggle.center(), Modifiers::default());
        draw(cx);
        assert_eq!(cx.update(|_, cx| workspace.read(cx).panel_visible), visible);
        assert_title_alignment(cx, "after toggling panel");
    }
}

#[gpui_kit::test]
fn side_columns_keep_their_width_and_the_editor_absorbs_window_changes(cx: &mut TestAppContext) {
    let (_dir, _data, workspace) = open(cx, Some("# A\n"));
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    with_root(cx, &workspace);
    let widths = |cx: &mut VisualTestContext, width: f32| {
        cx.simulate_resize(size(px(width), px(800.)));
        draw(cx);
        let sidebar = cx.debug_bounds("sidebar").unwrap().size.width;
        let panel = cx.debug_bounds("context-panel").unwrap().size.width;
        let editor = cx.debug_bounds("editor-column").unwrap().size.width;
        assert_eq!(sidebar + panel + editor, px(width), "三栏正好铺满");
        let status = cx.debug_bounds("status-bar").expect("状态栏");
        assert!(status.bottom() <= px(800.), "状态栏在窗口里");
        (sidebar, panel)
    };
    assert_eq!(widths(cx, 1400.), (px(260.), px(300.)));
    assert_eq!(widths(cx, 1100.), (px(260.), px(300.)), "窗口变窄只压编辑区");
    // 260 + 300 + 360 放不下 800：先把面板压到 220，再压左侧栏
    assert_eq!(widths(cx, 800.), (px(220.), px(220.)));
    assert_eq!(widths(cx, 1400.), (px(260.), px(300.)), "放大后原样回来");
    close_index(cx, &workspace);
}

#[gpui_kit::test]
fn dragging_a_divider_resizes_collapses_past_the_minimum_and_double_click_resets(cx: &mut TestAppContext) {
    let (_dir, _data, workspace) = open(cx, Some("# A\n"));
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    with_root(cx, &workspace);
    cx.simulate_resize(size(px(1400.), px(800.)));
    draw(cx);
    let drag = |cx: &mut VisualTestContext, handle: &'static str, path: &[f32]| {
        draw(cx);
        let start = cx.debug_bounds(handle).expect(handle).center();
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        draw(cx);
        for dx in path {
            cx.simulate_mouse_move(start + point(px(*dx), px(0.)), MouseButton::Left, Modifiers::default());
            draw(cx);
        }
        let end = start + point(px(*path.last().unwrap()), px(0.));
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
        draw(cx);
    };
    let state = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            let w = workspace.read(cx);
            (w.sidebar_visible, w.sidebar_width, w.panel_visible, w.panel_width)
        })
    };

    drag(cx, "split-sidebar", &[40., 100.]);
    assert_eq!(state(cx), (true, px(360.), true, px(300.)), "拖多少宽多少");
    assert_eq!(cx.debug_bounds("sidebar").unwrap().size.width, px(360.));

    // 左侧栏收起时拖右边的线：面板只变宽拖过的那么多（原来会跳到上限）
    workspace.update_in(cx, |w, window, cx| w.on_toggle_sidebar(&ToggleSidebar, window, cx));
    drag(cx, "split-panel", &[-40., -80.]);
    assert_eq!(state(cx), (false, px(360.), true, px(380.)));
    workspace.update_in(cx, |w, window, cx| w.on_toggle_sidebar(&ToggleSidebar, window, cx));

    // 往里拖过最小宽度一段就收起；同一次拖动拖回来又展开；在收起状态松手，下次展开回到拖之前的宽度
    drag(cx, "split-sidebar", &[-100., -260., 0., -300.]);
    assert_eq!(state(cx), (false, px(360.), true, px(380.)));
    workspace.update_in(cx, |w, window, cx| w.on_toggle_sidebar(&ToggleSidebar, window, cx));
    draw(cx);
    assert_eq!(cx.debug_bounds("sidebar").unwrap().size.width, px(360.));

    let at = cx.debug_bounds("split-sidebar").unwrap().center();
    cx.simulate_event(MouseDownEvent {
        button: MouseButton::Left,
        position: at,
        modifiers: Modifiers::default(),
        click_count: 2,
        first_mouse: false,
    });
    cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::default());
    draw(cx);
    assert_eq!(state(cx).1, px(260.), "双击分割线恢复默认宽度");
    let saved = cx.update(|_, cx| crate::paths::PersistedState::load(&workspace.read(cx).data_dir));
    assert_eq!((saved.sidebar_width, saved.panel_width), (Some(260.), Some(380.)), "宽度记下来了");
    close_index(cx, &workspace);
}

#[gpui_kit::test]
fn breadcrumb_folders_reveal_themselves_in_the_file_tree(cx: &mut TestAppContext) {
    let (dir, _data, workspace) = open(cx, Some("# A\n"));
    std::fs::create_dir_all(dir.path().join("项目/进行中")).unwrap();
    std::fs::write(dir.path().join("项目/进行中/周计划.md"), "# 周计划\n").unwrap();
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    with_root(cx, &workspace);
    let tree = workspace.update_in(cx, |w, window, cx| {
        let tree = w.open.as_ref().unwrap().tree.clone();
        tree.update(cx, |t, _| t.rescan());
        w.open_note(note("项目/进行中/周计划.md"), window, cx);
        tree.update(cx, |t, cx| t.collapse_all(cx));
        w.sidebar_tab = SidebarTab::Search;
        tree
    });
    draw(cx);
    for crumb in ["crumb-vault", "crumb:项目", "crumb:项目/进行中", "crumb-note"] {
        assert!(cx.debug_bounds(crumb).is_some(), "路径里有 {crumb}");
    }
    let at = cx.debug_bounds("crumb:项目").unwrap().center();
    cx.simulate_click(at, Modifiers::default());
    draw(cx);
    cx.update(|_, cx| {
        assert_eq!(workspace.read(cx).sidebar_tab, SidebarTab::Files, "点路径切回文件视图");
        assert_eq!(tree.read(cx).selected_folder().map(|p| p.as_str()), Some("项目"));
    });
    assert!(cx.debug_bounds("file-tree-folder:项目/进行中").is_some(), "展开到被点的文件夹");
    close_index(cx, &workspace);
}

/// 文件监听的一批变化：只改了已有笔记的内容（包括自己的自动保存）不动文件树；
/// 有新笔记时在后台重扫，界面线程上不做目录遍历（有安全软件审计文件访问时一次要几秒）。
#[gpui_kit::test]
fn saving_a_note_leaves_the_tree_alone_and_new_files_rescan_in_the_background(cx: &mut TestAppContext) {
    use crate::watch::Batch;
    let (dir, _data, workspace) = open(cx, Some("# A\n"));
    let cx = &mut VisualTestContext::from_window(cx.windows()[0], cx);
    let tree = cx.update(|_, cx| workspace.read(cx).open.as_ref().unwrap().tree.clone());
    workspace.update_in(cx, |w, window, cx| {
        w.on_vault_changed(&Batch { changed: vec![note("a.md")], ..Default::default() }, window, cx);
        assert!(!tree.read(cx).is_scanning(), "只改内容不重扫");
    });
    std::fs::write(dir.path().join("b.md"), "# B\n").unwrap();
    workspace.update_in(cx, |w, window, cx| {
        w.on_vault_changed(&Batch { changed: vec![note("b.md")], ..Default::default() }, window, cx);
        w.on_vault_changed(&Batch { rescan: true, ..Default::default() }, window, cx);
        assert!(tree.read(cx).is_scanning(), "新笔记触发重扫");
        assert!(!tree.read(cx).contains(&note("b.md")), "界面线程上没有同步遍历");
    });
    cx.run_until_parked();
    cx.update(|_, cx| {
        assert!(tree.read(cx).contains(&note("b.md")));
        assert!(!tree.read(cx).is_scanning(), "扫描中又来的请求合并成一次补扫，然后停下");
    });
    close_index(cx, &workspace);
}
