/// 设置页面构建：各分组配置项 + 文件选择对话框

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{button::Button, setting::*, *};

use crate::config::{load_entries_from_file, upsert_custom_entry};
use crate::icons::IconName;

use super::global::AppSettings;

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

// ---------- 辅助 ----------

/// 根据语言返回中文或英文字符串。
pub(super) fn zh_en(lang: &str, zh: &'static str, en: &'static str) -> &'static str {
    if lang == "en" { en } else { zh }
}

// ---------- 页面构建 ----------

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
                                                                .custom_programs_version += 1;
                                                        });
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
