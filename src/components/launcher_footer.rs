//! 启动器底部的快捷键提示与设置入口。

use std::rc::Rc;

use gpui::*;
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    *,
};

use crate::icons::IconName;

use super::ClickHandler;

/// 一条快捷键提示：键位符号 + 说明文字。
struct Hint {
    keys: Vec<SharedString>,
    label: SharedString,
}

/// 启动器底部的提示栏。
///
/// 三条硬约束，都踩过坑：
///
/// - 容器用 `h_flex` 而非 `StatusBar`：后者自带 `title_bar` 底色填充与 `px_2`，
///   而设计要的是无底色 + `px_4`，覆盖它反而更难维护；
/// - 提示用纯文本而非 `Kbd` 键帽：栏是 `text_xs` 字号，键帽的内边距与最小宽度
///   会把栏撑高、视觉上喧宾夺主（键帽留给搜索栏那种更宽松的行）；
/// - 设置按钮**不指定尺寸**：栏高由它决定，改尺寸会把整条栏一并撑高或压矮。
///
/// 提示文案由调用方注入，默认值只是中文兜底。
#[derive(IntoElement)]
pub struct LauncherFooter {
    hints: Vec<Hint>,
    settings_tooltip: SharedString,
    on_settings: Option<ClickHandler>,
}

impl LauncherFooter {
    pub fn new() -> Self {
        Self {
            hints: Vec::new(),
            settings_tooltip: "设置".into(),
            on_settings: None,
        }
    }

    /// 追加一条快捷键提示。`keys` 是键位**显示文本**（如 `"Ctrl"`、`"K"`），
    /// `label` 是随语言变化的说明文字。
    pub fn hint(
        mut self,
        keys: impl IntoIterator<Item = impl Into<SharedString>>,
        label: impl Into<SharedString>,
    ) -> Self {
        self.hints.push(Hint {
            keys: keys.into_iter().map(Into::into).collect(),
            label: label.into(),
        });
        self
    }

    /// 三条常用提示：选择 / 启动 / 关闭。
    ///
    /// 键位符号（`↑↓`、`↵`、`Esc`）是符号而不是文字，不随语言变化，所以留在
    /// 组件里；只有说明文字走本地化。本应用只面向 Windows，用 Windows 的写法即可。
    pub fn common_hints(
        self,
        select: impl Into<SharedString>,
        launch: impl Into<SharedString>,
        close: impl Into<SharedString>,
    ) -> Self {
        self.hint(["↑↓"], select)
            .hint(["↵"], launch)
            .hint(["Esc"], close)
    }

    /// 设置按钮的提示文案，同时充当它的无障碍名称。
    pub fn settings_tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.settings_tooltip = tooltip.into();
        self
    }

    /// 设置按钮的点击回调。不设置时按钮照常显示，只是点击没有反应。
    pub fn on_settings(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_settings = Some(Rc::new(handler));
        self
    }
}

impl Default for LauncherFooter {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderOnce for LauncherFooter {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let button = Button::new("settings-btn")
            .ghost()
            .icon(IconName::Settings)
            .tooltip(self.settings_tooltip);

        let button = match self.on_settings {
            Some(handler) => button.on_click(move |ev, window, cx| handler(ev, window, cx)),
            None => button,
        };

        h_flex()
            .flex_shrink_0()
            .px_4()
            .py_1()
            .gap_4()
            .border_t_1()
            .border_color(cx.theme().border)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .items_center()
            .children(self.hints.into_iter().map(render_hint))
            .child(div().flex_1())
            .child(button)
    }
}

/// 把一条提示画成 `键位 说明` 的单段文字。
///
/// 拼成一整段而不是拆成两个元素：这样字符间距和纯手写的 `"↑↓ 选择"` 完全一致，
/// 不会因为多一层 flex 容器而错位。
fn render_hint(hint: Hint) -> AnyElement {
    let keys = hint.keys.join(" ");
    div()
        .child(SharedString::from(format!("{keys} {}", hint.label)))
        .into_any_element()
}
