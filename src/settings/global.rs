/// 全局运行时设置，存储在 GPUI 的 Global 中

use gpui::*;

#[derive(Clone)]
pub struct AppSettings {
    pub theme: SharedString,
    pub language: SharedString,
    pub auto_launch: bool,
    pub show_descriptions: bool,
    pub search_in_desc: bool,
    pub max_results: SharedString,
    /// 自定义程序列表版本号，变更时触发设置页重渲染。
    pub custom_programs_version: u64,
    /// 窗口上次所在屏幕，用于多屏居中时决定目标屏幕。
    pub last_display: Option<DisplayId>,
    /// 唤出界面的全局快捷键，格式如 "alt+space"。
    pub hotkey: SharedString,
    /// AI 模型 API Key
    pub ai_api_key: SharedString,
    /// AI API Base URL（留空使用 Anthropic 官方地址）
    pub ai_base_url: SharedString,
    /// AI 模型名称，如 "claude-opus-4-5"
    pub ai_model: SharedString,
}

impl Global for AppSettings {}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            language: "zh".into(),
            auto_launch: false,
            show_descriptions: true,
            search_in_desc: true,
            max_results: "15".into(),
            custom_programs_version: 0,
            last_display: None,
            hotkey: "alt+space".into(),
            ai_api_key: "".into(),
            ai_base_url: "".into(),
            ai_model: "claude-opus-4-5".into(),
        }
    }
}
