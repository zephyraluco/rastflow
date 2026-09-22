#![windows_subsystem = "windows"]

mod config;
mod bindings;
mod settings;
mod layout;
mod locale;
mod utils;
mod icons;
mod app_icon;
mod tray;

use gpui::*;
use gpui_kit::component::*;
use rust_embed::RustEmbed;
use std::borrow::Cow;

#[derive(RustEmbed)]
#[folder = "./assets"]
#[include = "icons/**/*.svg"]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        if path.is_empty() {
            return Ok(None);
        }
        Self::get(path)
            .map(|f| Some(f.data))
            .ok_or_else(|| anyhow::anyhow!("could not find asset at path \"{path}\""))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(Self::iter()
            .filter_map(|p| p.starts_with(path).then(|| p.into()))
            .collect())
    }
}
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use global_hotkey::hotkey::{Code, HotKey, Modifiers};

use layout::LauncherView;
use settings::AppSettings;
use tray::TrayEvent;
use utils::{auto_launch_is_enabled, auto_launch_set, center_window, hide_window, show_window};

// ---------- 快捷键解析 ----------

/// 将 "alt+space"、"ctrl+shift+space" 等字符串解析为 HotKey。
/// 若字符串无法识别主键则返回 None。
fn parse_hotkey(s: &str) -> Option<HotKey> {
    let mut mods = Modifiers::empty();
    let mut code: Option<Code> = None;

    for part in s.to_lowercase().split('+') {
        match part.trim() {
            "alt"                        => mods |= Modifiers::ALT,
            "ctrl" | "control"           => mods |= Modifiers::CONTROL,
            "shift"                      => mods |= Modifiers::SHIFT,
            "super" | "win" | "meta" | "cmd" => mods |= Modifiers::SUPER,
            "space"  => code = Some(Code::Space),
            "tab"    => code = Some(Code::Tab),
            "enter"  => code = Some(Code::Enter),
            _ => {}
        }
    }

    code.map(|c| HotKey::new(if mods.is_empty() { None } else { Some(mods) }, c))
}

// ---------- 托盘图标 ----------

/// 系统托盘图标应使用的边长（像素）。
///
/// 取 `SM_CXSMICON`，也就是通知区域当前用的小图标尺寸：96 DPI 下是 16，
/// 150% 缩放是 24，200% 是 32。写死 16 的话，高分屏上图标会被系统硬放大而发糊。
///
/// 两点说明：
/// - 未设置 DPI 上下文时 `GetSystemMetrics` 返回的是**系统** DPI 下的取值
///   （PerMonitorV2 进程亦然）。托盘通常就在主显示器上，够用。
/// - 只在启动时算一次，所以之后改变缩放或把任务栏拖到别的显示器，
///   托盘图标不会重建 —— 要做的话得在 `WM_DPICHANGED` 时重新 `set_icon`。
fn tray_icon_size() -> u32 {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON};

        let size = unsafe { GetSystemMetrics(SM_CXSMICON) };
        if size > 0 {
            // 夹一下，避免异常的系统度量把图标做成一张巨图
            return (size as u32).clamp(8, 64);
        }
    }
    16
}

// ---------- 常量 ----------

/// 启动器窗口的逻辑宽度（gpui Pixels 单位）。
pub const WIN_W: f32 = 660.0;
/// 启动器窗口的逻辑高度（gpui Pixels 单位）。
pub const WIN_H: f32 = 520.0;

// ---------- 入口 ----------

