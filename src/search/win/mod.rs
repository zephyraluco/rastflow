//! Win32 薄封装层，**`unsafe` 只允许出现在这里**。
//!
//! | 子模块 | 职责 |
//! | --- | --- |
//! | [`volume`] | 卷句柄 `\\.\X:` 与 USN / MFT 的 IOCTL |
//! | [`usn`] | USN 记录格式与 FRN 编码（纯字节解析，不调 API） |
//! | [`disk`] | 本地固定盘 / NTFS 判断 |
//! | [`known_folder`] | `SHGetKnownFolderPath`：开始菜单、桌面 |
//!
//! 本文件放宽字符串转换、线程 id、取消同步 IO。

pub mod disk;
pub mod known_folder;
pub mod usn;
pub mod volume;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::IO::CancelSynchronousIo;
use windows::Win32::System::Threading::{GetCurrentThreadId, OpenThread, THREAD_TERMINATE};

use super::{CoreError, Result};

/// `&str` → 以 NUL 结尾的 UTF-16
pub(crate) fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `&[u16]`（可能带结尾 NUL）→ `String`
pub(crate) fn from_wide_lossy(value: &[u16]) -> String {
    let end = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

/// 当前线程 id
pub fn current_thread_id() -> u32 {
    unsafe { GetCurrentThreadId() }
}

/// 中断指定线程上正在进行的**同步** IO。
///
/// 监控线程阻塞在 `DeviceIoControl(FSCTL_READ_USN_JOURNAL)` 上，用标志位叫不醒它，
/// 必须先取消这次 IO。
pub fn cancel_synchronous_io(thread_id: u32) -> Result<()> {
    unsafe {
        let handle = OpenThread(THREAD_TERMINATE, false, thread_id)
            .map_err(|e| CoreError::win32("OpenThread", e))?;
        let result = CancelSynchronousIo(handle).map_err(|e| {
            // ERROR_NOT_FOUND(1168) 说明该线程当前没有挂起的同步 IO，属正常情况
            CoreError::win32("CancelSynchronousIo", e)
        });
        let _ = CloseHandle(handle);
        result
    }
}
