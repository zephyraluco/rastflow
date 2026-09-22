//! 系统托盘图标与右键菜单
//!
//! 直接用 Win32 实现（`Shell_NotifyIconW` + `TrackPopupMenu`），替代原先的
//! `tray-icon` crate。这样做换来两件事：
//!
//! 1. 少一棵依赖树（`tray-icon` 还连带 `muda` 菜单库、`dpi`）；
//! 2. 托盘图标直接取自**我们自己 exe 里已嵌入的图标资源** —— 那正是
//!    `build.rs` 为了 exe 自身图标而写进去的那份，于是三处图标（exe、
//!    安装器、托盘）只有一个来源，且由 Windows 从多尺寸 ICO 里挑最合适的一档
//!    （24px 这种非标准尺寸由系统缩放，比我们自己的盒式滤波更清楚）。
//!
//! 实现要点（都是踩过坑才会注意到的地方）：
//!
//! - **必须有窗口**：`Shell_NotifyIcon` 靠向某个窗口投递自定义消息来通知交互，
//!   所以要先建一个从不显示的顶层窗口，并用 `GWLP_USERDATA` 挂上上下文。
//! - **不要抢消息循环**：窗口建在 GPUI 的主线程上，由 GPUI 自己的
//!   `GetMessage/DispatchMessage` 循环把消息派发过来。因此 `WM_DESTROY` 里
//!   **绝对不能** `PostQuitMessage`，否则会把 GPUI 的循环一起结束掉。
//! - **Explorer 重启**：任务栏重建后会广播 `TaskbarCreated`，收到它必须重新
//!   登记图标，否则图标永久消失。
//! - **弹出菜单要先设前台窗口**：`TrackPopupMenu` 之前 `SetForegroundWindow`、
//!   之后 `PostMessageW(WM_NULL)`，否则点击菜单外面时菜单不会消失
//!   （微软文档对通知区域菜单专门说明的做法）。
//!
//! 交互结果通过 channel 交给主循环处理，而不是在窗口过程里直接操作窗口 ——
//! 窗口过程里拿不到 GPUI 的 `App`/`Context`。


