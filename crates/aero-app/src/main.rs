//! Aero 桌面应用（GPUI）。本 crate 只是 `aero-core` 的一个客户端：
//! 所有文本、块、vault 逻辑都在 core 里，这里只负责窗口、布局、交互与保存时机。
//!
//! 模块对应 `docs/ui_design.md` 的章节：
//! - [`workspace`]：主窗口三栏 + 状态栏（第 2 节）、首次运行页（第 11 节）
//! - [`file_tree`]：左侧文件树（4.1）
//! - [`editor`]：块级 Live Preview 编辑器（第 5、6 节）
//! - [`actions`]：快捷键总表（第 14 节）中 M1a 的部分

mod actions;
mod backlinks;
mod branding;
mod catalog;
mod cli;
mod colors;
mod demo_vault;
mod diagnostics;
mod editor;
mod file_tree;
mod matching;
mod paths;
mod quick_open;
mod render;
mod search;
mod search_settings;
mod search_view;
mod semantic;
mod semantic_policy;
mod settings;
mod tag_tree;
mod tags_view;
mod trash;
#[cfg(debug_assertions)]
mod ui_channel;
mod watch;
mod workspace;

use std::path::PathBuf;

use gpui_kit::component::{Root, TitleBar};
use gpui_kit::*;

use crate::workspace::Workspace;

/// debug 构建里由脚本驱动界面、窗口停在屏幕外（`ui_channel`，`.agents/skills/aero-ui-test/`）。
fn ui_parked() -> bool {
    #[cfg(debug_assertions)]
    return ui_channel::parked();
    #[cfg(not(debug_assertions))]
    false
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = cli::wants_cli(&args);
    let diagnostics = diagnostics::init(if cli { "cli" } else { "gui" });
    // 子命令走 CLI（方案 13.1）。无参数，或第一个参数是库目录，才启动图形界面。
    if cli {
        let code = cli::run(&args);
        drop(diagnostics);
        std::process::exit(code);
    }
    let initial_vault: Option<PathBuf> = args.first().map(PathBuf::from);

    gpui_kit::application().with_assets(branding::AppAssets).run(move |cx| {
        gpui_kit::init(cx);
        colors::apply_text_selection(cx);
        actions::init(cx);
        branding::set_application_icon();
        diagnostics::start_heartbeat(cx);

        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        // 界面自动化时窗口停在屏幕外，不去抢正在用的应用的焦点
        if !ui_parked() {
            cx.activate(true);
        }

        #[cfg_attr(not(debug_assertions), allow(unused_mut))]
        let mut options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(None, size(px(1280.), px(820.)), cx)),
            window_min_size: Some(size(px(760.), px(480.))),
            ..TitleBar::window_options()
        };
        #[cfg(debug_assertions)]
        ui_channel::prepare_window(&mut options, cx);

        cx.spawn(async move |cx| {
            let window = cx
                .open_window(options, |window, cx| {
                    let workspace =
                        cx.new(|cx| Workspace::new(initial_vault, paths::app_data_dir(), window, cx));
                    cx.new(|cx| Root::new(workspace, window, cx))
                })
                .expect("failed to open window");
            #[cfg(debug_assertions)]
            cx.update(|cx| ui_channel::spawn(window.into(), cx));
            #[cfg(not(debug_assertions))]
            let _ = window;
        })
        .detach();
    });
}
