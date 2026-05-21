use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{button::Button, setting::*, *};
use serde::{Deserialize, Serialize};

use crate::icons::IconName;
use crate::config::{load_entries_from_file, upsert_custom_entry};
use crate::locale::t;

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
    /// 唤出界面的全局快捷键，格式如 "alt+space"。
    pub hotkey: SharedString,
    /// AI 模型 API Key
    pub ai_api_key: SharedString,
    /// AI API Base URL（留空使用 OpenAI 官方地址）
    pub ai_base_url: SharedString,
    /// AI 模型名称，如 "gpt-4o"
    pub ai_model: SharedString,
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
            hotkey: "alt+space".into(),
            ai_api_key: "".into(),
            ai_base_url: "".into(),
            ai_model: "claude-opus-4-5".into(),
        }
    }
}

// ---------- 设置页面数据 ----------

pub fn build_settings_pages(lang: &str) -> Vec<SettingPage> {
    let default = AppSettings::default();
    // 将语言克隆到 'static str 用于闭包捕获
    let lang: &'static str = Box::leak(lang.to_string().into_boxed_str());

    vec![
        SettingPage::new(zh_en(lang, "外观", "Appearance"))
            .icon(Icon::new(IconName::Settings2))
            .group(
                SettingGroup::new()
                    .title(zh_en(lang, "主题与语言", "Theme & Language"))
                    .item(
                        SettingItem::new(
                            zh_en(lang, "配色主题", "Color Theme"),
                            SettingField::dropdown(
                                vec![
                                    ("system".into(), zh_en(lang, "跟随系统", "Follow System").into()),
                                    ("light".into(),  zh_en(lang, "浅色", "Light").into()),
                                    ("dark".into(),   zh_en(lang, "深色", "Dark").into()),
                                ],
                                |cx: &App| cx.global::<AppSettings>().theme.clone(),
                                |val: SharedString, cx: &mut App| {
                                    cx.global_mut::<AppSettings>().theme = val.clone();
                                    // 即时应用主题
                                    match val.as_ref() {
                                        "dark"   => Theme::change(ThemeMode::Dark, None, cx),
                                        "light"  => Theme::change(ThemeMode::Light, None, cx),
                                        _        => Theme::sync_system_appearance(None, cx),
                                    }
                                    cx.refresh_windows();
                                },
                            )
                            .default_value(default.theme.clone()),
                        )
                        .description(zh_en(lang, "选择应用程序的配色主题", "Choose the color theme for the application")),
                    )
                    .item(
                        SettingItem::new(
                            zh_en(lang, "界面语言", "Language"),
                            SettingField::dropdown(
                                vec![
                                    ("zh".into(), "简体中文".into()),
                                    ("en".into(), "English".into()),
                                ],
                                |cx: &App| cx.global::<AppSettings>().language.clone(),
                                |val: SharedString, cx: &mut App| {
                                    cx.global_mut::<AppSettings>().language = val;
                                    cx.refresh_windows();
                                },
                            )
                            .default_value(default.language.clone()),
                        )
                        .description(zh_en(lang, "选择界面显示语言", "Select the display language")),
                    ),
            )
            .group(
                SettingGroup::new()
                    .title(zh_en(lang, "显示", "Display"))
                    .item(
                        SettingItem::new(
                            zh_en(lang, "显示应用描述", "Show App Descriptions"),
                            SettingField::switch(
                                |cx: &App| cx.global::<AppSettings>().show_descriptions,
                                |val: bool, cx: &mut App| {
                                    cx.global_mut::<AppSettings>().show_descriptions = val;
                                },
                            )
                            .default_value(default.show_descriptions),
                        )
                        .description(zh_en(lang, "在列表中显示应用程序的描述文字", "Show app descriptions in the list")),
                    ),
            ),
        SettingPage::new(zh_en(lang, "行为", "Behavior"))
            .icon(Icon::new(IconName::Settings))
            .group(
                SettingGroup::new()
                    .title(zh_en(lang, "启动", "Startup"))
                    .item(
                        SettingItem::new(
                            zh_en(lang, "开机自动启动", "Launch at Login"),
                            SettingField::switch(
                                |cx: &App| cx.global::<AppSettings>().auto_launch,
                                |val: bool, cx: &mut App| {
                                    cx.global_mut::<AppSettings>().auto_launch = val;
                                },
                            )
                            .default_value(default.auto_launch),
                        )
                        .description(zh_en(lang, "系统启动时自动运行程序启动器", "Automatically start the launcher at system boot")),
                    ),
            )
            .group(
                SettingGroup::new()
                    .title(zh_en(lang, "搜索", "Search"))
                    .item(
                        SettingItem::new(
                            zh_en(lang, "搜索包含描述", "Search in Descriptions"),
                            SettingField::switch(
                                |cx: &App| cx.global::<AppSettings>().search_in_desc,
                                |val: bool, cx: &mut App| {
                                    cx.global_mut::<AppSettings>().search_in_desc = val;
                                },
                            )
                            .default_value(default.search_in_desc),
                        )
                        .description(zh_en(lang, "搜索时同时匹配应用程序的描述文字", "Include app descriptions when searching")),
                    )
                    .item(
                        SettingItem::new(
                            zh_en(lang, "最大显示数量", "Max Results"),
                            SettingField::dropdown(
                                vec![
                                    ("5".into(),  zh_en(lang, "5 条",  "5").into()),
                                    ("10".into(), zh_en(lang, "10 条", "10").into()),
                                    ("15".into(), zh_en(lang, "15 条", "15").into()),
                                    ("20".into(), zh_en(lang, "20 条", "20").into()),
                                ],
                                |cx: &App| cx.global::<AppSettings>().max_results.clone(),
                                |val: SharedString, cx: &mut App| {
                                    cx.global_mut::<AppSettings>().max_results = val;
                                },
                            )
                            .default_value(default.max_results.clone()),
                        )
                        .description(zh_en(lang, "搜索结果列表最多显示的应用数量", "Maximum number of results shown in the list")),
                    ),
            ),
        SettingPage::new(zh_en(lang, "快捷键", "Hotkeys"))
            .icon(Icon::new(IconName::Star))
            .group(
                SettingGroup::new()
                    .title(zh_en(lang, "全局快捷键", "Global Hotkeys"))
                    .item(
                        SettingItem::new(
                            zh_en(lang, "唤出界面", "Show Launcher"),
                            SettingField::dropdown(
                                vec![
                                    ("alt+space".into(),        "Alt + Space".into()),
                                    ("ctrl+space".into(),       "Ctrl + Space".into()),
                                    ("ctrl+alt+space".into(),   "Ctrl + Alt + Space".into()),
                                    ("super+space".into(),      "Win + Space".into()),
                                    ("ctrl+shift+space".into(), "Ctrl + Shift + Space".into()),
                                ],
                                |cx: &App| cx.global::<AppSettings>().hotkey.clone(),
                                |val: SharedString, cx: &mut App| {
                                    cx.global_mut::<AppSettings>().hotkey = val;
                                },
                            )
                            .default_value(default.hotkey.clone()),
                        )
                        .description(zh_en(
                            lang,
                            "按下此快捷键可随时从任意窗口唤出启动器界面",
                            "Press this hotkey to open the launcher from anywhere",
                        )),
                    ),
            ),
        SettingPage::new(zh_en(lang, "自定义程序", "Custom Apps"))
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
                                    .child(zh_en(lang, "暂未添加自定义程序", "No custom apps added yet")),
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
                                        .child(zh_en(lang, "添加程序", "Add App"))
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
                                                                "", "", &path,
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
        SettingPage::new(zh_en(lang, "AI 设置", "AI Settings"))
            .icon(Icon::new(IconName::Bot))
            .group(
                SettingGroup::new()
                    .title(zh_en(lang, "模型", "Model"))
                    .item(
                        SettingItem::new(
                            zh_en(lang, "AI 模型", "AI Model"),
                            SettingField::input(
                                |cx: &App| cx.global::<AppSettings>().ai_model.clone(),
                                |val: SharedString, cx: &mut App| {
                                    cx.global_mut::<AppSettings>().ai_model = val;
                                },
                            )
                            .default_value(default.ai_model.clone()),
                        )
                        .description(zh_en(
                            lang,
                            "模型名称，如 claude-opus-4-5、claude-sonnet-4-5",
                            "Model name, e.g. claude-opus-4-5, claude-sonnet-4-5",
                        )),
                    )
                    .item(
                        SettingItem::new(
                            zh_en(lang, "API 地址", "API Base URL"),
                            SettingField::input(
                                |cx: &App| cx.global::<AppSettings>().ai_base_url.clone(),
                                |val: SharedString, cx: &mut App| {
                                    cx.global_mut::<AppSettings>().ai_base_url = val;
                                },
                            )
                            .default_value(default.ai_base_url.clone()),
                        )
                        .description(zh_en(
                            lang,
                            "留空使用 Anthropic 官方接口，或填入自定义 API 地址",
                            "Leave empty for Anthropic default, or enter a custom endpoint",
                        )),
                    ),
            )
            .group(
                SettingGroup::new()
                    .title(zh_en(lang, "认证", "Authentication"))
                    .item(
                        SettingItem::new(
                            zh_en(lang, "API Key", "API Key"),
                            SettingField::input(
                                |cx: &App| cx.global::<AppSettings>().ai_api_key.clone(),
                                |val: SharedString, cx: &mut App| {
                                    cx.global_mut::<AppSettings>().ai_api_key = val;
                                },
                            )
                            .default_value(default.ai_api_key.clone()),
                        )
                        .description(zh_en(
                            lang,
                            "优先使用此处填写的 Key；留空则读取 ANTHROPIC_API_KEY 环境变量",
                            "This key takes priority; falls back to ANTHROPIC_API_KEY env var if empty",
                        )),
                    ),
            ),
    ]
}

