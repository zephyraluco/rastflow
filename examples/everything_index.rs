#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(dead_code)]
#![cfg(windows)]

#[path = "../src/bindings.rs"]
mod bindings;

fn main() {
    use bindings::*;

    unsafe {
        println!(
            "Everything version: {}.{}.{}.{}",
            Everything_GetMajorVersion(),
            Everything_GetMinorVersion(),
            Everything_GetRevision(),
            Everything_GetBuildNumber()
        );

        let db_loaded = Everything_IsDBLoaded() != 0;
        println!("DB loaded: {db_loaded}");
        if !db_loaded {
            eprintln!("Everything database is not loaded. Start Everything and wait for indexing before querying.");
        }

        // 写索引：请求 Everything 更新所有文件夹索引并保存数据库。
        // 这要求 Everything 正在运行；失败时可用 Everything_GetLastError 查看原因。
        if Everything_UpdateAllFolderIndexes() == 0 {
            print_last_error("Everything_UpdateAllFolderIndexes");
        }
        if Everything_SaveDB() == 0 {
            print_last_error("Everything_SaveDB");
        }

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
