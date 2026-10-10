//! 启动器列表项：图标（或首字母色块）+ 名称 + 类别标签。
//!
//! 这里只负责「一行的内容长什么样」，选中态与键盘导航交给
//! gpui-kit 的 [`ListItem`](gpui_kit::component::list::ListItem) 统一处理。

use std::sync::Arc;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_kit::component::*;

/// 列表项图标的边长。列表行高按 7 个 4px 单位（28px）设计。
const ICON_EDGE: f32 = 28.;

/// 条目图标：取到图标就显示图片，否则回退到首字母色块。
///
/// 图标由后台线程调用 Shell / GDI 提取后回填，渲染时可能还没到，
/// 所以 `icon` 是 `Option`，渲染路径上只做内存查表。
pub fn entry_icon(icon: Option<Arc<RenderImage>>, name: &str, cx: &App) -> AnyElement {
    match icon {
        Some(icon) => img(icon)
            .w(px(ICON_EDGE))
            .h(px(ICON_EDGE))
            .flex_shrink_0()
            .into_any_element(),
        None => letter_tile(name, cx),
    }
}

/// 取不到图标时的回退：一个带首字母的色块，保证每行左侧都有视觉锚点。
fn letter_tile(name: &str, cx: &App) -> AnyElement {
    let letter = name
        .chars()
        .next()
        .map(|c| c.to_ascii_uppercase())
        .unwrap_or('?');

    div()
        .w(px(ICON_EDGE))
        .h(px(ICON_EDGE))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .bg(cx.theme().accent)
        .text_sm()
        .font_bold()
        .text_color(cx.theme().accent_foreground)
        .child(letter.to_string())
        .into_any_element()
}

/// 应用条目的行内容：图标 + 名称 + 类别标签。
///
/// 名称占据剩余宽度并截断；类别为空时不渲染标签，避免出现空胶囊。
///
/// 类别标签是自绘而非 `Tag`：`Tag` 要么带边框（`border_1`）、要么在
/// `xsmall` 下用 `rems(0.625)` 算出小数像素字号 —— 中文在这个尺寸会发虚。
/// 设计要的是 12px、无边框、`muted` 底的一枚淡胶囊，`Tag` 无法直接给出，
/// 所以这里用 `div` 自绘。
pub fn app_row(
    icon: Option<Arc<RenderImage>>,
    name: &SharedString,
    category: &SharedString,
    cx: &App,
) -> AnyElement {
    h_flex()
        .w_full()
        .items_center()
        .gap_3()
        .px_3()
        .py_1()
        .child(entry_icon(icon, name, cx))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_sm()
                .child(name.clone()),
        )
        .when(!category.is_empty(), |this| {
            this.child(
                div()
                    .flex_shrink_0()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(cx.theme().muted)
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(category.clone()),
            )
        })
        .into_any_element()
}
