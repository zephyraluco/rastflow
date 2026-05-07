use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{button::Button, setting::*, *};

use crate::layout::{load_entries_from_file, upsert_custom_entry};

// ---------- 文件选择对话框 ----------

#[cfg(windows)]
fn pick_program_file() -> Option<String> {
    use windows::{
        Win32::{
            System::Com::{
                CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL,
                COINIT_APARTMENTTHREADED,
            },
            UI::Shell::{
                Common::COMDLG_FILTERSPEC, FileOpenDialog, IFileOpenDialog,
                SIGDN_FILESYSPATH,
            },
        },
        core::w,
    };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL).ok()?;

        let filters = [
            COMDLG_FILTERSPEC {
                pszName: w!("程序文件"),
                pszSpec: w!("*.exe;*.lnk;*.url;*.cmd;*.bat"),
            },
            COMDLG_FILTERSPEC {
                pszName: w!("所有文件"),
                pszSpec: w!("*.*"),
            },
        ];
        let _ = dialog.SetFileTypes(&filters);

        if dialog.Show(None).is_err() {
            return None;
        }

        let item = dialog.GetResult().ok()?;
        let path_pwstr = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;

        let mut ptr = path_pwstr.0;
        let mut len = 0usize;
        while *ptr != 0 {
            ptr = ptr.add(1);
            len += 1;
        }
        let slice = std::slice::from_raw_parts(path_pwstr.0, len);
        let path = String::from_utf16_lossy(slice);
        CoTaskMemFree(Some(path_pwstr.0 as *mut core::ffi::c_void as *const _));

        Some(path)
    }
}

#[cfg(not(windows))]
fn pick_program_file() -> Option<String> {
    None
}

// ---------- 全局设置 ----------

#[derive(Clone)]
pub struct AppSettings {
    pub theme: SharedString,
    pub language: SharedString,
    pub auto_launch: bool,
    pub show_descriptions: bool,
    pub search_in_desc: bool,
    pub max_results: SharedString,
    /// 自定义程序列表版本号，变更时触发设置页重渲染。
    pub custom_programs_version: u64,
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
            custom_programs_version: 0,
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
        SettingPage::new("自定义程序")
            .icon(Icon::new(IconName::Plus))
            .group(
                SettingGroup::new().item(SettingItem::render(|_opts, _win, cx| {
                    // 读取版本号：使此闭包订阅 AppSettings 变更，添加程序后自动刷新
                    let _v = cx.global::<AppSettings>().custom_programs_version;
                    let entries: Vec<(String, String)> = load_entries_from_file()
                        .into_iter()
                        .filter(|e| e.category.as_ref() == "自定义程序")
                        .map(|e| (e.name.to_string(), e.launch_target.unwrap_or_default()))
                        .collect();

                    let fg = cx.theme().foreground;
                    let muted = cx.theme().muted_foreground;
                    let border = cx.theme().border;
                    let strip_a = cx.theme().background;
                    let strip_b = cx.theme().muted;

                    v_flex()
                        .w_full()
                        .gap_2()
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
                                    .child("暂未添加自定义程序"),
                            )
                        })
                        .when(!entries.is_empty(), |this| {
                            this.child(
                                v_flex()
                                    .w_full()
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(border)
                                    .overflow_hidden()
                                    .children(entries.into_iter().enumerate().map(
                                        |(i, (name, path))| {
                                            h_flex()
                                                .w_full()
                                                .px_3()
                                                .py_2()
                                                .gap_3()
                                                .bg(if i % 2 == 0 { strip_a } else { strip_b })
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .text_sm()
                                                        .font_semibold()
                                                        .text_color(fg)
                                                        .overflow_hidden()
                                                        .child(name),
                                                )
                                                .child(
                                                    div()
                                                        .flex_shrink_0()
                                                        .text_xs()
                                                        .text_color(muted)
                                                        .max_w_64()
                                                        .overflow_hidden()
                                                        .child(path),
                                                )
                                        },
                                    )),
                            )
                        })
                        .child(
                            div()
                                .w_full()
                                .pt_3()
                                .flex()
                                .justify_center()
                                .child(
                                    Button::new("add-program-btn")
                                        .child("添加程序")
                                        .on_click(|_, _, cx| {
                                            // 必须在独立 OS 线程上运行 COM 文件对话框，
                                            // 否则 dialog.Show() 的内部消息泵会在
                                            // App RefCell 已借用时触发 gpui 回调 → panic。
                                            let (tx, rx) =
                                                std::sync::mpsc::sync_channel::<Option<String>>(1);
                                            std::thread::spawn(move || {
                                                tx.send(pick_program_file()).ok();
                                            });
                                            cx.spawn(async move |async_cx: &mut gpui::AsyncApp| {
                                                let picked: Option<String> = async_cx
                                                    .background_executor()
                                                    .spawn(async move {
                                                        rx.recv().ok().flatten()
                                                    })
                                                    .await;
                                                if let Some(path) = picked {
                                                    async_cx
                                                        .update(|cx| {
                                                            if let Err(e) = upsert_custom_entry(
                                                                "", "", "", &path,
                                                            ) {
                                                                eprintln!("添加程序失败: {e}");
                                                            }
                                                            cx.global_mut::<AppSettings>()
                                                                .custom_programs_version +=
                                                                1;
                                                        })
                                                        ;
                                                }
                                            })
                                            .detach();
                                        }),
                                ),
                        )
                })),
            ),
    ]
}

// ---------- 设置窗口视图 ----------

pub struct SettingsView {
    // 订阅 AppSettings 全局变更，确保添加程序后列表自动刷新
    _global_sub: Subscription,
}

impl SettingsView {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let _global_sub =
            cx.observe_global::<AppSettings>(|_, cx| cx.notify());
        Self { _global_sub }
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .child(
                TitleBar::new().child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .text_color(cx.theme().foreground)
                        .child("设置"),
                ),
            )
            .child(Settings::new("settings").pages(build_settings_pages()))
    }
}
