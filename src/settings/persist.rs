/// 设置持久化：读写 settings.json

use serde::{Deserialize, Serialize};

use super::global::AppSettings;

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedSettings {
    pub theme: String,
    pub language: String,
}

impl Default for PersistedSettings {
    fn default() -> Self {
        let d = AppSettings::default();
        Self {
            theme: d.theme.to_string(),
            language: d.language.to_string(),
        }
    }
}

pub fn settings_path() -> std::path::PathBuf {
    crate::utils::app_data_dir().join("settings.json")
}

/// 从 settings.json 加载持久化设置，文件不存在时返回默认值。
pub fn load_settings() -> PersistedSettings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 将 AppSettings 中需要持久化的字段写入 settings.json。
pub fn save_settings(s: &AppSettings) {
    let p = PersistedSettings {
        theme: s.theme.to_string(),
        language: s.language.to_string(),
    };
    if let Ok(data) = serde_json::to_string_pretty(&p) {
        let _ = std::fs::write(settings_path(), data);
    }
}
