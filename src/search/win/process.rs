//! 服务安装维护的提权执行与命名管道服务端身份检查。

use std::mem::size_of;

use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetExitCodeProcess, GetProcessId, INFINITE, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::PCWSTR;

use super::super::{CoreError, Result};
use super::pipe::win32_code;
use super::to_wide;

/// 当前进程 id
pub fn current_pid() -> u32 {
    unsafe { GetCurrentProcessId() }
}

/// 一个由我们拉起的、拥有独立生命周期的子进程。
///
/// `Drop` 会杀掉它：父进程无论怎么退出（包括崩溃），都不能留下一个提权的孤儿。
pub struct ElevatedChild {
    process: HANDLE,
}

impl ElevatedChild {
    /// 等它退出，返回退出码
    pub fn wait(&self) -> Option<u32> {
        let waited = unsafe { WaitForSingleObject(self.process, INFINITE) };
        if waited != WAIT_OBJECT_0 {
            return None;
        }
        let mut code = 0u32;
        unsafe { GetExitCodeProcess(self.process, &mut code) }
            .ok()
            .map(|()| code)
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
        nShow: SW_HIDE.0,
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
        return Err(CoreError::Helper("提权子进程没有返回进程句柄".to_string()));
    }
    let pid = unsafe { GetProcessId(info.hProcess) };
    if pid == 0 {
        let _ = unsafe { CloseHandle(info.hProcess) };
        return Err(CoreError::Helper("取不到提权子进程的 pid".to_string()));
    }
    Ok(ElevatedChild {
        process: info.hProcess,
    })
}

/// 提权执行一次 `current_exe <flag>` 并等它结束，退出码非 0 视为失败。
///
/// 服务安装/卸载走这条路径：它必须**同步**完成，否则调用方接着查服务状态会
/// 得到「还没装上」的错觉。
pub fn run_elevated_and_wait(flag: &str) -> Result<()> {
    let child = spawn_elevated(flag)?;
    match child.wait() {
        Some(0) => Ok(()),
        Some(code) => Err(CoreError::Helper(format!(
            "以管理员身份执行 {flag} 失败（退出码 {code}）"
        ))),
        None => Err(CoreError::Helper(format!("等待 {flag} 结束失败"))),
    }
}

/// 服务只接受从同一安装位置运行的客户端。此检查不等同于代码签名验证。
pub fn is_desktop_client(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let Ok(expected) = std::env::current_exe() else {
        return false;
    };
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut buffer = vec![0u16; 32768];
        let mut length = buffer.len() as u32;
        let result = windows::Win32::System::Threading::QueryFullProcessImageNameW(
            process,
            windows::Win32::System::Threading::PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(process);
        result.is_ok()
            && String::from_utf16_lossy(&buffer[..length as usize]).to_lowercase()
                == expected.to_string_lossy().to_lowercase()
    }
}
