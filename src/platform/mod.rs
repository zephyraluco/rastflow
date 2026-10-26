//! 桌面侧 Windows 集成；索引服务不初始化此层。
pub mod instance;

/// 交给 Windows Shell 打开目标，不经过 cmd，保留快捷方式及关联程序语义。
pub fn open_target(target: &str) -> std::io::Result<()> {
    use windows::Win32::UI::Shell::{SEE_MASK_FLAG_NO_UI, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;
    if target.is_empty() || target.contains('\0') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "启动目标为空或包含 NUL",
        ));
    }
    let file: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
    let info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_FLAG_NO_UI,
        lpFile: PCWSTR(file.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    let mut info = info;
    unsafe { ShellExecuteExW(&mut info) }
        .map_err(|error| std::io::Error::from_raw_os_error((error.code().0 as u32 & 0xffff) as i32))
}
