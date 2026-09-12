use std::{
    path::PathBuf,
    sync::{OnceLock, RwLock},
};

use serde::{Deserialize, Serialize};
use tauri::Manager;

const PREFERENCES_FILE: &str = "integration-preferences.json";

/// 集成偏好缓存：每个 hook 事件 / 通知都会读一次偏好，
/// 之前每次都做 read_to_string + serde_json::from_str，这里只读一次磁盘。
static PREFERENCES_CACHE: OnceLock<RwLock<Option<IntegrationPreferences>>> = OnceLock::new();

fn preferences_cache() -> &'static RwLock<Option<IntegrationPreferences>> {
    PREFERENCES_CACHE.get_or_init(|| RwLock::new(None))
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct IntegrationPreferences {
    pub notifications_and_hooks_enabled: bool,
}

impl Default for IntegrationPreferences {
    fn default() -> Self {
        Self {
            notifications_and_hooks_enabled: true,
        }
    }
}

fn preferences_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join(PREFERENCES_FILE))
        .map_err(|e| format!("无法解析集成配置目录: {e}"))
}

fn read_preferences_from_disk(app: &tauri::AppHandle) -> IntegrationPreferences {
    let Ok(path) = preferences_path(app) else {
        return IntegrationPreferences::default();
    };

    let Ok(content) = std::fs::read_to_string(path) else {
        return IntegrationPreferences::default();
    };

    serde_json::from_str(&content).unwrap_or_default()
}

pub fn load_preferences(app: &tauri::AppHandle) -> IntegrationPreferences {
    if let Ok(cached) = preferences_cache().read() {
        if let Some(preferences) = *cached {
            return preferences;
        }
    }

    let preferences = read_preferences_from_disk(app);
    if let Ok(mut cached) = preferences_cache().write() {
        *cached = Some(preferences);
    }
    preferences
}

pub fn save_preferences(app: &tauri::AppHandle, enabled: bool) -> Result<(), String> {
    let path = preferences_path(app)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建集成配置目录 {} 失败: {e}", parent.display()))?;
    }

    let preferences = IntegrationPreferences {
        notifications_and_hooks_enabled: enabled,
    };
    let content =
        serde_json::to_string_pretty(&preferences).map_err(|e| format!("序列化集成配置失败: {e}"))?;

    std::fs::write(&path, content).map_err(|e| format!("写入 {} 失败: {e}", path.display()))?;

    if let Ok(mut cached) = preferences_cache().write() {
        *cached = Some(preferences);
    }
    Ok(())
}

pub fn notifications_and_hooks_enabled(app: &tauri::AppHandle) -> bool {
    load_preferences(app).notifications_and_hooks_enabled
}
