use gpui::*;
use gpui_component::{setting::*, *};

// ---------- 全局设置 ----------

#[derive(Clone)]
pub struct AppSettings {
    pub theme: SharedString,
    pub language: SharedString,
    pub auto_launch: bool,
    pub show_descriptions: bool,
    pub search_in_desc: bool,
    pub max_results: SharedString,
    /// 窗口上次所在屏幕，用于多屏居中时决定目标屏幕。
    pub last_display: Option<DisplayId>,
}

impl Global for AppSettings {}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            language: "zh".into(),
            auto_launch: false,
            show_descriptions: true,
            search_in_desc: true,
            max_results: "15".into(),
            last_display: None,
        }
    }
}

// ---------- 设置页面数据 ----------

pub fn build_settings_pages() -> Vec<SettingPage> {
    let default = AppSettings::default();

    vec![
        SettingPage::new("外观")
            .icon(Icon::new(IconName::Settings2))
            .group(
                SettingGroup::new()
                    .title("主题与语言")
                    .item(
                        SettingItem::new(
                            "配色主题",
                            SettingField::dropdown(
                                vec![
                                    ("system".into(), "跟随系统".into()),
                                    ("light".into(), "浅色".into()),
                                    ("dark".into(), "深色".into()),
                                ],
                                |cx: &App| cx.global::<AppSettings>().theme.clone(),
                                |val: SharedString, cx: &mut App| {
                                    cx.global_mut::<AppSettings>().theme = val;
                                },
                            )
                            .default_value(default.theme.clone()),
                        )
                        .description("选择应用程序的配色主题"),
                    )
                    .item(
                        SettingItem::new(
                            "界面语言",
                            SettingField::dropdown(
                                vec![
                                    ("zh".into(), "简体中文".into()),
                                    ("en".into(), "English".into()),
                                ],
                                |cx: &App| cx.global::<AppSettings>().language.clone(),
                                |val: SharedString, cx: &mut App| {
                                    cx.global_mut::<AppSettings>().language = val;
                                },
                            )
                            .default_value(default.language.clone()),
                        )
                        .description("选择界面显示语言"),
                    ),
            )
            .group(
                SettingGroup::new()
                    .title("显示")
                    .item(
                        SettingItem::new(
                            "显示应用描述",
                            SettingField::switch(
                                |cx: &App| cx.global::<AppSettings>().show_descriptions,
                                |val: bool, cx: &mut App| {
                                    cx.global_mut::<AppSettings>().show_descriptions = val;
                                },
                            )
                            .default_value(default.show_descriptions),
                        )
                        .description("在列表中显示应用程序的描述文字"),
                    ),
            ),
        SettingPage::new("行为")
            .icon(Icon::new(IconName::Settings))
            .group(
                SettingGroup::new()
                    .title("启动")
                    .item(
                        SettingItem::new(
                            "开机自动启动",
                            SettingField::switch(
                                |cx: &App| cx.global::<AppSettings>().auto_launch,
                                |val: bool, cx: &mut App| {
                                    cx.global_mut::<AppSettings>().auto_launch = val;
                                },
                            )
                            .default_value(default.auto_launch),
                        )
                        .description("系统启动时自动运行程序启动器"),
                    ),
            )
            .group(
                SettingGroup::new()
                    .title("搜索")
                    .item(
                        SettingItem::new(
                            "搜索包含描述",
                            SettingField::switch(
                                |cx: &App| cx.global::<AppSettings>().search_in_desc,
                                |val: bool, cx: &mut App| {
                                    cx.global_mut::<AppSettings>().search_in_desc = val;
                                },
                            )
                            .default_value(default.search_in_desc),
                        )
                        .description("搜索时同时匹配应用程序的描述文字"),
                    )
                    .item(
                        SettingItem::new(
                            "最大显示数量",
                            SettingField::dropdown(
                                vec![
                                    ("5".into(), "5 条".into()),
                                    ("10".into(), "10 条".into()),
                                    ("15".into(), "15 条".into()),
                                    ("20".into(), "20 条".into()),
                                ],
                                |cx: &App| cx.global::<AppSettings>().max_results.clone(),
                                |val: SharedString, cx: &mut App| {
                                    cx.global_mut::<AppSettings>().max_results = val;
                                },
                            )
                            .default_value(default.max_results.clone()),
                        )
                        .description("搜索结果列表最多显示的应用数量"),
                    ),
            ),
    ]
}

// ---------- 设置窗口视图 ----------

pub struct SettingsView;

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(cx.theme().background)
            .child(Settings::new("settings").pages(build_settings_pages()))
    }
}
