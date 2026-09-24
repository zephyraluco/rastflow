/// 设置页面构建：各分组配置项 + 文件选择对话框

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_kit::component::{button::{Button, ButtonVariants}, setting::*, *};

use crate::config::{load_entries_from_file, upsert_custom_entry};
use crate::icons::IconName;
use crate::update::{self, UpdateStatus};

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
        SettingPage::new(zh_en(lang, "关于", "About"))
            .icon(Icon::new(IconName::Info))
            .group(
                SettingGroup::new()
                    .title(zh_en(lang, "版本与更新", "Version & Updates"))
                    .item(
                        SettingItem::new(
                            zh_en(lang, "当前版本", "Current Version"),
                            SettingField::render(|_, _, _| {
                                div()
                                    .text_sm()
                                    .child(update::current_version())
                                    .into_any_element()
                            }),
                        ),
                    )
                    .item(
                        SettingItem::new(
                            zh_en(lang, "自动检查更新", "Check Automatically"),
                            SettingField::switch(
                                |cx: &App| cx.global::<AppSettings>().auto_check_update,
                                |val: bool, cx: &mut App| {
                                    cx.global_mut::<AppSettings>().auto_check_update = val;
                                    // 刚打开开关就顺手查一次：否则用户要等到下一个检查周期
                                    // （24 小时）才能看到结果，会以为开关没生效。
                                    // 已在忙时 check() 自己会忽略。
                                    if val {
                                        update::check();
                                    }
                                },
                            )
                            .default_value(default.auto_check_update),
                        )
                        .description(zh_en(
                            lang,
                            "启动后自动检查是否有新版本，发现后可在下方一键升级",
                            "Check for new versions on startup; upgrade below with one click",
                        )),
                    )
                    // 状态行 + 两个按钮属于「宽内容」，放进右侧那一列会被挤得换行。
                    // 跟「自定义程序」页一样改用整行的 SettingItem::render。
                    .item(SettingItem::render(move |_, _, cx| {
                        render_update_field(lang, cx)
                    })),
            ),
    ]
}

// ---------- 关于页：更新状态与操作 ----------

/// 渲染「更新」这一项的内容：状态行 + 变更摘要 + 两个按钮。
///
/// 每次都重新读 [`update::status`]，状态变化靠 `SettingsView` 的轮询触发重渲染。
fn render_update_field(lang: &'static str, cx: &App) -> AnyElement {
    let status = update::status();
    let fg = cx.theme().foreground;
    let muted = cx.theme().muted_foreground;

    // 每个阶段都要让用户看得出「现在到底在干什么」，尤其是失败时
    let (icon, text) = match &status {
        UpdateStatus::Idle => (None, zh_en(lang, "尚未检查", "Not checked yet").to_string()),
        UpdateStatus::Checking => (
            Some(IconName::LoaderCircle),
            zh_en(lang, "正在检查…", "Checking…").to_string(),
        ),
        UpdateStatus::UpToDate { version } => (
            Some(IconName::CircleCheck),
            format!("{}（{version}）", zh_en(lang, "已是最新版本", "Up to date")),
        ),
        UpdateStatus::Available { version, .. } => (
            Some(IconName::ArrowDown),
            format!(
                "{}：{version}",
                zh_en(lang, "发现新版本", "New version available")
            ),
        ),
        UpdateStatus::Downloading { got, total } => (
            Some(IconName::LoaderCircle),
            match total {
                // total 为 0 表示服务端没给 Content-Length，此时只能报已下载量
                Some(total) if *total > 0 => format!(
                    "{}：{} / {}",
                    zh_en(lang, "正在下载", "Downloading"),
                    human_size(*got),
                    human_size(*total)
                ),
                _ => format!("{}…", zh_en(lang, "正在下载", "Downloading")),
            },
        ),
        UpdateStatus::Installing { version } => (
            Some(IconName::LoaderCircle),
            format!(
                "{}：{version}（{}）",
                zh_en(lang, "正在安装", "Installing"),
                zh_en(lang, "程序会自动重启", "the app will restart automatically")
            ),
        ),
        UpdateStatus::Failed { reason } => (
            Some(IconName::TriangleAlert),
            format!("{}：{reason}", zh_en(lang, "检查失败", "Failed")),
        ),
    };

    let failed = matches!(status, UpdateStatus::Failed { .. });
    let notes = match &status {
        UpdateStatus::Available { notes, .. } if !notes.trim().is_empty() => Some(notes.clone()),
        _ => None,
    };

    v_flex()
        .w_full()
        .gap_2()
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap_2()
                .text_sm()
                .text_color(if failed { muted } else { fg })
                .when_some(icon, |this, icon| this.child(Icon::new(icon)))
                .child(text),
        )
        .when_some(notes, |this, notes| {
            this.child(
                div()
                    .w_full()
                    .max_h_32()
                    .overflow_hidden()
                    .rounded_md()
                    .bg(cx.theme().muted)
                    .p_2()
                    .text_xs()
                    .text_color(muted)
                    .child(notes),
            )
        })
        .child(
            h_flex()
                .w_full()
                .gap_2()
                .child(
                    Button::new("update-check-btn")
                        .ghost()
                        .child(zh_en(lang, "检查更新", "Check for Updates"))
                        // 正在检查/下载时禁掉，避免重复触发（update::check 也会拦，
                        // 但按钮变灰能让用户看出「已经在做了」）
                        .disabled(status.is_busy())
                        .on_click(|_, _, _| update::check()),
                )
                .when(status.can_install(), |this| {
                    this.child(
                        Button::new("update-install-btn")
                            .child(zh_en(lang, "立即升级", "Update Now"))
                            .on_click(|_, _, _| update::install()),
                    )
                }),
        )
        .into_any_element()
}

/// 把字节数转成人类可读的形式（仅用于下载进度）。
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
