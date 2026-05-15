use gpui::{AnyElement, App, IntoElement, RenderOnce, Window, SharedString};
use gpui_component::{Icon, IconNamed};
use gpui_component::icon_named;

// 调用宏扫描你自己的 Crate 目录下的自定义图标
icon_named!(IconName, "./assets/icons");

impl From<IconName> for AnyElement {
    fn from(value: IconName) -> Self {
        Icon::new(value).into_any_element()
    }
}

impl RenderOnce for IconName {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        Icon::new(self)
    }
}