fn main() {
    gpui_kit::application().with_assets(Assets).run(move |cx| {
        gpui_kit::init(cx);
        cx.set_global(AppSettings::default());
        // 从 settings.json 加载持久化设置（主题、语言等）
        {
            let persisted = settings::load_settings();
            let s = cx.global_mut::<AppSettings>();
            if !persisted.theme.is_empty() { s.theme = persisted.theme.into(); }
            if !persisted.language.is_empty() { s.language = persisted.language.into(); }
        }
        // 设置变更时自动写入 settings.json
        cx.observe_global::<AppSettings>(|cx| {
            settings::save_settings(cx.global::<AppSettings>());
        }).detach();
        // 启动时从注册表读取实际自启状态并同步到设置，确保开关显示正确
        cx.global_mut::<AppSettings>().auto_launch = auto_launch_is_enabled();
        // 启动时应用已保存的主题设置
        {
            let saved_theme = cx.global::<AppSettings>().theme.clone();
            match saved_theme.as_ref() {
                "dark"  => Theme::change(ThemeMode::Dark, None, cx),
                "light" => Theme::change(ThemeMode::Light, None, cx),
                _       => Theme::sync_system_appearance(None, cx),
            }
        }

        cx.spawn(async move |cx| {
            // ── 打开启动器窗口（居中于主显示器）───────────────────────────
            let win_size = size(px(WIN_W), px(WIN_H));
            // 在 async spawn 中需通过 cx.update() 访问 &App
            let initial_bounds = cx.update(|cx| WindowBounds::centered(win_size, cx));

            let window_handle = cx
                .open_window(
                    WindowOptions {
                        // 启动时自动居中到主显示器
                        window_bounds: Some(initial_bounds),
                        titlebar: Some(TitlebarOptions {
                            title: None,
                            appears_transparent: true,
                            traffic_light_position: Some(point(px(9.), px(9.))),
                        }),
                        // PopUp 窗口不在任务栏和 Alt+Tab 中显示
                        kind: WindowKind::PopUp,
                        // 静默启动：创建时不显示、不激活，等待热键或托盘唤出
                        show: false,
                        focus: false,
                        ..Default::default()
                    },
                    |window, cx| {
                        let view = cx.new(|cx| LauncherView::new(window, cx));
                        cx.new(|cx| Root::new(view, window, cx))
                    },
                )
                .expect("Failed to open window");

            // ── 系统托盘 ─────────────────────────────────────────────────
            // 必须在主线程（也就是拥有消息循环的线程）创建：托盘窗口靠该线程的
            // 消息循环接收交互，而 gpui 的 spawn 任务正是跑在主线程上。
            let tray = match crate::tray::TrayIcon::new("rastflow", tray_icon_size()) {
                Ok(tray) => Some(tray),
                Err(err) => {
                    // 托盘不可用时仍可靠全局快捷键工作，所以只告警、不退出
                    eprintln!("[rastflow] 系统托盘不可用：{err}");
                    None
                }
            };

            // ── 全局快捷键 ────────────────────────────────────────────────
            // GlobalHotKeyManager 必须在拥有 Win32 消息循环的线程上创建。
            // gpui 的 spawn 任务运行在主线程，与消息循环同线程，故此处安全。
            let hk_manager = GlobalHotKeyManager::new()
                .expect("Failed to create GlobalHotKeyManager");

            // 读取初始快捷键设置并注册
            let mut current_hotkey_str: String =
                cx.update(|cx| cx.global::<AppSettings>().hotkey.to_string());
            let mut registered_hotkey_id: Option<u32> = None;

            // 追踪 auto_launch 设置变化
            let mut current_auto_launch: bool =
                cx.update(|cx| cx.global::<AppSettings>().auto_launch);

            if let Some(hk) = parse_hotkey(&current_hotkey_str) {
                let id = hk.id();
                if hk_manager.register(hk).is_ok() {
                    registered_hotkey_id = Some(id);
                } else {
                    eprintln!("快捷键注册失败：{current_hotkey_str}");
                }
            }

            // 获取后台执行器，用于定时等待
            let bg = cx.update(|cx| cx.background_executor().clone());

            // ── 静默启动 Everything（若未运行）─────────────────────────
            // 在后台线程检测 Everything 进程，未运行则在安装位置找到
            // Everything.exe 并以 -startup 静默启动，不阻塞窗口创建。
            bg.spawn(async {
                layout::everything::ensure_running();
            })
            .detach();

            // ── 托盘事件轮询循环 ─────────────────────────────────────────
            loop {
                // 检测 auto_launch 设置变化，同步到注册表
                let new_auto_launch: bool =
                    cx.update(|cx| cx.global::<AppSettings>().auto_launch);
                if new_auto_launch != current_auto_launch {
                    auto_launch_set(new_auto_launch);
                    current_auto_launch = new_auto_launch;
                }

                // 检测快捷键设置是否发生变化，若变则重新注册
                let new_hotkey_str: String = cx
                    .update(|cx| cx.global::<AppSettings>().hotkey.to_string());
                if new_hotkey_str != current_hotkey_str {
                    // 注销旧快捷键
                    if let Some(old_id) = registered_hotkey_id.take() {
                        if let Some(hk) = parse_hotkey(&current_hotkey_str) {
                            if hk.id() == old_id {
                                let _ = hk_manager.unregister(hk);
                            }
                        }
                    }
                    // 注册新快捷键
                    if let Some(hk) = parse_hotkey(&new_hotkey_str) {
                        let id = hk.id();
                        if hk_manager.register(hk).is_ok() {
                            registered_hotkey_id = Some(id);
                        } else {
                            eprintln!("快捷键注册失败：{new_hotkey_str}");
                        }
                    }
                    current_hotkey_str = new_hotkey_str;
                }

                // 处理全局快捷键事件 → 切换窗口显示/隐藏
                while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
                    if event.state == HotKeyState::Pressed
                        && registered_hotkey_id == Some(event.id)
                    {
                        cx.update(|cx| {
                            window_handle
                                .update(cx, |_, window, cx| {
                                    if window.is_window_active() {
                                        hide_window(window);
                                    } else {
                                        show_window(window);
                                        center_window(window, cx);
                                        window.activate_window();
                                    }
                                })
                                .ok();
                        });
                    }
                }

                // 处理托盘交互（左键单击、右键菜单项）
                if let Some(tray) = &tray {
                    while let Some(event) = tray.try_recv() {
                        if event == TrayEvent::Quit {
                            cx.update(|cx| cx.quit());
                            return;
                        }

                        // 左键单击总是唤出；菜单项则是显示/隐藏切换
                        let toggle = event == TrayEvent::ToggleWindow;
                        cx.update(|cx| {
                            window_handle
                                .update(cx, |_, window, cx| {
                                    if toggle && window.is_window_active() {
                                        hide_window(window);
                                    } else {
                                        // SW_SHOW 让隐藏窗口可见，再居中，再聚焦
                                        show_window(window);
                                        center_window(window, cx);
                                        window.activate_window();
                                    }
                                })
                                .ok();
                        });
                    }
                }

                // 每 50ms 轮询一次
                bg.timer(std::time::Duration::from_millis(50)).await;
            }
        })
        .detach();
    });
}
