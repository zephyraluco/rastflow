use std::ops::Deref;
use std::path::PathBuf;

use gpui::*;

use crate::settings::AppSettings;

// ---------- 应用数据目录 ----------

/// 返回 `%LOCALAPPDATA%\rastflow\`，自动创建目录。
///
/// 在所有 Windows 用户上下文中均可写，包括开机自启场景。
/// 若 LOCALAPPDATA 不可用（极端情况），回落至 exe 所在目录。
pub fn app_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(dir) = std::env::var("LOCALAPPDATA") {
            let path = PathBuf::from(dir).join("rastflow");
            let _ = std::fs::create_dir_all(&path);
            return path;
        }
    }
    // 回退
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

// ---------- 窗口可见性 ----------

/// 彻底隐藏窗口（SW_HIDE），不在任务栏/桌面留下图标。
/// gpui 的 `minimize_window()` 调用 SW_MINIMIZE，对 PopUp 窗口会产生桌面图标，故改用此函数。
pub fn hide_window(window: &mut Window) {
    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};

        if let Ok(rw) = window.window_handle() {
            if let RawWindowHandle::Win32(win32) = rw.as_raw() {
                let hwnd = HWND(win32.hwnd.get() as *mut core::ffi::c_void);
                unsafe { let _ = ShowWindow(hwnd, SW_HIDE); }
            }
        }
    }
}

/// 将已隐藏的窗口重新设为可见（SW_SHOW）。
/// 调用此函数后再调用 `window.activate_window()` 完成聚焦。
pub fn show_window(window: &mut Window) {
    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOW};

        if let Ok(rw) = window.window_handle() {
            if let RawWindowHandle::Win32(win32) = rw.as_raw() {
                let hwnd = HWND(win32.hwnd.get() as *mut core::ffi::c_void);
                unsafe { let _ = ShowWindow(hwnd, SW_SHOW); }
            }
        }
    }
}

// ---------- 窗口居中 ----------

/// 将窗口移动到上次记录的屏幕中央（多屏）。
///
/// - 若已记录过屏幕（`LastDisplay` 全局），则居中到该屏幕的可见区域（排除任务栏）。
/// - 若尚未记录，则居中到主显示器。
///
/// **实现说明**：gpui 没有提供创建后移动窗口位置的 API，
/// 因此在 Windows 上通过 `HasWindowHandle` 拿到 HWND，
/// 再调用 Win32 `SetWindowPos` 完成移动。
pub fn center_window(window: &mut Window, cx: &impl Deref<Target = App>) {
    let cx: &App = cx;

    let display_id = cx
        .try_global::<AppSettings>()
        .and_then(|s| s.last_display);

    let win_size = size(px(crate::WIN_W), px(crate::WIN_H));

    let display = display_id
        .and_then(|id| cx.find_display(id))
        .or_else(|| cx.primary_display());

    let bounds = if let Some(d) = display {
        // 用 visible_bounds 排除任务栏，确保窗口不被遮挡
        Bounds::centered_at(d.visible_bounds().center(), win_size)
    } else {
        // 极端回退：无法获取显示器信息时放在左上角附近
        Bounds {
            origin: point(px(100.), px(100.)),
            size: win_size,
        }
    };

    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER,
        };

        // gpui 的 Pixels 是逻辑像素，乘以 scale_factor 得到设备像素
        let scale = window.scale_factor();
        let x = (bounds.origin.x.as_f32() * scale) as i32;
        let y = (bounds.origin.y.as_f32() * scale) as i32;
        let w = (win_size.width.as_f32() * scale) as i32;
        let h = (win_size.height.as_f32() * scale) as i32;

        if let Ok(rw) = window.window_handle() {
            if let RawWindowHandle::Win32(win32) = rw.as_raw() {
                let hwnd = HWND(win32.hwnd.get() as *mut core::ffi::c_void);
                unsafe {
                    let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
                }
            }
        }
    }

    // 非 Windows 平台（macOS/Linux）暂无原生移动 API，忽略
    #[cfg(not(windows))]
    let _ = bounds;
}

// ---------- 开机自启 ----------

/// 注册表运行键路径（当前用户）。
const AUTO_LAUNCH_REG_PATH: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
/// 注册表值名称（即本程序在 Run 键下的条目名）。
const AUTO_LAUNCH_VALUE: &str = "rastflow";

/// 从注册表读取当前是否已启用开机自启。
#[cfg(windows)]
pub fn auto_launch_is_enabled() -> bool {
    use windows::{
        Win32::System::Registry::{
            RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, KEY_READ,
        },
        core::PCWSTR,
    };

    let key_path: Vec<u16> = AUTO_LAUNCH_REG_PATH
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let value_name: Vec<u16> = AUTO_LAUNCH_VALUE
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut hkey = Default::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            None,
            KEY_READ,
            &mut hkey,
        )
        .is_err()
        {
            return false;
        }
        let result = RegQueryValueExW(
            hkey,
            PCWSTR(value_name.as_ptr()),
            None,
            None,
            None,
            None,
        );
        let _ = RegCloseKey(hkey);
        result.is_ok()
    }
}

/// 启用或禁用开机自启（写入/删除注册表 Run 键条目）。
#[cfg(windows)]
pub fn auto_launch_set(enable: bool) {
    use windows::{
        Win32::System::Registry::{
            RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW,
            HKEY_CURRENT_USER, KEY_WRITE, REG_SZ,
        },
        core::PCWSTR,
    };

    let key_path: Vec<u16> = AUTO_LAUNCH_REG_PATH
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let value_name: Vec<u16> = AUTO_LAUNCH_VALUE
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut hkey = Default::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            None,
            KEY_WRITE,
            &mut hkey,
        )
        .is_err()
        {
            eprintln!("auto_launch_set: 无法打开注册表键");
            return;
        }

        if enable {
            if let Ok(exe) = std::env::current_exe() {
                // 路径带引号，防止路径含空格时启动失败
                let quoted = format!("\"{}\"", exe.to_string_lossy());
                let data: Vec<u16> =
                    quoted.encode_utf16().chain(std::iter::once(0)).collect();
                let bytes = std::slice::from_raw_parts(
                    data.as_ptr() as *const u8,
                    data.len() * 2,
                );
                let result = RegSetValueExW(
                    hkey,
                    PCWSTR(value_name.as_ptr()),
                    None,
                    REG_SZ,
                    Some(bytes),
                );
                if result != windows::Win32::Foundation::NO_ERROR {
                    eprintln!("auto_launch_set: 写入注册表失败: {:?}", result);
                }
            }
        } else {
            // 删除条目；若不存在则忽略错误
            let _ = RegDeleteValueW(hkey, PCWSTR(value_name.as_ptr()));
        }

        let _ = RegCloseKey(hkey);
    }
}

#[cfg(not(windows))]
pub fn auto_launch_is_enabled() -> bool {
    false
}

#[cfg(not(windows))]
pub fn auto_launch_set(_enable: bool) {}
