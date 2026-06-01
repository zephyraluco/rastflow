/// Everything 文件搜索集成模块
///
/// 检测 Everything 是否已安装，并通过 es.exe（命令行工具）或直接启动
/// Everything GUI 来执行文件搜索。

use std::path::PathBuf;

/// Everything 的检测状态
#[derive(Clone, Debug, PartialEq)]
pub enum EverythingStatus {
    /// 尚未检测
    Unknown,
    /// 未安装 Everything
    NotInstalled,
    /// 已安装 Everything，但未找到 es.exe（仅能跳转到 Everything GUI）
    InstalledOnly { exe_path: PathBuf },
    /// 已安装 Everything 且找到 es.exe（可在窗口内显示搜索结果）
    ReadyWithEs { exe_path: PathBuf, es_path: PathBuf },
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

/// 在 Everything 安装目录和 PATH 中搜索 es.exe
pub fn find_es_exe(everything_path: &PathBuf) -> Option<PathBuf> {
    // 同目录下查找
    if let Some(dir) = everything_path.parent() {
        let es = dir.join("es.exe");
        if es.exists() {
            return Some(es);
        }
    }
    // PATH 中查找
    #[cfg(windows)]
    if let Ok(output) = {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        std::process::Command::new("where")
            .arg("es.exe")
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
    None
}

/// 检测 Everything 安装状态（同步，在后台线程调用）
pub fn detect() -> EverythingStatus {
    match find_everything_exe() {
        None => EverythingStatus::NotInstalled,
        Some(exe_path) => {
            match find_es_exe(&exe_path) {
                Some(es_path) => EverythingStatus::ReadyWithEs { exe_path, es_path },
                None => EverythingStatus::InstalledOnly { exe_path },
            }
        }
    }
}

/// 用 es.exe 搜索文件，返回路径列表（同步，在后台线程调用）
pub fn search_with_es(es_path: &PathBuf, query: &str, max: usize) -> Vec<String> {
    if query.trim().is_empty() {
        return vec![];
    }
    #[cfg(windows)]
    let output = {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        std::process::Command::new(es_path)
            .args(["-n", &max.to_string(), "-sort", "date-recently-changed", query])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    };
    #[cfg(not(windows))]
    let output = std::process::Command::new(es_path)
        .args(["-n", &max.to_string(), "-sort", "date-recently-changed", query])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            text.lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        }
        _ => vec![],
    }
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

/// 用系统默认方式打开文件或文件夹
pub fn open_path(path: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let _ = std::process::Command::new("explorer")
            .arg(path)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}
