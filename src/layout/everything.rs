/// Everything 文件搜索集成模块
///
/// 检测 Everything 是否已安装，并通过 Everything SDK 执行文件搜索。
use std::path::PathBuf;
use std::process::Command;

use crate::bindings::*;

const SEARCH_RESULT_LIMIT: u32 = 200;

/// Everything 的检测状态
#[derive(Clone, Debug, PartialEq)]
pub enum EverythingStatus {
    /// 尚未检测
    Unknown,
    /// 未安装 Everything
    NotInstalled,
    /// 已安装 Everything，但当前用户下没有索引数据库
    NotIndexed { exe_path: PathBuf },
    /// 已安装 Everything，且当前用户下已有索引数据库
    Indexed { exe_path: PathBuf },
}

#[derive(Clone, Debug)]
pub struct EverythingSearchError {
    pub code: u32,
}

#[derive(Clone, Debug)]
pub struct EverythingResult {
    pub name: String,
    pub path: String,
    pub size: String,
    pub modified: String,
}

impl EverythingResult {
    pub fn full_path(&self) -> PathBuf {
        PathBuf::from(&self.path).join(&self.name)
    }
}

/// 在常见路径和注册表中搜索 Everything.exe
pub fn find_everything_exe() -> Option<PathBuf> {
    let candidates = [
        r"C:\Program Files\Everything\Everything.exe",
        r"C:\Program Files (x86)\Everything\Everything.exe",
        r"C:\Users\Public\Everything\Everything.exe",
    ];
    for path in &candidates {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }

    #[cfg(windows)]
    {
        if let Some(p) = find_everything_in_registry() {
            return Some(p);
        }
    }

    // 最后尝试 PATH
    #[cfg(windows)]
    if let Ok(output) = {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        std::process::Command::new("where")
            .arg("Everything.exe")
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    } {
        if output.status.success() {
            let s = String::from_utf8_lossy(&output.stdout);
            let line = s.lines().next().unwrap_or("").trim().to_string();
            if !line.is_empty() {
                return Some(PathBuf::from(line));
            }
        }
    }
    #[cfg(not(windows))]
    if let Ok(output) = std::process::Command::new("which")
        .arg("everything")
        .output()
    {
        if output.status.success() {
            let s = String::from_utf8_lossy(&output.stdout);
            let line = s.lines().next().unwrap_or("").trim().to_string();
            if !line.is_empty() {
                return Some(PathBuf::from(line));
            }
        }
    }

    None
}

/// 在注册表 App Paths 中查找 Everything.exe
#[cfg(windows)]
fn find_everything_in_registry() -> Option<PathBuf> {
    use windows::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegOpenKeyExW,
        RegQueryValueExW,
    };
    use windows::core::PCWSTR;

    unsafe {
        let key_path: Vec<u16> =
            "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\Everything.exe\0"
                .encode_utf16()
                .collect();
        let mut hkey = windows::Win32::System::Registry::HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key_path.as_ptr()),
            None,
            KEY_READ,
            &mut hkey,
        );
        if result.is_err() {
            return None;
        }

        let value_name: Vec<u16> = "\0".encode_utf16().collect(); // default value
        let mut buf = vec![0u16; 512];
        let mut buf_len = (buf.len() * 2) as u32;
        let mut reg_type = REG_VALUE_TYPE::default();

        let query_result = RegQueryValueExW(
            hkey,
            PCWSTR(value_name.as_ptr()),
            None,
            Some(&mut reg_type),
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut buf_len),
        );

        let _ = RegCloseKey(hkey);

        if query_result.is_ok() && reg_type == REG_SZ {
            let len = (buf_len / 2) as usize;
            let path = String::from_utf16_lossy(&buf[..len.saturating_sub(1)]);
            let p = PathBuf::from(path.trim());
            if p.exists() {
                return Some(p);
            }
        }
        None
    }
}

/// 检测 Everything 安装状态（同步，在后台线程调用）
pub fn detect() -> EverythingStatus {
    match find_everything_exe() {
        None => EverythingStatus::NotInstalled,
        Some(exe_path) if everything_db_exists() => EverythingStatus::Indexed { exe_path },
        Some(exe_path) => EverythingStatus::NotIndexed { exe_path },
    }
}

/// 检查当前用户 Everything 索引数据库是否存在。
fn everything_db_exists() -> bool {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|path| path.join("Everything").join("Everything.db"))
        .is_some_and(|path| path.exists())
}

/// 用 Everything.exe 打开 GUI 并预填查询词
pub fn open_everything_gui(exe_path: &PathBuf, query: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let _ = std::process::Command::new(exe_path)
            .args(["-search", query])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new(exe_path)
            .args(["-search", query])
            .spawn();
    }
}

#[cfg(windows)]
pub fn open_result(result: &EverythingResult) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    Command::new("cmd")
        .args(["/C", "start", ""])
        .arg(result.full_path())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
}

#[cfg(not(windows))]
pub fn open_result(_result: &EverythingResult) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "当前平台尚未实现文件打开",
    ))
}

/// 按 Everything SDK 示例方式搜索，并请求列表展示需要的字段。
pub fn search(query: &str) -> Result<Vec<EverythingResult>, EverythingSearchError> {
    let query = wide(query);

    unsafe {
        Everything_SetSearchW(query.as_ptr());
        Everything_SetRequestFlags(
            EVERYTHING_REQUEST_FILE_NAME
                | EVERYTHING_REQUEST_PATH
                | EVERYTHING_REQUEST_SIZE
                | EVERYTHING_REQUEST_DATE_MODIFIED,
        );
        Everything_SetSort(EVERYTHING_SORT_NAME_ASCENDING);
        Everything_SetOffset(0);
        Everything_SetMax(SEARCH_RESULT_LIMIT);

        if Everything_QueryW(1) == 0 {
            return Err(EverythingSearchError {
                code: Everything_GetLastError(),
            });
        }

        let count = Everything_GetNumResults();
        let mut results = Vec::with_capacity(count as usize);
        for index in 0..count {
            let name = wide_ptr_to_string(Everything_GetResultFileNameW(index));
            let path = wide_ptr_to_string(Everything_GetResultPathW(index));
            results.push(EverythingResult {
                name,
                path,
                size: result_size(index),
                modified: result_modified(index),
            });
        }
        Everything_CleanUp();
        Ok(results)
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn wide_ptr_to_string(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }

    let mut len = 0;
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }

    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr, len) })
}

unsafe fn result_size(index: u32) -> String {
    let mut size = LARGE_INTEGER { QuadPart: 0 };
    if unsafe { Everything_GetResultSize(index, &mut size) } == 0 {
        return "-".to_string();
    }

    format_size(unsafe { size.QuadPart }.max(0) as u64)
}

unsafe fn result_modified(index: u32) -> String {
    let mut filetime = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    if unsafe { Everything_GetResultDateModified(index, &mut filetime) } == 0 {
        return "-".to_string();
    }

    let ticks = ((filetime.dwHighDateTime as u64) << 32) | filetime.dwLowDateTime as u64;
    format_filetime(ticks)
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

fn format_filetime(ticks: u64) -> String {
    const WINDOWS_TO_UNIX_EPOCH_SECONDS: u64 = 11_644_473_600;
    let seconds = ticks / 10_000_000;
    if seconds < WINDOWS_TO_UNIX_EPOCH_SECONDS {
        return "-".to_string();
    }

    format_unix_time(seconds - WINDOWS_TO_UNIX_EPOCH_SECONDS)
}

fn format_unix_time(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (y + i64::from(m <= 2), m as u32, d as u32)
}
