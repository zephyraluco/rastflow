/// Everything 文件搜索集成模块
///
/// 检测 Everything 是否已安装，并直接启动 Everything GUI 来执行文件搜索。

use std::path::PathBuf;

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
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_LOCAL_MACHINE, KEY_READ,
        REG_SZ, REG_VALUE_TYPE,
    };
    use windows::core::PCWSTR;

    unsafe {
        let key_path: Vec<u16> = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\Everything.exe\0"
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

