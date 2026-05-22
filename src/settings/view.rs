/// 设置窗口视图

use gpui::*;
use gpui_component::{setting::*, *};

use crate::locale::t;

use super::global::AppSettings;
use super::pages::build_settings_pages;

pub struct SettingsView {
    /// 订阅 AppSettings 全局变更，确保添加程序后列表自动刷新
    _global_sub: Subscription,
}

impl SettingsView {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let _global_sub = cx.observe_global::<AppSettings>(|_, cx| cx.notify());
        Self { _global_sub }
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = cx.global::<AppSettings>().language.to_string();
        let title = t("settings.title", cx);
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .child(
                TitleBar::new().child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .text_color(cx.theme().foreground)
                        .child(title),
                ),
            )
            .child(Settings::new("settings").pages(build_settings_pages(&lang)))
    }
}
