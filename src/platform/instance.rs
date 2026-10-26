//! 每个 Windows 会话只有一个桌面实例；重复启动只通知现有窗口。
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, LPARAM, WPARAM,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_APP};
use windows::core::{PCWSTR, w};

pub const ACTIVATE_MESSAGE: u32 = WM_APP + 2;
pub struct DesktopInstance(HANDLE);

impl DesktopInstance {
    pub fn acquire() -> windows::core::Result<Option<Self>> {
        Self::claim(w!("Local\\rastflow.desktop"))
    }

    fn claim(name: PCWSTR) -> windows::core::Result<Option<Self>> {
        let handle = unsafe { CreateMutexW(None, false, name) }?;
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            let _ = unsafe { CloseHandle(handle) };
            Ok(None)
        } else {
            Ok(Some(Self(handle)))
        }
    }
}
impl Drop for DesktopInstance {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

pub fn activate_existing() -> windows::core::Result<()> {
    // 首个实例可能仍在创建窗口，等待一个短且有界的初始化窗口。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if let Ok(window) = unsafe { FindWindowW(w!("rastflow_tray_window"), PCWSTR::null()) } {
            return unsafe { PostMessageW(Some(window), ACTIVATE_MESSAGE, WPARAM(0), LPARAM(0)) };
        }
        if std::time::Instant::now() >= deadline {
            return Err(windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(1460)));
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_instance_does_not_take_ownership_and_exit_releases_name() {
        let name: Vec<u16> = format!("Local\\rastflow-test-{}", std::process::id())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let first = DesktopInstance::claim(PCWSTR(name.as_ptr()))
            .unwrap()
            .unwrap();
        assert!(
            DesktopInstance::claim(PCWSTR(name.as_ptr()))
                .unwrap()
                .is_none()
        );
        drop(first);
        assert!(
            DesktopInstance::claim(PCWSTR(name.as_ptr()))
                .unwrap()
                .is_some()
        );
    }
}
