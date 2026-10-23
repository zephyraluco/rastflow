//! Win32 薄封装层，**`unsafe` 只允许出现在这里**。
//!
//! `volume` 管卷句柄与 USN / MFT 的 IOCTL，`usn` 解析 USN 记录与 FRN（纯字节，不调 API），
//! `disk` 判断盘类型，`known_folder` 取已知文件夹。
//! `pipe` 是与提权索引子进程之间的命名管道，`process` 管提权与子进程生命周期。
//!
//! 本文件放宽字符串转换、线程 id、取消同步 IO、路径存在性判断。

pub mod disk;
pub mod known_folder;
pub mod pipe;
pub mod process;
pub mod usn;
pub mod volume;

use windows::Win32::Foundation::{
    GetLastError, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND,
};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
use windows::Win32::System::IO::CancelSynchronousIo;
use windows::Win32::System::Threading::{GetCurrentThreadId, OpenThread, THREAD_TERMINATE};
use windows::core::PCWSTR;

use super::{CoreError, Result};

/// `MAX_PATH`：不带 `\\?\` 前缀时 Win32 路径长度的上限（UTF-16 单元）
const MAX_PATH_UTF16: usize = 260;

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

/// 路径是否仍然存在。
///
/// 返回 `None` 表示**判断不了**（权限不足、共享冲突等）：调用方不能据此删索引条目。
///
/// 不用 `Path::exists()`：它把「打不开」一律当成「不存在」，于是
/// 1. 长路径（≥260 个 UTF-16 单元）在不带 `\\?\` 前缀时会被当成找不到；
/// 2. `ERROR_ACCESS_DENIED`、`ERROR_SHARING_VIOLATION` 也返回 false。
///
/// 搜索时的懒删除如果照它删条目，这些文件会**永久**从索引里消失。
pub fn path_exists(path: &str) -> Option<bool> {
    // 长路径报的也是 PATH_NOT_FOUND：加逐字前缀再确认一次
    if !path.starts_with(r"\\?") && path.encode_utf16().count() >= MAX_PATH_UTF16 {
        if raw_exists(path) == Some(true) {
            return Some(true);
        }
        return raw_exists(&format!(r"\\?\{path}"));
    }
    raw_exists(path)
}

/// 直接问 Win32：`Some(true)` 存在、`Some(false)` 明确不存在、`None` 判断不了
fn raw_exists(path: &str) -> Option<bool> {
    let wide = to_wide(path);
    if unsafe { GetFileAttributesW(PCWSTR(wide.as_ptr())) } != INVALID_FILE_ATTRIBUTES {
        return Some(true);
    }
    match unsafe { GetLastError() } {
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Some(false),
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rastflow-win-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn path_exists_tells_found_from_missing() {
        let dir = temp_dir("exists");
        let file = dir.join("exists.txt");
        std::fs::write(&file, b"x").unwrap();

        assert_eq!(path_exists(&file.to_string_lossy()), Some(true));
        assert_eq!(
            path_exists(&dir.join("missing.txt").to_string_lossy()),
            Some(false)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_exists_follows_long_paths_instead_of_reporting_missing() {
        // 长路径不带 `\\?\` 前缀时 Win32 会报 PATH_NOT_FOUND —— 如果照单全收，
        // 搜索时的懒删除会把这类文件永久摘掉。
        let mut deep = temp_dir("long");
        while deep.to_string_lossy().len() < 300 {
            deep.push("rastflow-long-path-segment");
        }
        let verbatim_dir = format!(r"\\?\{}", deep.display());
        std::fs::create_dir_all(&verbatim_dir).unwrap();
        let file = deep.join("inside.txt");
        std::fs::write(format!(r"\\?\{}", file.display()), b"x").unwrap();

        let plain = file.to_string_lossy().to_string();
        assert!(plain.len() >= 260, "用例本身要足够长");
        assert_eq!(path_exists(&plain), Some(true), "长路径不能当成不存在");

        let _ = std::fs::remove_dir_all(&verbatim_dir);
    }
}
