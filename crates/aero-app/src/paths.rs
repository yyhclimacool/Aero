//! 应用数据目录（方案 5.3）与应用级持久状态。
//!
//! 保存应用状态、可重建索引与可编辑示例库；demo-vault 中的用户修改不是缓存。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// `~/Library/Application Support/Aero`（macOS）、`$XDG_DATA_HOME/aero`（Linux）、`%APPDATA%\Aero`（Windows）。
/// debug 构建可以用 `AERO_DATA_DIR` 指到别处，界面自动化靠它不碰用户自己的状态与索引。
pub fn app_data_dir() -> PathBuf {
    if cfg!(debug_assertions)
        && let Some(dir) = std::env::var_os("AERO_DATA_DIR")
    {
        return PathBuf::from(dir);
    }
    if cfg!(target_os = "macos") {
        home().join("Library").join("Application Support").join("Aero")
    } else if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(home).join("Aero")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".local").join("share"))
            .join("aero")
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 链接悬停预览（`ui_design.md` 8.1 / 12）。存在应用级 `state.toml` 里。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinkPreviewMode {
    /// 悬停 500 ms 后显示
    #[default]
    Hover,
    /// 按住 ⌘ 才显示
    Command,
    /// 不显示
    Off,
}

/// 应用级持久状态，`state.toml`。
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedState {
    /// 最近打开的 vault 根目录，最新的在前
    pub recent_vaults: Vec<PathBuf>,
    pub sidebar_visible: Option<bool>,
    pub panel_visible: Option<bool>,
    /// 左侧栏 / 右侧面板拖出来的宽度（px）
    pub sidebar_width: Option<f32>,
    pub panel_width: Option<f32>,
    pub link_preview: LinkPreviewMode,
    /// 聚焦块显示源码（9.1）。默认关闭：聚焦时隐藏标记。
    pub focus_shows_source: bool,
    /// 按 vault_id 记住的界面状态（`ui_design.md` 2.1：不写入 vault）
    pub vaults: std::collections::BTreeMap<String, VaultUi>,
}

/// 一个 vault 的标签页与收藏。路径都是 vault 内的相对路径。
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VaultUi {
    /// 打开的标签页，从左到右；空串是空白的「新标签页」
    pub tabs: Vec<String>,
    pub active_tab: usize,
    /// 收藏的笔记，按收藏的先后
    pub favorites: Vec<String>,
}

impl PersistedState {
    fn file(data_dir: &Path) -> PathBuf {
        data_dir.join("state.toml")
    }

    pub fn load(data_dir: &Path) -> Self {
        fs::read_to_string(Self::file(data_dir))
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, data_dir: &Path) {
        let path = Self::file(data_dir);
        let _ = fs::create_dir_all(data_dir);
        if let Ok(body) = toml::to_string(self) {
            let _ = fs::write(&path, body);
        }
    }

    /// 把 vault 移到最近列表顶部（最多保留 10 个）。
    pub fn touch_vault(&mut self, root: &Path) {
        self.recent_vaults.retain(|p| p != root);
        self.recent_vaults.insert(0, root.to_path_buf());
        self.recent_vaults.truncate(10);
    }

    pub fn forget_vault(&mut self, root: &Path) {
        self.recent_vaults.retain(|p| p != root);
    }
}

/// 本地文件的 `file://` URL。路径按字节百分号编码，斜杠保留，避免空格和中文让系统报 -50。
pub fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                url.push(byte as char)
            }
            _ => url.push_str(&format!("%{byte:02X}")),
        }
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_url_encodes_spaces_and_keeps_slashes() {
        let url = file_url(Path::new("/Users/a/我的 文件.zip"));
        assert!(url.starts_with("file:///Users/a/"));
        assert!(url.contains("%20"));
        assert!(!url.contains(' '));
        assert!(url.ends_with(".zip"));
    }

    #[test]
    fn link_preview_mode_roundtrips_and_defaults_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = PersistedState::default();
        assert_eq!(state.link_preview, LinkPreviewMode::Hover);
        state.link_preview = LinkPreviewMode::Command;
        state.save(dir.path());
        assert_eq!(PersistedState::load(dir.path()).link_preview, LinkPreviewMode::Command);
        fs::write(dir.path().join("state.toml"), "recent_vaults = []\n").unwrap();
        assert_eq!(PersistedState::load(dir.path()).link_preview, LinkPreviewMode::Hover);
    }
}

/// `~/Knowledge` 显示为 `~/Knowledge` 而不是绝对路径。
pub fn display_path(p: &Path) -> String {
    let home = home();
    match p.strip_prefix(&home) {
        Ok(rest) if home != Path::new(".") => format!("~/{}", rest.display()),
        _ => p.display().to_string(),
    }
}
