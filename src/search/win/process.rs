//! 进程辅助：判断当前进程是否提权、以管理员身份拉起索引子进程、以及子进程侧的
//! 「父进程一死就跟着退出」守卫。
//!
//! 索引子进程用的是**同一个可执行文件** + `--index-helper`，所以打包时不需要额外的
//! 二进制（cargo-packager 只收主二进制）。

use std::mem::size_of;

use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetProcessId, INFINITE, OpenProcess,
    PROCESS_ACCESS_RIGHTS, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOCLOSEPROCESS, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW,
    ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::PCWSTR;

use super::super::{CoreError, Result};
use super::pipe::win32_code;
use super::to_wide;

/// `SYNCHRONIZE`：等待进程退出的最小权限。
///
/// windows crate 把它归在 `FileSystem` 下（`FILE_ACCESS_RIGHTS`），但数值对所有
/// 内核对象通用，这里按 `PROCESS_ACCESS_RIGHTS` 用。
const SYNCHRONIZE: u32 = 0x0010_0000;

/// 当前进程 id
pub fn current_pid() -> u32 {
    unsafe { GetCurrentProcessId() }
}

/// 一个由我们拉起的、拥有独立生命周期的子进程。
///
/// `Drop` 会杀掉它：父进程无论怎么退出（包括崩溃），都不能留下一个提权的孤儿。
pub struct ElevatedChild {
    process: HANDLE,
    pid: u32,
}

unsafe impl Send for ElevatedChild {}
unsafe impl Sync for ElevatedChild {}

impl ElevatedChild {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn kill(&self) {
        unsafe {
            let _ = TerminateProcess(self.process, 1);
        }
    }
}

impl Drop for ElevatedChild {
    fn drop(&mut self) {
        self.kill();
        unsafe {
            let _ = CloseHandle(self.process);
        }
    }
}

/// 以管理员身份启动 `current_exe`，命令行参数为 `parameters`。
///
/// `parameters` 里含空格的值需要调用方自己加引号（ShellExecute 收的是整条命令行）。
pub fn spawn_elevated(parameters: &str) -> Result<ElevatedChild> {
    let exe = std::env::current_exe().map_err(CoreError::Io)?;
    let verb = to_wide("runas");
    let file = to_wide(&exe.to_string_lossy());
    let params = to_wide(parameters);

    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        // NOCLOSEPROCESS：要拿到进程句柄；NOASYNC：返回时操作已完成；
        // FLAG_NO_UI：自己给错误提示，不要弹系统对话框
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    match unsafe { ShellExecuteExW(&mut info) } {
        Ok(()) => {}
        // 用户在 UAC 提示上点了「否」
        Err(err) if win32_code(&err) == ERROR_CANCELLED.0 => {
            return Err(CoreError::ElevationDenied);
        }
        Err(err) => return Err(CoreError::win32("ShellExecuteExW(runas)", err)),
    }

    if info.hProcess.is_invalid() {
        return Err(CoreError::Helper(
            "提权子进程没有返回进程句柄".to_string(),
        ));
    }
    let pid = unsafe { GetProcessId(info.hProcess) };
    if pid == 0 {
        let _ = unsafe { CloseHandle(info.hProcess) };
        return Err(CoreError::Helper("取不到提权子进程的 pid".to_string()));
    }
    Ok(ElevatedChild {
        process: info.hProcess,
        pid,
    })
}

/// 子进程侧的守卫：父进程退出后立刻自杀。
///
/// 子进程平时阻塞在管道读取或 USN 读取上，**没有**机会发现父进程已经不在了；
/// 一条只做等待的线程是最省事的办法，也能保证不会留下提权孤儿进程。
pub fn exit_with_parent(parent_pid: u32) {
    let _ = std::thread::Builder::new()
        .name("rastflow-helper-guard".to_string())
        .spawn(move || {
            // 提权进程对普通用户的父进程拿 SYNCHRONIZE 是允许的
            let Ok(parent) = (unsafe {
                OpenProcess(PROCESS_ACCESS_RIGHTS(SYNCHRONIZE), false, parent_pid)
            }) else {
                // 拿不到句柄就退化成「靠管道断开发现」，不影响正确性
                return;
            };
            let waited = unsafe { WaitForSingleObject(parent, INFINITE) };
            let _ = unsafe { CloseHandle(parent) };
            if waited == WAIT_OBJECT_0 {
                unsafe {
                    let _ = TerminateProcess(GetCurrentProcess(), 0);
                }
            }
        });
}
