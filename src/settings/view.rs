/// 设置窗口视图

use std::time::Duration;

use gpui::*;
use gpui_kit::component::{setting::*, *};

use crate::locale::t;
use crate::update;

use super::global::AppSettings;
use super::pages::build_settings_pages;

/// 轮询升级状态的间隔。
///
/// 比得只是一个结构体，代价可以忽略；取 400ms 是因为它同时也是下载进度的刷新率，
/// 再快对肉眼没有区别，再慢进度条会显得卡顿。
const UPDATE_POLL: Duration = Duration::from_millis(400);

pub struct SettingsView {
    /// 订阅 AppSettings 全局变更，确保添加程序后列表自动刷新
    _global_sub: Subscription,
}

impl SettingsView {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let _global_sub = cx.observe_global::<AppSettings>(|_, cx| cx.notify());

        // 升级状态是在 update 模块的后台线程里推进的（检查、下载都在那边），
        // 拿不到 GPUI 的 App，所以没法直接通知界面。这里改成轮询：
        // 状态一变就重渲染，把「关于」页刷成最新。
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let mut last = update::status();
            loop {
                let now = update::status();
                if now != last {
                    last = now;
                    // 实体已释放说明设置窗口关了，及时结束轮询，
                    // 否则这个 detach 掉的任务会一直空转到进程结束
                    let alive = cx.update(|app| this.update(app, |_, cx| cx.notify()).is_ok());
                    if !alive {
                        return;
                    }
                }
                cx.background_executor().timer(UPDATE_POLL).await;
            }
        })
        .detach();

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
