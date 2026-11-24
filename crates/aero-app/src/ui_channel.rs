//! 界面自动化通道，只在 debug 构建里编译，设了 `AERO_UI_CHANNEL=<命令文件>` 才启用。
//!
//! 每 16 ms 读一次命令文件（读完即删），逐行把鼠标 / 键盘事件经 GPUI 派发进窗口；一批处理完，在
//! `<命令文件>.ack` 里写一个递增的序号。事件不经过系统，不移动真实鼠标，窗口被别的应用挡住也收得到。
//! 窗口默认停在主屏右缘之外、只露 1 pt：macOS 仍然给它出帧，`screencapture -l` 截得到完整窗口，
//! 又不挡住正在用的应用。`AERO_UI_PARK=0` 时照常居中并激活：看得到光标和选区，但会抢走焦点。
//! 脚本与用法见 `.agents/skills/aero-ui-test/`。
//!
//! 命令（坐标是窗口内的点，左上角为原点）：
//! - `move x y`：移动指针；按着左键时就是拖动
//! - `down x y [shift|cmd|alt|ctrl ...]` / `up x y`：按下 / 松开左键
//! - `dclick x y`、`tclick x y`：双击、三击
//! - `scroll x y dy [dx]`：滚轮，像素；负数向下翻
//! - `key <按键>`：GPUI 的按键写法，如 `escape`、`cmd-z`、`shift-down`
//! - `type <文字>`：逐字输入，可含中文与空格
//! - `wait ms`：等待，期间照常出帧
//! - `theme light|dark`：仅为当前测试应用切换主题，不修改系统设置

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::*;

fn enabled() -> bool {
    std::env::var_os("AERO_UI_CHANNEL").is_some()
}

/// 窗口停在屏幕外、不激活。这时窗口不是活跃窗口：光标、选区不画，焦点回调也要等下一帧才到。
pub fn parked() -> bool {
    enabled() && std::env::var("AERO_UI_PARK").as_deref() != Ok("0")
}

/// 通道启用时把窗口停到屏幕右缘之外，并且不抢焦点。
pub fn prepare_window(options: &mut WindowOptions, cx: &App) {
    if !parked() {
        return;
    }
    let Some(display) = cx.primary_display() else { return };
    let screen = display.bounds();
    let origin = point(screen.right() - px(1.), screen.top() + px(40.));
    options.window_bounds = Some(WindowBounds::Windowed(Bounds::new(origin, size(px(1280.), px(820.)))));
    options.kind = WindowKind::PopUp;
    options.focus = false;
}

pub fn spawn(window: AnyWindowHandle, cx: &mut App) {
    let Some(path) = std::env::var_os("AERO_UI_CHANNEL").map(PathBuf::from) else { return };
    let ack = path.with_extension("ack");
    let _ = std::fs::write(&ack, "0");
    cx.spawn(async move |cx| {
        let mut pressed = false;
        let mut done = 0u64;
        loop {
            cx.background_executor().timer(Duration::from_millis(16)).await;
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let _ = std::fs::remove_file(&path);
            for line in text.lines() {
                let mut words = line.split_whitespace();
                let Some(cmd) = words.next() else { continue };
                let rest: Vec<&str> = words.collect();
                if cmd == "wait" {
                    let ms = rest.first().and_then(|v| v.parse().ok()).unwrap_or(100);
                    cx.background_executor().timer(Duration::from_millis(ms)).await;
                    continue;
                }
                let _ = window.update(cx, |_, window, cx| {
                    run(cmd, &rest, &mut pressed, window, cx);
                    window.refresh();
                });
            }
            done += 1;
            let _ = std::fs::write(&ack, done.to_string());
        }
    })
    .detach();
}

fn run(cmd: &str, rest: &[&str], pressed: &mut bool, window: &mut Window, cx: &mut App) {
    let nums: Vec<f32> = rest.iter().filter_map(|v| v.parse().ok()).collect();
    let at = || point(px(nums.first().copied().unwrap_or(0.)), px(nums.get(1).copied().unwrap_or(0.)));
    let button = MouseButton::Left;
    let mut modifiers = Modifiers::default();
    for word in rest {
        match *word {
            "shift" => modifiers.shift = true,
            "cmd" => modifiers.platform = true,
            "alt" => modifiers.alt = true,
            "ctrl" => modifiers.control = true,
            _ => {}
        }
    }
    let down = |click_count, window: &mut Window, cx: &mut App| {
        let event = MouseDownEvent { button, position: at(), modifiers, click_count, first_mouse: false };
        window.dispatch_event(PlatformInput::MouseDown(event), cx);
    };
    let up = |click_count, window: &mut Window, cx: &mut App| {
        let event = MouseUpEvent { button, position: at(), modifiers, click_count };
        window.dispatch_event(PlatformInput::MouseUp(event), cx);
    };
    match cmd {
        "theme" => {
            let mode = match rest.first().copied() {
                Some("light") => gpui_kit::component::ThemeMode::Light,
                Some("dark") => gpui_kit::component::ThemeMode::Dark,
                _ => {
                    log::warn!("ui channel: bad theme {rest:?}");
                    return;
                }
            };
            gpui_kit::component::Theme::change(mode, Some(window), cx);
        }
        "move" => {
            let event =
                MouseMoveEvent { position: at(), pressed_button: pressed.then_some(button), modifiers };
            window.dispatch_event(PlatformInput::MouseMove(event), cx);
        }
        "down" => {
            *pressed = true;
            down(1, window, cx);
        }
        "up" => {
            *pressed = false;
            up(1, window, cx);
        }
        "dclick" | "tclick" => {
            let clicks = if cmd == "dclick" { 2 } else { 3 };
            for count in 1..=clicks {
                down(count, window, cx);
                up(count, window, cx);
            }
            *pressed = false;
        }
        "scroll" => {
            let dx = nums.get(3).copied().unwrap_or(0.);
            let dy = nums.get(2).copied().unwrap_or(0.);
            let event = ScrollEvent {
                position: at(),
                delta: ScrollDelta::Pixels(point(px(dx), px(dy))),
                modifiers,
                touch_phase: TouchPhase::Moved,
                source: ScrollSource::Pointer,
            };
            window.dispatch_event(PlatformInput::Scroll(event), cx);
        }
        "key" => match rest.first().map(|key| Keystroke::parse_with_os(key, OperatingSystem::Mac)) {
            Some(Ok(keystroke)) => {
                window.dispatch_keystroke(keystroke, cx);
            }
            _ => log::warn!("ui channel: bad key {rest:?}"),
        },
        "type" => {
            for ch in rest.join(" ").chars() {
                let key = ch.to_string();
                let keystroke =
                    Keystroke { modifiers: Modifiers::default(), key: key.clone(), key_char: Some(key) };
                window.dispatch_keystroke(keystroke, cx);
            }
        }
        _ => log::warn!("ui channel: unknown command {cmd:?}"),
    }
}
