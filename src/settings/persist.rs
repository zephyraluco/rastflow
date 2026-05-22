/// 设置持久化：读写 settings.json

use serde::{Deserialize, Serialize};

use super::global::AppSettings;

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedSettings {
    pub theme: String,
    pub language: String,
    pub ai_api_key: String,
    pub ai_base_url: String,
    pub ai_model: String,
}

impl Default for PersistedSettings {
    fn default() -> Self {
        let d = AppSettings::default();
        Self {
            theme: d.theme.to_string(),
            language: d.language.to_string(),
            ai_api_key: d.ai_api_key.to_string(),
            ai_base_url: d.ai_base_url.to_string(),
            ai_model: d.ai_model.to_string(),
        }
    }
}

pub fn settings_path() -> std::path::PathBuf {
    std::env::current_dir()
        .unwrap_or_default()
        .join("settings.json")
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
        ai_api_key: s.ai_api_key.to_string(),
        ai_base_url: s.ai_base_url.to_string(),
        ai_model: s.ai_model.to_string(),
    };
    if let Ok(data) = serde_json::to_string_pretty(&p) {
        let _ = std::fs::write(settings_path(), data);
    }
}
