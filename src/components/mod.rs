//! 本项目可复用的界面组件。
//!
//! 遵循「能用现成的就不自绘」的层次：
//!
//! 1. 首选 gpui-kit 的 styled 组件（`Button`、`Input`、`List`、`Tag`、`Kbd`、
//!    `Empty`、`Spinner`、`Alert`、`StatusBar` …）；
//! 2. gpui-kit 只给行为不给样式的场景，用 gpui-base 提供的组合能力；
//! 3. 只有在上面两层都覆盖不到的、本项目专有的形态，才在这里自绘。
//!
//! 组件只回答「长什么样、点了会怎样」，不持有业务状态：数据与回调都由调用方
//! 注入，因此它们可以脱离启动器单独复用与测试。

pub mod app_list_item;
pub mod custom_app_list;
pub mod file_search;
pub mod launcher_footer;
pub mod search_bar;

use std::rc::Rc;

use gpui::{App, ClickEvent, Window};

/// 组件回调的统一签名：`(事件, 窗口, 应用状态) -> ()`。
///
/// 组件本身不持有业务状态，交互一律通过这个回调交给调用方，所以
/// `Option<ClickHandler>` 在组件里反复出现；写成别名免得每处都抄一遍长类型。
pub(crate) type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// 解析一个编译期已知合法的按键描述。
///
/// 键位串是代码里的字面量（`"tab"`、`"enter"` …），解析失败只可能是写错了键名，
/// 属于开发期错误；渲染时静默降级反而会把问题藏起来，因此这里直接 panic。
pub(crate) fn keystroke(spec: &str) -> gpui::Keystroke {
    gpui::Keystroke::parse(spec)
        .unwrap_or_else(|err| panic!("非法的按键描述 {spec:?}（应为 gpui 键位串）: {err}"))
}