// ---------- 辅助函数 ----------

/// 根据语言返回中文或英文字符串。
fn zh_en(lang: &str, zh: &'static str, en: &'static str) -> &'static str {
    if lang == "en" { en } else { zh }
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
        let lang = cx.global::<AppSettings>().language.to_string();
        let title = t("settings.title", cx);
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .child(
                TitleBar::new().child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .text_color(cx.theme().foreground)
                        .child(title),
                ),
            )
            .child(Settings::new("settings").pages(build_settings_pages(&lang)))
    }
}

// ---------- 设置持久化 ----------

/// 持久化到 settings.json 的字段子集。
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedSettings {
    pub theme: String,
    pub language: String,
    pub ai_api_key: String,
    pub ai_base_url: String,
    pub ai_model: String,
}

impl Default for PersistedSettings {
    fn default() -> Self {
        let d = AppSettings::default();
        Self {
            theme: d.theme.to_string(),
            language: d.language.to_string(),
            ai_api_key: d.ai_api_key.to_string(),
            ai_base_url: d.ai_base_url.to_string(),
            ai_model: d.ai_model.to_string(),
        }
    }
}

pub fn settings_path() -> std::path::PathBuf {
    std::env::current_dir()
        .unwrap_or_default()
        .join("settings.json")
}

/// 从 settings.json 加载持久化设置，文件不存在时返回默认值。
pub fn load_settings() -> PersistedSettings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 将 AppSettings 中需要持久化的字段写入 settings.json。
pub fn save_settings(s: &AppSettings) {
    let p = PersistedSettings {
        theme: s.theme.to_string(),
        language: s.language.to_string(),
        ai_api_key: s.ai_api_key.to_string(),
        ai_base_url: s.ai_base_url.to_string(),
        ai_model: s.ai_model.to_string(),
    };
    if let Ok(data) = serde_json::to_string_pretty(&p) {
        let _ = std::fs::write(settings_path(), data);
    }
}
