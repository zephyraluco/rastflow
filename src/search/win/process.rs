//! 进程辅助：判断当前进程是否提权、以管理员身份拉起索引子进程、以及子进程侧的
//! 「父进程一死就跟着退出」守卫。
//!
//! 索引子进程用的是**同一个可执行文件** + `--index-helper`，所以打包时不需要额外的
//! 二进制（cargo-packager 只收主二进制）。

use std::mem::size_of;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_CANCELLED, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::Security::{
    GetTokenInformation, IsWellKnownSid, TOKEN_QUERY, TOKEN_USER, TokenUser, WinLocalSystemSid,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetExitCodeProcess, GetProcessId, INFINITE,
    OpenProcess, OpenProcessToken, PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION,
    TerminateProcess, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOCLOSEPROCESS, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW,
    ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
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

/// 这个进程是否可信地“以更高权限运行”（典型就是索引服务的 `LocalSystem`）。
///
/// 用来识别「连上的管道对面到底是不是我们装的索引服务」。判定分两种情况：
///
/// - **令牌读得到**：严格要求 `TokenUser` 是 `WinLocalSystemSid`。
/// - **令牌读不到**：普通用户打不开 `LocalSystem` 进程的句柄与令牌（`ACCESS_DENIED`），
///   而普通用户自己的进程一定能打开。所以“拒绝访问”本身就已经证明对方权限高于本进程。
///
/// 第二类是必需的：如果把它当成失败，主进程就会误判服务不可用而回退到 `runas`，
/// 导致每次启动都弹 UAC。已经拿到管理员权限的进程本来就能做更多，不在威胁模型内。
pub fn is_trusted_server_process(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe {
        let process = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(process) => process,
            // 打不开只可能是权限不足（进程已退出会是 INVALID_PARAMETER）
            Err(err) => return win32_code(&err) == ERROR_ACCESS_DENIED.0,
        };
        let mut token = HANDLE::default();
        let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token).is_ok();
        let _ = CloseHandle(process);
        if !opened {
            // 同上：句柄已经打开，令牌却拿不到，只能是对方权限更高
            return true;
        }

        // 先问需要多大（TokenUser 的数据长度取决于 SID 长度）
        let mut needed = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        if needed == 0 {
            let _ = CloseHandle(token);
            return false;
        }
        let mut buffer = vec![0u8; needed as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr() as *mut core::ffi::c_void),
            needed,
            &mut needed,
        )
        .is_ok();
        let _ = CloseHandle(token);
        if !ok {
            return false;
        }

        // `TokenUser` 返回的是 TOKEN_USER，首字段是 SID_AND_ATTRIBUTES
        let user = buffer.as_ptr() as *const TOKEN_USER;
        let sid = (*user).User.Sid;
        !sid.is_invalid() && IsWellKnownSid(sid, WinLocalSystemSid).as_bool()
    }
}
