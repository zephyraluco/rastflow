//! 文件搜索面板：索引状态、结果表头与结果行。
//!
//! 索引没就绪时 [`index_status_panel`] 占满内容区，就绪后返回 `None`，
//! 由调用方渲染结果列表 —— 「状态」与「结果」两块因此互不牵连。
//! 结果表头与结果行共用同一组列宽常量，改一处即可对齐两处。

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_kit::component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    spinner::Spinner,
    *,
};

use crate::icons::IconName;
use crate::layout::filesearch::{FileResult, SearchStatus};
use crate::locale::{scanning_detail, t};

/// 结果表各列宽度（像素）。名称与大小、修改时间取固定宽度，
/// 路径吃掉剩余空间，这样长路径被截断而不是把其它列挤走。
const NAME_COL: f32 = 180.;
const SIZE_COL: f32 = 86.;
const MODIFIED_COL: f32 = 128.;

/// 索引未就绪时的状态面板。
///
/// - `Uninitialized` / `Indexing`：等待或建索引进度；
/// - `Unavailable`：失败原因；若属于需要管理员权限，额外给出「安装索引服务」按钮；
/// - `Ready`：返回 `None`，由调用方接管结果列表。
pub fn index_status_panel(
    status: &SearchStatus,
    on_elevate: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_retry: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Option<AnyElement> {
    match status {
        SearchStatus::Ready { .. } => None,

        SearchStatus::Uninitialized => Some(
            centered()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(t("filesearch.init", cx)),
                )
                .into_any_element(),
        ),

        SearchStatus::Indexing { disk, written } => {
            let detail = match disk {
                Some(disk) => vec![
                    scanning_detail(*disk, *written, cx),
                    t("filesearch.indexing.hint", cx).to_string(),
                ],
                None => vec![
                    t("filesearch.indexing.preparing", cx).to_string(),
                    t("filesearch.indexing.hint", cx).to_string(),
                ],
            };
            Some(busy_panel(
                t("filesearch.indexing", cx).to_string(),
                detail,
                cx,
            ))
        }

        SearchStatus::Unavailable { reason, .. } => Some(unavailable_panel(
            reason,
            status.needs_admin(),
            on_elevate,
            on_retry,
            cx,
        )),
    }
}

/// 结果表的表头（名称 / 路径 / 大小 / 修改时间）。
pub fn result_header(cx: &App) -> AnyElement {
    let muted = cx.theme().muted_foreground;

    h_flex()
        .w_full()
        .gap_3()
        .px_3()
        .py_1()
        .border_b_1()
        .border_color(cx.theme().border)
        .text_xs()
        .font_semibold()
        .text_color(muted)
        .child(div().w(px(NAME_COL)).child(t("filesearch.column.name", cx)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(t("filesearch.column.path", cx)),
        )
        .child(div().w(px(SIZE_COL)).child(t("filesearch.column.size", cx)))
        .child(
            div()
                .w(px(MODIFIED_COL))
                .child(t("filesearch.column.modified", cx)),
        )
        .into_any_element()
}

/// 结果表的一行。
pub fn result_row(
    result: &FileResult,
    icon: Option<std::sync::Arc<RenderImage>>,
    cx: &App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;

    h_flex()
        .w_full()
        .items_center()
        .gap_3()
        .px_3()
        .py_1()
        .child(
            h_flex()
                .w(px(NAME_COL))
                .flex_shrink_0()
                .gap_1()
                .child(match icon {
                    Some(icon) => img(icon)
                        .w(px(16.))
                        .h(px(16.))
                        .flex_shrink_0()
                        .into_any_element(),
                    None => Icon::new(if result.is_dir {
                        IconName::Folder
                    } else {
                        IconName::File
                    })
                    .size(px(16.))
                    .into_any_element(),
                })
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .child(result.name.clone()),
                ),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_sm()
                .text_color(muted)
                .child(result.dir.clone()),
        )
        .child(
            div()
                .w(px(SIZE_COL))
                .truncate()
                .text_sm()
                .text_color(muted)
                .child(result.size.clone()),
        )
        .child(
            div()
                .w(px(MODIFIED_COL))
                .truncate()
                .text_sm()
                .text_color(muted)
                .child(result.modified.clone()),
        )
        .into_any_element()
}

/// 查无结果时的空态：朴素的一行弱化文字（与重构前一致）。
///
/// 不用 `Empty`：它自带虚线边框与大内边距，在小窗口的结果区里会变成一个
/// 突兀的方框。
pub fn no_results(cx: &App) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .p_8()
        .text_color(cx.theme().muted_foreground)
        .child(t("filesearch.no_results", cx))
        .into_any_element()
}

/// 内容区中央的竖排布局：状态面板四种形态共用。
///
/// 不用 `Empty`：那是一个带虚线边框的占位盒子，而这里要的是「裸」居中内容，
/// 边框会把小窗口撑得很重。
fn centered() -> Div {
    v_flex()
        .flex_1()
        .items_center()
        .justify_center()
        .gap_1()
        .p_6()
}

/// 「等待中 / 进行中」：转圈 + 标题 + 若干行明细。
fn busy_panel(title: String, detail: Vec<String>, cx: &App) -> AnyElement {
    let muted = cx.theme().muted_foreground;

    centered()
        .gap_3()
        .child(Spinner::new().large())
        .child(
            v_flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .text_color(cx.theme().foreground)
                        .child(title),
                )
                .children(detail.into_iter().map(|line| {
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(SharedString::from(line))
                })),
        )
        .into_any_element()
}

/// 失败状态：警示图标 + 标题 + 原因 + 安装服务 / 重试操作。
fn unavailable_panel(
    reason: &str,
    needs_admin: bool,
    on_elevate: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_retry: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;

    let mut actions = h_flex().gap_2();
    if needs_admin {
        actions = actions.child(
            Button::new("elevate-btn")
                .child(SharedString::from(t("filesearch.elevate", cx)))
                .on_click(move |ev, window, cx| on_elevate(ev, window, cx)),
        );
    }
    actions = actions.child(
        Button::new("retry-btn")
            .ghost()
            .child(SharedString::from(t("filesearch.retry", cx)))
            .on_click(move |ev, window, cx| on_retry(ev, window, cx)),
    );

    centered()
        .gap_4()
        .child(
            Icon::new(IconName::TriangleAlert)
                .large()
                .text_color(cx.theme().warning),
        )
        .child(
            v_flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .text_color(cx.theme().foreground)
                        .child(t("filesearch.unavailable", cx)),
                )
                .child(div().text_xs().text_color(muted).child(reason.to_owned()))
                .when(needs_admin, |this| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(t("filesearch.needs_admin", cx)),
                    )
                }),
        )
        .child(actions)
        .into_any_element()
}
