//! 应用数据目录（方案 5.3）与应用级持久状态。
//!
//! 这里只放"丢了也不心疼"的东西：最近打开的 vault、面板折叠状态。索引在 M1b 起也落在这个目录下。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// `~/Library/Application Support/Aero`（macOS）、`$XDG_DATA_HOME/aero`（Linux）、`%APPDATA%\Aero`（Windows）。
pub fn app_data_dir() -> PathBuf {
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

/// 应用级持久状态，`state.toml`。
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedState {
    /// 最近打开的 vault 根目录，最新的在前
    pub recent_vaults: Vec<PathBuf>,
    pub sidebar_visible: Option<bool>,
    pub panel_visible: Option<bool>,
}

impl PersistedState {
    fn file() -> PathBuf {
        app_data_dir().join("state.toml")
    }

    pub fn load() -> Self {
        fs::read_to_string(Self::file()).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        let path = Self::file();
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
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

/// `~/Knowledge` 显示为 `~/Knowledge` 而不是绝对路径。
pub fn display_path(p: &Path) -> String {
    let home = home();
    match p.strip_prefix(&home) {
        Ok(rest) if home != Path::new(".") => format!("~/{}", rest.display()),
        _ => p.display().to_string(),
    }
}
