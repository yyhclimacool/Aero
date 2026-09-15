//! Aero 桌面应用（GPUI）。本 crate 只是 `aero-core` 的一个客户端：
//! 所有文本、块、vault 逻辑都在 core 里，这里只负责窗口、布局、交互与保存时机。
//!
//! 模块对应 `docs/ui_design.md` 的章节：
//! - [`workspace`]：主窗口三栏 + 状态栏（第 2 节）、首次运行页（第 11 节）
//! - [`file_tree`]：左侧文件树（4.1）
//! - [`editor`]：块级 Live Preview 编辑器（第 5、6 节）
//! - [`actions`]：快捷键总表（第 14 节）中 M1a 的部分

mod actions;
mod branding;
mod editor;
mod file_tree;
mod paths;
mod workspace;

use std::path::PathBuf;

use gpui_kit::component::{Root, TitleBar};
use gpui_kit::*;

use crate::workspace::Workspace;

/// 输出到 stderr 的极简 logger。`AERO_LOG=debug|info|warn|error`，默认 warn；只看自己 crate 的日志。
struct StderrLogger(log::LevelFilter);

impl log::Log for StderrLogger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= self.0 && m.target().starts_with("aero")
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!("[{} {}] {}", record.level(), record.target(), record.args());
        }
    }

    fn flush(&self) {}
}

fn init_logger() {
    let level = match std::env::var("AERO_LOG").as_deref() {
        Ok("debug") => log::LevelFilter::Debug,
        Ok("info") => log::LevelFilter::Info,
        Ok("error") => log::LevelFilter::Error,
        _ => log::LevelFilter::Warn,
    };
    let _ = log::set_boxed_logger(Box::new(StderrLogger(level)));
    log::set_max_level(level);
}

fn main() {
    init_logger();

    // `aero <vault 目录>` 直接打开；否则进入首次运行页 / 最近的 vault
    let initial_vault: Option<PathBuf> = std::env::args().nth(1).map(PathBuf::from);

    gpui_kit::application().with_assets(branding::AppAssets).run(move |cx| {
        gpui_kit::init(cx);
        actions::init(cx);
        branding::set_application_icon();

        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1280.), px(820.)), cx)),
            window_min_size: Some(size(px(760.), px(480.))),
            ..TitleBar::window_options()
        };

        cx.spawn(async move |cx| {
            cx.open_window(options, |window, cx| {
                let workspace = cx.new(|cx| Workspace::new(initial_vault, window, cx));
                cx.new(|cx| Root::new(workspace, window, cx))
            })
            .expect("failed to open window");
        })
        .detach();
    });
}
