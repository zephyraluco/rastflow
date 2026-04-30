mod settings;
mod layout;
mod utils;

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

use layout::LauncherView;
use settings::AppSettings;
use utils::{center_window, hide_window, show_window};

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
                        ..Default::default()
                    },
                    |window, cx| {
                        let view = cx.new(|cx| LauncherView::new(window, cx));
                        cx.new(|cx| Root::new(view, window, cx))
                    },
                )
                .expect("Failed to open window");

            // 激活窗口，使其获得键盘焦点
            let _ = cx.update(|cx| {
                window_handle
                    .update(cx, |_, window, _cx| window.activate_window())
                    .ok();
            });

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
                .with_tooltip("flux")
                .with_icon(tray_icon_img)
                .build()
                .expect("Failed to build tray icon");

            let toggle_id = toggle_item.id().clone();
            let quit_id   = quit_item.id().clone();

            // 获取后台执行器，用于定时等待
            let bg = cx.update(|cx| cx.background_executor().clone());

            // ── 托盘事件轮询循环 ─────────────────────────────────────────
            loop {
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
