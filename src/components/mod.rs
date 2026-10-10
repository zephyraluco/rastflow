//! 本项目可复用的界面组件。
//!
//! 取舍顺序：gpui-kit 的 styled 组件 → gpui-base 的组合能力 → 自绘。判定标准是
//! 「能否**直接使用**」而非「库里有没有」——需要覆盖组件多个内建样式才能对上设计的，
//! 就不算直接使用。具体反例见 `docs/architecture.md` §8.1。
//!
//! 组件只回答「长什么样、点了会怎样」，不持有业务状态：数据与回调都由调用方注入。

pub mod app_list_item;
pub mod custom_app_list;
pub mod file_search;
pub mod launcher_footer;
pub mod search_bar;

use std::rc::Rc;

use gpui::{App, ClickEvent, Window};

/// 组件回调的统一签名：`(事件, 窗口, 应用状态) -> ()`。
///
/// 交互一律通过回调交给调用方，所以 `Option<ClickHandler>` 在组件里反复出现。
pub(crate) type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// 解析一个编译期已知合法的按键描述。
///
/// 键位串都是代码里的字面量，解析失败只可能是写错了键名；静默降级会把问题藏起来，
/// 所以这里直接 panic。
pub(crate) fn keystroke(spec: &str) -> gpui::Keystroke {
    gpui::Keystroke::parse(spec)
        .unwrap_or_else(|err| panic!("非法的按键描述 {spec:?}（应为 gpui 键位串）: {err}"))
}
