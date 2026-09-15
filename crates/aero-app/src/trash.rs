//! 移到系统废纸篓（`ui_design.md` 4.1：删除一律进废纸篓，不做自定义回收站）。
//!
//! macOS 用 `NSFileManager trashItemAtURL:`，可以在 Finder 里"放回原处"。
//! 其他平台 v1 不支持，返回错误而不是永久删除。

use std::path::Path;

#[cfg(target_os = "macos")]
pub fn move_to_trash(path: &Path) -> Result<(), String> {
    use objc2_foundation::{NSFileManager, NSString, NSURL};

    let Some(s) = path.to_str() else {
        return Err(format!("{}: path is not valid UTF-8", path.display()));
    };
    let url = NSURL::fileURLWithPath(&NSString::from_str(s));
    let fm = NSFileManager::defaultManager();
    fm.trashItemAtURL_resultingItemURL_error(&url, None).map_err(|err| err.localizedDescription().to_string())
}

#[cfg(not(target_os = "macos"))]
pub fn move_to_trash(path: &Path) -> Result<(), String> {
    Err(format!("{}: moving to trash is not supported on this platform yet", path.display()))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn trashing_a_temp_file_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("gone.md");
        std::fs::write(&f, "x").unwrap();
        move_to_trash(&f).unwrap();
        assert!(!f.exists());
        // 不存在的文件：报错而不是 panic
        assert!(move_to_trash(&dir.path().join("missing.md")).is_err());
    }
}
