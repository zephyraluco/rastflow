#![windows_subsystem = "windows"]

mod settings;
mod layout;
mod locale;
mod utils;
mod icons;

use gpui::*;
use gpui_component::*;
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
use tray_icon::{
    TrayIconBuilder, TrayIconEvent,
    MouseButton as TrayMouseButton,
    menu::{Menu as TrayMenu, MenuEvent, MenuItem, PredefinedMenuItem},
};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use global_hotkey::hotkey::{Code, HotKey, Modifiers};

use layout::LauncherView;
use settings::AppSettings;
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

// ---------- 常量 ----------

/// 启动器窗口的逻辑宽度（gpui Pixels 单位）。
pub const WIN_W: f32 = 660.0;
/// 启动器窗口的逻辑高度（gpui Pixels 单位）。
pub const WIN_H: f32 = 520.0;

// ---------- 入口 ----------

fn main() {
    gpui_platform::application().with_assets(Assets).run(move |cx| {
        gpui_component::init(cx);
        cx.set_global(AppSettings::default());
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
            // 构建右键菜单
            let tray_menu = TrayMenu::new();
            let toggle_item = MenuItem::new("显示 / 隐藏", true, None);
            let quit_item   = MenuItem::new("退出", true, None);
            tray_menu.append(&toggle_item).unwrap();
            tray_menu.append(&PredefinedMenuItem::separator()).unwrap();
            tray_menu.append(&quit_item).unwrap();

            // 生成一个 16×16 的蓝色圆形图标
            let mut icon_rgba = vec![0u8; 16 * 16 * 4];
            for y in 0..16i32 {
                for x in 0..16i32 {
                    let i = ((y * 16 + x) * 4) as usize;
                    let dx = x - 7;
                    let dy = y - 7;
                    if dx * dx + dy * dy <= 36 {
                        icon_rgba[i]     = 0;
                        icon_rgba[i + 1] = 120;
                        icon_rgba[i + 2] = 215;
                        icon_rgba[i + 3] = 255;
                    }
                }
            }
            let tray_icon_img = tray_icon::Icon::from_rgba(icon_rgba, 16, 16)
                .expect("Failed to create tray icon");

            // 创建托盘图标（必须在主线程创建，spawn 任务运行在主线程上）
            let _tray = TrayIconBuilder::new()
                .with_menu(Box::new(tray_menu))
                .with_menu_on_left_click(false)   // 左键不弹菜单，仅右键弹菜单
                .with_tooltip("rastflow")
                .with_icon(tray_icon_img)
                .build()
                .expect("Failed to build tray icon");

            let toggle_id = toggle_item.id().clone();
            let quit_id   = quit_item.id().clone();

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

                // 处理托盘图标左键单击 → 居中并显示窗口
                while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                    if let TrayIconEvent::Click {
                        button: TrayMouseButton::Left, ..
                    } = event
                    {
                        cx.update(|cx| {
                            window_handle
                                .update(cx, |_, window, cx| {
                                    // SW_SHOW 让隐藏窗口可见，再居中，再聚焦
                                    show_window(window);
                                    center_window(window, cx);
                                    window.activate_window();
                                })
                                .ok();
                        });
                    }
                }

                // 处理右键菜单事件
                while let Ok(event) = MenuEvent::receiver().try_recv() {
                    if event.id == quit_id {
                        // 退出应用
                        cx.update(|cx| cx.quit());
                        return;
                    } else if event.id == toggle_id {
                        // 显示 / 隐藏切换
                        cx.update(|cx| {
                            window_handle
                                .update(cx, |_, window, cx| {
                                    if window.is_window_active() {
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
