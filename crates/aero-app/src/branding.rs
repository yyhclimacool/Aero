//! 内嵌品牌资源；开发启动和打包应用使用同一枚图标。

use std::borrow::Cow;

use gpui_kit::assets::Assets;
use gpui_kit::{AssetSource, Result, SharedString};

pub const ICON_PATH: &str = "branding/aero-icon.png";
const ICON_PNG: &[u8] = include_bytes!("../../../assets/branding/aero-icon-v1.png");

// 默认图标包之外用到的图标。没列进来的图标会画成空白（`tests::every_icon_in_the_source_is_bundled` 把关）。
gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [
        Lightbulb,
        CircleAlert,
        CircleQuestionMark,
        List,
        Quote,
        FilePlus,
        FolderPlus,
        Tag,
        Link,
        TableOfContents,
        ChevronsDownUp,
        LocateFixed,
    ]
);

/// 扩展 GPUI Kit 的资源源，保留组件库所需的全部默认图标。
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == ICON_PATH {
            return Ok(Some(Cow::Borrowed(ICON_PNG)));
        }
        if let Some(icon) = ExtraIcons.load(path)? {
            return Ok(Some(icon));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut assets = Assets.list(path)?;
        assets.extend(ExtraIcons.list(path)?);
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

#[cfg(test)]
mod tests {
    use gpui_kit::AssetSource as _;
    use gpui_kit::assets::IconName;

    #[test]
    fn every_icon_in_the_source_is_bundled() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = vec![src];
        let mut missing = Vec::new();
        while let Some(path) = files.pop() {
            if path.is_dir() {
                files.extend(std::fs::read_dir(&path).unwrap().map(|entry| entry.unwrap().path()));
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for (at, _) in text.match_indices("IconName::") {
                let name: String = text[at + 10..].chars().take_while(char::is_ascii_alphanumeric).collect();
                let Some(icon) = IconName::ALL.iter().find(|icon| format!("{icon:?}") == name) else {
                    continue;
                };
                if super::AppAssets.load(&icon.path()).unwrap().is_none() {
                    missing.push(format!("{name} ({})", path.display()));
                }
            }
        }
        missing.sort();
        missing.dedup();
        assert!(missing.is_empty(), "图标没打进资源包，会画成空白：{missing:?}");
    }
}
