#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(dead_code)]
#![cfg(windows)]

#[path = "../src/bindings.rs"]
mod bindings;

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    use bindings::*;

    unsafe {
        if let Some(exe_path) = find_everything_exe() {
            if let Err(err) = start_everything_silent(&exe_path) {
                eprintln!("Failed to start Everything silently: {err}");
            }
        } else {
            eprintln!("Everything.exe not found in common install locations or PATH.");
        }

        let db_loaded = wait_for_db_loaded(Duration::from_secs(3));
        println!("DB loaded: {db_loaded}");
        if !db_loaded {
            eprintln!("Everything database is not loaded. Wait for indexing before querying.");
        }

        println!(
            "Everything version: {}.{}.{}.{}",
            Everything_GetMajorVersion(),
            Everything_GetMinorVersion(),
            Everything_GetRevision(),
            Everything_GetBuildNumber()
        );

        // 写索引：请求 Everything 更新所有文件夹索引并保存数据库。
        // 这要求 Everything 正在运行；失败时可用 Everything_GetLastError 查看原因。
        // if Everything_UpdateAllFolderIndexes() == 0 {
        //     print_last_error("Everything_UpdateAllFolderIndexes");
        // }
        // if Everything_SaveDB() == 0 {
        //     print_last_error("Everything_SaveDB");
        // }

        // 读索引：从当前 Everything 索引中查询并打印最多 20 条结果。
        let query = wide("*");
        Everything_SetSearchW(query.as_ptr());
        Everything_SetMax(20);
        Everything_SetOffset(0);
        Everything_SetSort(EVERYTHING_SORT_DATE_RECENTLY_CHANGED_DESCENDING);
        Everything_SetRequestFlags(EVERYTHING_REQUEST_FULL_PATH_AND_FILE_NAME);

        if Everything_QueryW(1) == 0 {
            print_last_error("Everything_QueryW");
            Everything_CleanUp();
            return;
        }

        let count = Everything_GetNumResults();
        let total = Everything_GetTotResults();
        println!("Showing {count} of {total} indexed results:");

        for index in 0..count {
            let mut buffer = vec![0u16; 4096];
            let len =
                Everything_GetResultFullPathNameW(index, buffer.as_mut_ptr(), buffer.len() as u32)
                    as usize;
            let len = len.min(buffer.len());
            println!("{}", String::from_utf16_lossy(&buffer[..len]));
        }

        Everything_CleanUp();
    }
}

fn find_everything_exe() -> Option<PathBuf> {
    let candidates = [
        r"C:\Program Files\Everything\Everything.exe",
        r"C:\Program Files (x86)\Everything\Everything.exe",
        r"C:\Users\Public\Everything\Everything.exe",
    ];

    for candidate in candidates {
        let path = PathBuf::from(candidate);
        if path.exists() {
            return Some(path);
        }
    }

    let output = std::process::Command::new("where")
        .arg("Everything.exe")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(PathBuf::from)
}

fn start_everything_silent(exe_path: &PathBuf) -> std::io::Result<()> {
    std::process::Command::new(exe_path)
        .arg("-startup")
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    Ok(())
}

fn wait_for_db_loaded(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if unsafe { bindings::Everything_IsDBLoaded() != 0 } {
            return true;
        }
        thread::sleep(Duration::from_millis(250));
    }

    unsafe { bindings::Everything_IsDBLoaded() != 0 }
}

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn print_last_error(operation: &str) {
    let error = unsafe { bindings::Everything_GetLastError() };
    eprintln!(
        "{operation} failed: {error} ({})",
        everything_error_name(error)
    );
}

fn everything_error_name(error: u32) -> &'static str {
    match error {
        0 => "EVERYTHING_OK",
        1 => "EVERYTHING_ERROR_MEMORY",
        2 => "EVERYTHING_ERROR_IPC: Everything search client is not running",
        3 => "EVERYTHING_ERROR_REGISTERCLASSEX",
        4 => "EVERYTHING_ERROR_CREATEWINDOW",
        5 => "EVERYTHING_ERROR_CREATETHREAD",
        6 => "EVERYTHING_ERROR_INVALIDINDEX",
        7 => "EVERYTHING_ERROR_INVALIDCALL",
        8 => "EVERYTHING_ERROR_INVALIDREQUEST",
        9 => "EVERYTHING_ERROR_INVALIDPARAMETER",
        _ => "UNKNOWN",
    }
}
