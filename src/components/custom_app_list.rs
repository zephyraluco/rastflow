//! 设置页「自定义程序」列表。

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_kit::component::{
    button::Button,
    description_list::{DescriptionItem, DescriptionList},
    *,
};

use crate::icons::IconName;

use super::ClickHandler;

/// 自定义程序列表 + 「添加程序」入口。
///
/// 「名称 → 路径」是典型的键值对，直接复用 gpui-kit 的 [`DescriptionList`]；
/// 空态与添加按钮由本组件补齐。数据与回调都由调用方注入，组件不关心
/// 条目来自数据库还是内存；文案默认值为中文，调用方应当覆盖。
#[derive(IntoElement)]
pub struct CustomAppList {
    entries: Vec<(SharedString, SharedString)>,
    empty_label: SharedString,
    add_label: SharedString,
    on_add: Option<ClickHandler>,
}

impl CustomAppList {
    /// `entries` 是 `(名称, 启动目标)` 对。
    pub fn new(entries: Vec<(String, String)>) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|(name, path)| (name.into(), path.into()))
                .collect(),
            empty_label: "暂未添加自定义程序".into(),
            add_label: "添加程序".into(),
            on_add: None,
        }
    }

    /// 列表为空时的占位文案。
    pub fn empty_label(mut self, label: impl Into<SharedString>) -> Self {
        self.empty_label = label.into();
        self
    }

    /// 「添加程序」按钮的文案。
    pub fn add_label(mut self, label: impl Into<SharedString>) -> Self {
        self.add_label = label.into();
        self
    }

    /// 「添加程序」按钮的点击回调。
    pub fn on_add(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_add = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for CustomAppList {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            entries,
            empty_label,
            add_label,
            on_add,
        } = self;

        let add = Button::new("add-program-btn")
            .icon(IconName::Plus)
            .label(add_label);
        let add = match on_add {
            Some(handler) => add.on_click(move |ev, window, cx| handler(ev, window, cx)),
            None => add,
        };

        let muted = cx.theme().muted_foreground;

        v_flex()
            .w_full()
            .gap_3()
            .when(entries.is_empty(), |this| {
                this.child(
                    div()
                        .w_full()
                        .py_8()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_sm()
                        .text_color(muted)
                        .child(empty_label),
                )
            })
            .when(!entries.is_empty(), |this| {
                this.child(
                    DescriptionList::new()
                        .columns(1)
                        .label_width(px(160.))
                        .children(
                            entries
                                .into_iter()
                                .map(|(name, path)| DescriptionItem::new(name).value(path)),
                        ),
                )
            })
            .child(div().w_full().flex().justify_center().child(add))
    }
}
