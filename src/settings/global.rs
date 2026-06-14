/// 全局运行时设置，存储在 GPUI 的 Global 中

use gpui::*;

#[derive(Clone)]
pub struct AppSettings {
    pub theme: SharedString,
    pub language: SharedString,
    pub auto_launch: bool,
    /// 自定义程序列表版本号，变更时触发设置页重渲染。
    pub custom_programs_version: u64,
    /// 窗口上次所在屏幕，用于多屏居中时决定目标屏幕。
    pub last_display: Option<DisplayId>,
    /// 唤出界面的全局快捷键，格式如 "alt+space"。
    pub hotkey: SharedString,
}

impl Global for AppSettings {}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            language: "zh".into(),
            auto_launch: false,
            custom_programs_version: 0,
            last_display: None,
            hotkey: "alt+space".into(),
        }
    }
}
