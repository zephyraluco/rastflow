//! 启动器顶部的搜索栏。

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_kit::component::{
    input::{Input, InputState},
    kbd::Kbd,
    *,
};

use crate::icons::IconName;

use super::keystroke;

/// 启动器顶部的搜索栏。
///
/// 左侧图标提示当前模式（应用搜索 / 文件搜索），中间是输入框，右侧按模式给出
/// 「Tab 切到文件搜索」的提示或「本地索引」标记。窗口拖动由外层容器负责，
/// 因为那是窗口行为而不是搜索栏的职责。
///
/// 右侧文案的默认值是中文，调用方应当用 [`SearchBar::labels`] 注入当前语言。
#[derive(IntoElement)]
pub struct SearchBar {
    input: Entity<InputState>,
    file_mode: bool,
    file_search_label: SharedString,
    index_label: SharedString,
}

impl SearchBar {
    /// `input` 被克隆成 [`Entity`] 句柄（引用计数 +1，不是深拷贝），
    /// 与 gpui-kit 的 `Input::new(&state)` 用法一致。
    pub fn new(input: &Entity<InputState>, file_mode: bool) -> Self {
        Self {
            input: input.clone(),
            file_mode,
            file_search_label: "文件搜索".into(),
            index_label: "本地索引".into(),
        }
    }

    /// 覆盖右侧提示文案，供调用方按当前语言注入。
    pub fn labels(
        mut self,
        file_search: impl Into<SharedString>,
        index: impl Into<SharedString>,
    ) -> Self {
        self.file_search_label = file_search.into();
        self.index_label = index.into();
        self
    }
}

impl RenderOnce for SearchBar {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let muted_fg = cx.theme().muted_foreground;

        let mode_icon = if self.file_mode {
            IconName::FolderOpen
        } else {
            IconName::Search
        };

        // 图标与输入框同级、靠 `gap_2` 分开，而不是塞进 `Input` 的 prefix 槽位：
        // prefix 的内边距很小，图标会贴住占位文字，行首显得挤。
        // 图标不指定尺寸，跟着继承的字号走（与原先的 emoji 表现一致）。
        h_flex()
            .gap_2()
            .items_center()
            .child(div().text_color(muted_fg).child(mode_icon))
            .child(Input::new(&self.input).appearance(false).flex_1())
            .when(self.file_mode, |this| {
                this.child(
                    // 「本地索引」用 accent 底的实心胶囊；`Tag` 没有 accent 变体，
                    // 且会带一圈边框，所以自绘
                    div()
                        .flex_shrink_0()
                        .px_2()
                        .py_px()
                        .rounded_sm()
                        .bg(cx.theme().accent)
                        .text_xs()
                        .text_color(cx.theme().accent_foreground)
                        .child(self.index_label.clone()),
                )
            })
            .when(!self.file_mode, |this| {
                this.child(
                    h_flex()
                        .flex_shrink_0()
                        .gap_1()
                        .items_center()
                        .text_xs()
                        .text_color(muted_fg)
                        .child(self.file_search_label.clone())
                        .child(
                            // 键帽默认只给底色；补一圈描边，让它读起来是个「可按键」
                            // 而不是一块色斑
                            Kbd::new(keystroke("tab"))
                                .border_1()
                                .border_color(cx.theme().border),
                        ),
                )
            })
    }
}
