//! 内嵌品牌资源；开发启动和打包应用使用同一枚图标。

use std::borrow::Cow;

use gpui_kit::assets::Assets;
use gpui_kit::{AssetSource, Result, SharedString};

pub const ICON_PATH: &str = "branding/aero-icon.png";
const ICON_PNG: &[u8] = include_bytes!("../../../assets/branding/aero-icon-v1.png");

/// 扩展 GPUI Kit 的资源源，保留组件库所需的全部默认图标。
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == ICON_PATH {
            return Ok(Some(Cow::Borrowed(ICON_PNG)));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut assets = Assets.list(path)?;
        if ICON_PATH.starts_with(path) {
            assets.push(ICON_PATH.into());
        }
        Ok(assets)
    }
}

/// 在 GPUI 的主线程启动回调中调用，让 `cargo run` 也显示应用的 Dock 图标。
#[cfg(target_os = "macos")]
pub fn set_application_icon() {
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let Some(main_thread) = MainThreadMarker::new() else {
        log::warn!("application icon must be set on the main thread");
        return;
    };
    let data = NSData::with_bytes(include_bytes!("../../../assets/branding/aero-icon-v1.icns"));
    let Some(icon) = NSImage::initWithData(NSImage::alloc(), &data) else {
        log::warn!("could not decode the embedded application icon");
        return;
    };
    let app = NSApplication::sharedApplication(main_thread);
    // SAFETY: 在主线程调用，并传入已成功解码的非空 NSImage。
    unsafe { app.setApplicationIconImage(Some(&icon)) };
}

#[cfg(not(target_os = "macos"))]
pub fn set_application_icon() {}