/// 托盘交互产生的事件，由主循环消费。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// 左键单击图标
    LeftClick,
    /// 菜单：「显示 / 隐藏」
    ToggleWindow,
    /// 菜单：「退出」
    Quit,
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::sync::mpsc::{self, Receiver, Sender};

    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
        NOTIFY_ICON_MESSAGE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyIcon,
        DestroyWindow, GetCursorPos, GetWindowLongPtrW, HICON, HMENU, IMAGE_FLAGS, IMAGE_ICON,
        LoadImageW, MF_SEPARATOR, MF_STRING, PostMessageW, RegisterClassW, RegisterWindowMessageW,
        SetForegroundWindow, SetWindowLongPtrW, TrackPopupMenu, CREATESTRUCTW, GWLP_USERDATA,
        TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_DESTROY,
        WM_LBUTTONUP, WM_NCCREATE, WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_OVERLAPPED,
    };

    use super::TrayEvent;

    /// 托盘图标的标识。只有一枚图标，固定用 1 即可。
    const TRAY_ICON_ID: u32 = 1;

    /// 托盘把鼠标事件投递到这个自定义消息上。
    const TRAY_CALLBACK_MESSAGE: u32 = WM_APP + 1;

    /// exe 资源段里应用图标的资源 ID（见 `build.rs` 的 `winresource` 调用）。
    ///
    /// `winresource` 写出的 .rc 就是 `1 ICON "app-icon.ico"`，rc.exe 会把它展开成
    /// `RT_GROUP_ICON` + 若干 `RT_ICON`，ID 都是 1。
    const APP_ICON_RESOURCE_ID: usize = 1;

    /// 菜单项 id（`TPM_RETURNCMD` 会把选中的 id 作为 `TrackPopupMenu` 的返回值）。
    const MENU_TOGGLE: i32 = 1;
    const MENU_QUIT: i32 = 2;

    /// 托盘自身窗口的类名。用 `w!` 拿到 `'static` 的宽字符串字面量，
    /// 省掉自己维护缓冲区生命周期的麻烦。
    const TRAY_WINDOW_CLASS: PCWSTR = w!("rastflow_tray_window");

    /// 挂在窗口 `GWLP_USERDATA` 上的上下文。
    struct TrayContext {
        events: Sender<TrayEvent>,
        /// 当前登记的图标句柄，需自行销毁
        icon: HICON,
        /// `TaskbarCreated` 消息 id；explorer 重启后用它重新登记图标
        taskbar_created: u32,
        /// 提示文字（宽字符，含结尾 NUL），重新登记时要一并恢复
        tooltip: Vec<u16>,
    }

    /// 构建右键菜单。
    ///
    /// 单独抽出来是为了能测：真正弹出菜单要等用户右键，自动化测不到，
    /// 但「菜单能不能正确建出来」是可以测的。
    pub(super) fn build_menu() -> Option<HMENU> {
        let menu = unsafe { CreatePopupMenu() }.ok()?;

        let appended = unsafe {
            AppendMenuW(menu, MF_STRING, MENU_TOGGLE as usize, w!("显示 / 隐藏"))
                .and_then(|()| AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()))
                .and_then(|()| AppendMenuW(menu, MF_STRING, MENU_QUIT as usize, w!("退出")))
        };

        match appended {
            Ok(()) => Some(menu),
            Err(_) => {
                unsafe {
                    let _ = DestroyMenu(menu);
                }
                None
            }
        }
    }

    impl TrayContext {
        /// 登记 / 注销 / 更新托盘图标。
        fn shell_notify(&self, hwnd: HWND, message: NOTIFY_ICON_MESSAGE) -> bool {
            let mut data = NOTIFYICONDATAW {
                cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: TRAY_ICON_ID,
                uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
                uCallbackMessage: TRAY_CALLBACK_MESSAGE,
                hIcon: self.icon,
                ..Default::default()
            };

            let tip_length = self.tooltip.len().min(data.szTip.len());
            data.szTip[..tip_length].copy_from_slice(&self.tooltip[..tip_length]);

            (unsafe { Shell_NotifyIconW(message, &data) }).0 != 0
        }

        /// 在光标处弹出右键菜单，并把用户的选择发到 channel。
        ///
        /// 菜单每次现建现毁：只有两三项，创建成本可忽略，也省掉了维护
        /// `HMENU` 生命周期与「菜单变了要通知窗口过程」这类麻烦。
        unsafe fn show_menu(&self, hwnd: HWND) {
            let Some(menu) = build_menu() else {
                return;
            };

            unsafe {
                let mut cursor = POINT::default();
                let _ = GetCursorPos(&mut cursor);

                // 先把（不可见的）窗口设为前台窗口，菜单才会在点击别处时自动收起。
                // 这也是为什么这个窗口不能是 message-only 窗口。
                let _ = SetForegroundWindow(hwnd);

                let chosen = TrackPopupMenu(
                    menu,
                    // RETURNCMD：让选中项 id 直接作为返回值返回，
                    // 免得再去处理 WM_COMMAND。
                    TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_LEFTALIGN,
                    cursor.x,
                    cursor.y,
                    Some(0),
                    hwnd,
                    None,
                );

                // 微软建议在通知区域菜单之后补一个无害消息，让任务切换状态正确收尾
                let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
                let _ = DestroyMenu(menu);

                match chosen.0 {
                    MENU_TOGGLE => {
                        let _ = self.events.send(TrayEvent::ToggleWindow);
                    }
                    MENU_QUIT => {
                        let _ = self.events.send(TrayEvent::Quit);
                    }
                    // 0 = 用户点了菜单外面，取消
                    _ => {}
                }
            }
        }
    }

    impl Drop for TrayContext {
        fn drop(&mut self) {
            if !self.icon.0.is_null() {
                unsafe {
                    let _ = DestroyIcon(self.icon);
                }
            }
        }
    }

    /// 托盘窗口的窗口过程。
    ///
    /// 注意这里只能碰 Win32，不能碰 GPUI —— 拿不到 `App`/`Context`。
    /// 所以交互结果一律塞进 channel，交给主循环去执行。
    unsafe extern "system" fn tray_window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // WM_NCCREATE 是唯一能拿到 CreateWindowExW 那个 lpParam 的时机，
        // 在此把它挂到窗口上，后续消息再从 GWLP_USERDATA 取回。
        if message == WM_NCCREATE {
            let create = lparam.0 as *const CREATESTRUCTW;
            let context = unsafe { (*create).lpCreateParams } as *mut TrayContext;
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, context as isize) };
            // 返回 1 表示继续创建窗口
            return LRESULT(1);
        }

        let context = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const TrayContext;
        // 窗口已建好但上下文还没挂上（或已回收）时，老实交给默认处理
        let Some(context) = (unsafe { context.as_ref() }) else {
            return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        };

        // explorer 重启会把所有托盘图标清空，必须重新登记
        if context.taskbar_created != 0 && message == context.taskbar_created {
            context.shell_notify(hwnd, NIM_ADD);
            return LRESULT(0);
        }

        match message {
            TRAY_CALLBACK_MESSAGE => {
                // 这里用的是旧版回调协议（不调 NIM_SETVERSION）：
                // wParam 是图标 id，而鼠标消息在 lParam 里。
                // 新版协议会反过来（鼠标消息在 wParam、锚点坐标在 lParam），
                // 为了少一个可能悄悄失效的状态，这里跟随最经典、最稳的旧协议。
                match lparam.0 as u32 {
                    WM_LBUTTONUP => {
                        let _ = context.events.send(TrayEvent::LeftClick);
                    }
                    WM_RBUTTONUP => unsafe { context.show_menu(hwnd) },
                    _ => {}
                }
                LRESULT(0)
            }
            // ⚠️ 千万不要在这里 PostQuitMessage：这个窗口跑在 GPUI 的主线程上，
            // 消息循环是 GPUI 的，退出它等于把整个应用一起关掉。
            WM_DESTROY => LRESULT(0),
            _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
        }
    }

    /// 从自身 exe 的图标资源里加载 `size`×`size` 的应用图标。
    ///
    /// 之所以能这样做，是因为 `build.rs` 已经把 `assets/app-icon.ico`
    /// 通过 `winresource` 写进了 exe 的资源段 —— 于是托盘不必再自己解码 ICO，
    /// 也由 Windows 从多尺寸里挑最合适的一档。
    pub(super) fn load_app_icon(size: u32) -> Option<HICON> {
        let module = unsafe { GetModuleHandleW(None) }.ok()?;        // name 传资源 ID（低 16 位的「整数原子」），cx/cy 指定期望尺寸。
        // 不使用 LR_DEFAULTSIZE，因为我们要的是明确的托盘尺寸。
        let handle = unsafe {
            LoadImageW(
                Some(HINSTANCE(module.0)),
                PCWSTR(APP_ICON_RESOURCE_ID as *const u16),
                IMAGE_ICON,
                size as i32,
                size as i32,
                IMAGE_FLAGS(0),
            )
        }
        .ok()?;

        Some(HICON(handle.0))
    }

    /// 把提示文字转成含结尾 NUL 的宽字符。
    fn wide_tooltip(tooltip: &str) -> Vec<u16> {
        tooltip.encode_utf16().take(127).chain(Some(0)).collect()
    }

    /// 系统托盘图标。丢弃时自动从任务栏移除并销毁窗口与图标。
    pub struct TrayIcon {
        hwnd: HWND,
        context: *mut TrayContext,
        events: Receiver<TrayEvent>,
    }

    impl TrayIcon {
        /// 创建托盘图标。
        ///
        /// 必须在**拥有消息循环的线程**上调用（本项目就是 GPUI 主线程，
        /// 因为窗口过程要靠该线程的消息循环来派发），否则收不到任何交互事件。
        pub fn new(tooltip: &str, size: u32) -> Result<Self, String> {
            let icon = load_app_icon(size).ok_or_else(|| {
                "加载 exe 内的应用图标失败（图标资源是否已由 build.rs 嵌入？）".to_string()
            })?;

            let instance = unsafe { GetModuleHandleW(None) }
                .map_err(|err| format!("取模块句柄失败: {err}"))?;
            let instance = HINSTANCE(instance.0);

            let (events_tx, events_rx) = mpsc::channel();

            let context = Box::into_raw(Box::new(TrayContext {
                events: events_tx,
                icon,
                taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
                tooltip: wide_tooltip(tooltip),
            }));

            unsafe {
                // 类名重复注册只会返回 0 并置 last_error = ERROR_CLASS_ALREADY_EXISTS，
                // 无害；同进程内也只会走到一次。
                let window_class = WNDCLASSW {
                    lpfnWndProc: Some(tray_window_proc),
                    hInstance: instance,
                    lpszClassName: TRAY_WINDOW_CLASS,
                    ..Default::default()
                };
                RegisterClassW(&window_class);
            }

            // 建一个永不显示的顶层窗口，只作为托盘消息的落点。
            // WS_EX_TOOLWINDOW 保证它不出现在任务栏 / Alt-Tab；
            // WS_EX_NOACTIVATE 保证它不会抢走焦点。
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    TRAY_WINDOW_CLASS,
                    PCWSTR::null(),
                    WS_OVERLAPPED,
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    Some(instance),
                    Some(context as *const c_void),
                )
            };

            let hwnd = match hwnd {
                Ok(hwnd) => hwnd,
                Err(err) => {
                    // 窗口没建成，上下文还归我们管，必须回收，否则连带泄漏 HICON
                    drop(unsafe { Box::from_raw(context) });
                    return Err(format!("创建托盘窗口失败: {err}"));
                }
            };

            let tray = Self {
                hwnd,
                context,
                events: events_rx,
            };

            // 登记图标。explorer 尚未就绪时可能失败，此时留着窗口等 TaskbarCreated
            // 再补登记即可，所以这里不当作致命错误。
            if !unsafe { &*context }.shell_notify(hwnd, NIM_ADD) {
                eprintln!("[rastflow] 托盘图标登记失败，将在任务栏就绪后重试");
            }

            Ok(tray)
        }

        /// 取出一个待处理的交互事件（非阻塞）。
        pub fn try_recv(&self) -> Option<TrayEvent> {
            self.events.try_recv().ok()
        }
    }

    impl Drop for TrayIcon {
        fn drop(&mut self) {
            let context = unsafe { &*self.context };

            // 从任务栏移除，再销毁窗口。
            // 二者都必须在创建窗口的那个线程上调用，而本结构体只会存在于主线程。
            context.shell_notify(self.hwnd, NIM_DELETE);
            let _ = unsafe { DestroyWindow(self.hwnd) };

            // 收回上下文；其 Drop 会一并 DestroyIcon
            drop(unsafe { Box::from_raw(self.context) });
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::sync::mpsc::Receiver;

    use super::TrayEvent;

    /// 非 Windows 平台没有系统托盘，创建直接失败。
    pub struct TrayIcon {
        events: Receiver<TrayEvent>,
    }

    impl TrayIcon {
        pub fn new(_tooltip: &str, _size: u32) -> Result<Self, String> {
            Err("当前平台尚未实现系统托盘".to_string())
        }

        pub fn try_recv(&self) -> Option<TrayEvent> {
            self.events.try_recv().ok()
        }
    }
}

pub use imp::TrayIcon;

#[cfg(all(test, windows))]
mod tests {
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, DestroyMenu, GetMenuItemCount};

    use super::imp::{build_menu, load_app_icon, TrayIcon};

    /// 菜单能建出来、且三个条目（显示/隐藏、分隔线、退出）都在。
    #[test]
    fn menu_has_expected_entries() {
        let menu = build_menu().expect("构建右键菜单失败");

        let count = unsafe { GetMenuItemCount(Some(menu)) };
        assert_eq!(count, 3, "菜单应有 3 个条目（含分隔线），实际 {count}");

        unsafe {
            let _ = DestroyMenu(menu);
        }
    }

    /// 托盘图标的来源是本实现里最关键的一个假设：`build.rs` 把
    /// `assets/app-icon.ico` 写进了 exe 的资源段（ID 1），所以这里能用
    /// `LoadImageW` 直接从资源取图标。资源 ID 或嵌入方式一变，这条测试会先炸。
    #[test]
    fn app_icon_is_loadable_from_exe_resources() {
        for size in [16, 32, 48, 256] {
            let icon = load_app_icon(size)
                .unwrap_or_else(|| panic!("{size}px：未能从 exe 资源段加载应用图标"));
            unsafe {
                let _ = DestroyIcon(icon);
            }
        }
    }

    /// 端到端：建托盘窗口 + 登记图标到任务栏。
    ///
    /// 测试进程会短暂出现在通知区域，随后由 `Drop` 注销。这条测试能守住
    /// 「窗口能建起来」「`Shell_NotifyIcon(NIM_ADD)` 成功」这两件真正容易出错的事。
    #[test]
    fn tray_icon_registers_with_the_shell() {
        let tray = TrayIcon::new("rastflow 自检", 16);
        assert!(tray.is_ok(), "创建托盘图标失败：{:?}", tray.err());
    }

    /// 同一进程内重复创建 / 销毁不应出问题。
    ///
    /// 这条专门盯住两处容易写错的地方：窗口类重复注册（第二次 `RegisterClassW`
    /// 会失败但应被忽略），以及 `Box::from_raw` 是否恰好回收一次。
    #[test]
    fn tray_icon_can_be_recreated() {
        for _ in 0..3 {
            let tray = TrayIcon::new("rastflow 自检", 16);
            assert!(tray.is_ok(), "重复创建托盘图标失败：{:?}", tray.err());
        }
    }
}
