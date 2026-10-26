//! Windows 服务：把索引子进程做成常驻服务，**彻底免掉每次启动的 UAC**。
//!
//! 和 Everything 的 `Everything.exe -svc` 是同一个思路：同一个可执行文件，
//! 换一个命令行开关就变成服务；服务以 `LocalSystem` 运行，打开卷时天生具备
//! `GENERIC_WRITE`，普通用户的 GUI 端因此不再需要提权。
//!
//! 三种身份（由 `main.rs` 分派）：
//!
//! | 开关 | 谁启动 | 干什么 |
//! | --- | --- | --- |
//! | `--index-service` | SCM（服务控制管理器） | 跑 dispatcher，循环接受管道连接 |
//! | `--install-service` / `--uninstall-service` | 用户点按钮（提权一次） | 注册/删除服务 |
//!
//! ⚠️ 服务与 GUI 是**两个进程**，会话期间才建立管道连接。服务空闲时不读任何卷、
//! 不持有索引，内存占用与 Everything 的服务一样在 1 MB 量级。

use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::{
    ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_EXISTS,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Services::{
    CloseServiceHandle, ControlService, CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW,
    QueryServiceStatusEx, RegisterServiceCtrlHandlerExW, SC_HANDLE, SC_MANAGER_ALL_ACCESS,
    SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO, SERVICE_ACCEPT_STOP, SERVICE_ALL_ACCESS,
    SERVICE_AUTO_START, SERVICE_CONTROL_STOP, SERVICE_ERROR_NORMAL, SERVICE_QUERY_STATUS,
    SERVICE_RUNNING, SERVICE_START, SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_HANDLE,
    SERVICE_STATUS_PROCESS, SERVICE_STOP, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW, StartServiceW,
};
use windows::core::{PCWSTR, PWSTR};

use super::super::{CoreError, Result};
use super::pipe::win32_code;
use super::to_wide;

/// 服务名（SCM 里的键名，也是 `net start` 用的名字）
pub const SERVICE_NAME: &str = "rastflow-index";
/// 服务在「服务」管理器里显示的名字
const SERVICE_DISPLAY_NAME: &str = "rastflow 文件索引";

/// 命令行开关
pub const SERVICE_FLAG: &str = "--index-service";
pub const INSTALL_FLAG: &str = "--install-service";
pub const UNINSTALL_FLAG: &str = "--uninstall-service";

/// 服务看到的管道名。固定名字，服务是长期存在的服务端。
///
/// 固定名意味着「先占坑」是可能的，所以客户端连上之后必须校验服务端进程是
/// `LocalSystem`（见 [`super::process::is_trusted_server_process`]）。
pub const PIPE_NAME: &str = r"\\.\pipe\rastflow-index-service";

/// 升级/重装时等旧服务停下来的上限。
///
/// 服务可能正在跑一次全量 MFT 扫描（几十秒），控制命令要等扫描结束才生效。
/// 停不下来也继续改配置 —— 下次开机会用新二进制。
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// 卸载时等旧服务停下来的上限。要留足「正在扫描」的时间，否则删不掉 exe。
const UNINSTALL_STOP_TIMEOUT: Duration = Duration::from_secs(60);

/// 服务的对外状态
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceState {
    /// 没有安装
    NotInstalled,
    /// 已安装但没有运行
    Stopped,
    /// 正在启动
    Starting,
    /// 正在运行
    Running,
    /// 装不了/查不了（权限不足等），附原因
    Unknown,
}

impl ServiceState {
    pub fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }
}

// ---------- SCM 操作 ----------

fn open_manager(access: u32) -> Result<SC_HANDLE> {
    unsafe {
        OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), access)
            .map_err(|err| CoreError::win32("OpenSCManagerW", err))
    }
}

fn open_service(manager: SC_HANDLE, access: u32) -> Result<SC_HANDLE> {
    let name = to_wide(SERVICE_NAME);
    unsafe {
        OpenServiceW(manager, PCWSTR(name.as_ptr()), access)
            .map_err(|err| CoreError::win32("OpenServiceW", err))
    }
}

/// 查询服务当前状态。**不需要管理员**（`SC_MANAGER_CONNECT` + `SERVICE_QUERY_STATUS`）。
pub fn state() -> ServiceState {
    let Ok(manager) = open_manager(SC_MANAGER_CONNECT) else {
        return ServiceState::Unknown;
    };
    let service = open_service(manager, SERVICE_QUERY_STATUS);
    let _ = unsafe { CloseServiceHandle(manager) };
    let service = match service {
        Ok(handle) => handle,
        // 服务不存在是正常情况（没装），不是错误
        Err(CoreError::Win32 { err, .. }) if win32_code(&err) == ERROR_SERVICE_DOES_NOT_EXIST.0 => {
            return ServiceState::NotInstalled;
        }
        Err(_) => return ServiceState::Unknown,
    };

    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut needed = 0u32;
    let query = unsafe {
        QueryServiceStatusEx(
            service,
            SC_STATUS_PROCESS_INFO,
            Some(status_bytes(&mut status)),
            &mut needed,
        )
    };
    let _ = unsafe { CloseServiceHandle(service) };
    if query.is_err() {
        return ServiceState::Unknown;
    }

    match status.dwCurrentState {
        value if value == SERVICE_RUNNING => ServiceState::Running,
        value if value == SERVICE_START_PENDING => ServiceState::Starting,
        value if value == SERVICE_STOPPED => ServiceState::Stopped,
        _ => ServiceState::Stopped,
    }
}

fn status_bytes(status: &mut SERVICE_STATUS_PROCESS) -> &mut [u8] {
    unsafe {
        std::slice::from_raw_parts_mut(
            status as *mut SERVICE_STATUS_PROCESS as *mut u8,
            size_of::<SERVICE_STATUS_PROCESS>(),
        )
    }
}

/// 注册服务（需要管理员）。已存在就改成当前的 exe 路径，便于升级后指到新位置。
///
/// ⚠️ **先停再改再起**：升级时旧服务还在跑旧版本的 exe，而那个文件已经被安装器
/// 替换（或删除）。只改配置不会让 SCM 换掉正在运行的进程，用户会一直用着旧代码。
pub fn install() -> Result<()> {
    let exe = std::env::current_exe().map_err(CoreError::Io)?;
    // 命令行要整体加引号：路径里几乎一定有空格（Program Files）
    let command = format!("\"{}\" {SERVICE_FLAG}", exe.display());
    let manager = open_manager(SC_MANAGER_ALL_ACCESS)?;

    let name = to_wide(SERVICE_NAME);
    let display = to_wide(SERVICE_DISPLAY_NAME);
    let binary = to_wide(&command);

    let created = unsafe {
        CreateServiceW(
            manager,
            PCWSTR(name.as_ptr()),
            PCWSTR(display.as_ptr()),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR(binary.as_ptr()),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            // 空 = LocalSystem，不需要密码
            PCWSTR::null(),
            PCWSTR::null(),
        )
    };

    match created {
        Ok(service) => {
            let _ = unsafe { CloseServiceHandle(service) };
        }
        Err(err) if win32_code(&err) == ERROR_SERVICE_EXISTS.0 => {
            // 已安装：先停下来，否则 `ChangeServiceConfigW` 只改注册表、不换进程
            if let Err(err) = stop(STOP_TIMEOUT) {
                eprintln!("[rastflow] 停止旧索引服务失败（继续改配置）：{err}");
            }
            // 停过一次之后句柄要重新取：SCM 可能已经把这条服务换成了 pending-delete
            let service = open_service(manager, SERVICE_ALL_ACCESS);
            let _ = unsafe { CloseServiceHandle(manager) };
            let service = service?;
            let changed = unsafe {
                windows::Win32::System::Services::ChangeServiceConfigW(
                    service,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    PCWSTR(binary.as_ptr()),
                    PCWSTR::null(),
                    None,
                    PCWSTR::null(),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    PCWSTR(display.as_ptr()),
                )
            };
            let _ = unsafe { CloseServiceHandle(service) };
            changed.map_err(|err| CoreError::win32("ChangeServiceConfigW", err))?;
            return start();
        }
        Err(err) => {
            let _ = unsafe { CloseServiceHandle(manager) };
            return Err(CoreError::win32("CreateServiceW", err));
        }
    }

    let _ = unsafe { CloseServiceHandle(manager) };
    start()
}

/// 启动服务。已经在跑不算错误。
pub fn start() -> Result<()> {
    let manager = open_manager(SC_MANAGER_CONNECT)?;
    let service = open_service(manager, SERVICE_START | SERVICE_QUERY_STATUS);
    let _ = unsafe { CloseServiceHandle(manager) };
    let service = service?;

    let result = unsafe { StartServiceW(service, None) };
    let _ = unsafe { CloseServiceHandle(service) };
    match result {
        Ok(()) => Ok(()),
        Err(err) if win32_code(&err) == ERROR_SERVICE_ALREADY_RUNNING.0 => Ok(()),
        Err(err) => Err(CoreError::win32("StartServiceW", err)),
    }
}

/// 让服务停下来，并等它真正进入 `STOPPED`。
///
/// 返回 `Ok(true)` 表示确认已停。超时返回 `Ok(false)` —— 调用方自己决定要不要
/// 继续（卸载时即便停不下来也要删，SCM 会把它标成 pending delete）。
///
/// 没装过、本来就没在跑，都算「已停」。
pub fn stop(timeout: Duration) -> Result<bool> {
    let manager = open_manager(SC_MANAGER_CONNECT)?;
    let service = match open_service(manager, SERVICE_STOP | SERVICE_QUERY_STATUS) {
        Ok(handle) => handle,
        Err(CoreError::Win32 { err, .. })
            if win32_code(&err) == ERROR_SERVICE_DOES_NOT_EXIST.0 =>
        {
            let _ = unsafe { CloseServiceHandle(manager) };
            return Ok(true);
        }
        Err(err) => {
            let _ = unsafe { CloseServiceHandle(manager) };
            return Err(err);
        }
    };

    let mut status = SERVICE_STATUS::default();
    // 没在跑时会拿到 ERROR_SERVICE_NOT_ACTIVE，属正常
    let _ = unsafe { ControlService(service, SERVICE_CONTROL_STOP, &mut status) };
    let _ = unsafe { CloseServiceHandle(service) };
    let _ = unsafe { CloseServiceHandle(manager) };

    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        match state() {
            ServiceState::Stopped | ServiceState::NotInstalled => return Ok(true),
            // Running 但控制命令刚发出去：给它一点时间响应
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    Ok(matches!(state(), ServiceState::Stopped | ServiceState::NotInstalled))
}

/// 停掉并删除服务（需要管理员）。没装过算成功。
pub fn uninstall() -> Result<()> {
    let Ok(manager) = open_manager(SC_MANAGER_ALL_ACCESS) else {
        return Err(CoreError::ElevationDenied);
    };

    // 先等它停：服务进程还开着 exe 时，安装器删不掉那个文件（要拖到重启）
    let _ = stop(UNINSTALL_STOP_TIMEOUT);

    let service = match open_service(manager, SERVICE_ALL_ACCESS) {
        Ok(handle) => handle,
        Err(CoreError::Win32 { err, .. }) if win32_code(&err) == ERROR_SERVICE_DOES_NOT_EXIST.0 => {
            let _ = unsafe { CloseServiceHandle(manager) };
            return Ok(());
        }
        Err(err) => {
            let _ = unsafe { CloseServiceHandle(manager) };
            return Err(err);
        }
    };

    let deleted = unsafe { DeleteService(service) };
    let _ = unsafe { CloseServiceHandle(service) };
    let _ = unsafe { CloseServiceHandle(manager) };
    match deleted {
        Ok(()) => Ok(()),
        Err(err) => Err(CoreError::win32("DeleteService", err)),
    }
}

// ---------- 服务端：dispatcher 与主函数 ----------

/// 停止标志。服务控制处理器会置位，accept 循环与各会话据此退出。
static STOP: AtomicBool = AtomicBool::new(false);
/// `SERVICE_STATUS_HANDLE` 是裸指针（不是 `Send`），只能存成整数
static STATUS_HANDLE: AtomicIsize = AtomicIsize::new(0);
/// 服务主线程 id：`sc stop` 时要打断它正在进行的阻塞读
static MAIN_THREAD_ID: AtomicU32 = AtomicU32::new(0);

/// 服务进程入口：交给 SCM 的 dispatcher。**必须在主线程调用**，而且要尽快调用，
/// 否则 SCM 会以 1053（服务没有及时响应启动请求）判失败。
pub fn run_dispatcher() -> Result<()> {
    // 手动从命令行跑 `rastflow.exe --index-service` 时，`StartServiceCtrlDispatcherW`
    // 会**阻塞约 30 秒**才报 `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT`。
    // 那样看起来像卡死，所以先自己判一次。
    if !started_by_scm() {
        return Err(CoreError::Helper(
            "索引服务只能由 Windows 服务控制管理器启动".to_string(),
        ));
    }

    let name = to_wide(SERVICE_NAME);
    // 表必须以一个全零项结尾
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_ptr() as *mut u16),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    unsafe {
        StartServiceCtrlDispatcherW(table.as_ptr())
            .map_err(|err| CoreError::win32("StartServiceCtrlDispatcherW", err))
    }
}

fn report(state: windows::Win32::System::Services::SERVICE_STATUS_CURRENT_STATE, hint_ms: u32) {    let raw = STATUS_HANDLE.load(Ordering::SeqCst);
    if raw == 0 {
        return;
    }
    let handle = SERVICE_STATUS_HANDLE(raw as *mut core::ffi::c_void);
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        // 只在 RUNNING 之后才接受 STOP；start-pending 阶段拒绝任何控制
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP
        } else {
            0
        },
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: hint_ms,
    };
    let _ = unsafe { SetServiceStatus(handle, &status) };
}

/// 服务主函数：注册控制处理器 → 报告 RUNNING → 接受连接直到收到 STOP。
unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let name = to_wide(SERVICE_NAME);
    let handle = match unsafe {
        RegisterServiceCtrlHandlerExW(PCWSTR(name.as_ptr()), Some(control_handler), None)
    } {
        Ok(handle) => handle,
        // 注册不上就只能直接退出：SCM 会看到进程结束
        Err(_) => return,
    };
    STATUS_HANDLE.store(handle.0 as isize, Ordering::SeqCst);
    MAIN_THREAD_ID.store(super::current_thread_id(), Ordering::SeqCst);
    report(SERVICE_START_PENDING, 3000);

    // 真正的活交给会话循环；它返回后报告 STOPPED
    crate::search::helper_main::run_service_loop(&STOP, PIPE_NAME);
    report(SERVICE_STOPPED, 0);
}

/// 控制处理器：只关心 STOP / SHUTDOWN，其余（INTERROGATE 等）由 SCM 默认处理
unsafe extern "system" fn control_handler(
    control: u32,
    _event_type: u32,
    _event_data: *mut core::ffi::c_void,
    _context: *mut core::ffi::c_void,
) -> u32 {
    // SERVICE_CONTROL_STOP = 1，SERVICE_CONTROL_SHUTDOWN = 5
    if control == SERVICE_CONTROL_STOP || control == 5 {
        STOP.store(true, Ordering::SeqCst);
        // 主线程可能正阻塞在 `ReadFile`（等客户端的下一条指令）或 `ConnectNamedPipe` 上。
        // 光置标志它看不到，必须把这次同步 IO 取消掉。
        // （accept 阶段另有 2 秒轮询兜底，取消是空操作也没关系。）
        let thread_id = MAIN_THREAD_ID.load(Ordering::SeqCst);
        if thread_id != 0 {
            let _ = super::cancel_synchronous_io(thread_id);
        }
    }
    // NO_ERROR
    0
}

/// 报告「服务已就绪、正在接受连接」
pub fn report_running() {
    report(SERVICE_RUNNING, 0);
}

/// 当前进程是不是由服务控制管理器（`services.exe`）拉起的。
///
/// SCM 启动服务时，服务进程的父进程就是 `services.exe`。用户手动跑
/// `--index-service` 时父进程是终端或资源管理器，据此可以立刻退出。
fn started_by_scm() -> bool {
    let me = super::process::current_pid();
    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return false;
    };

    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut processes: Vec<(u32, u32, String)> = Vec::new();
    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok() {
        loop {
            let end = entry
                .szExeFile
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(entry.szExeFile.len());
            processes.push((
                entry.th32ProcessID,
                entry.th32ParentProcessID,
                String::from_utf16_lossy(&entry.szExeFile[..end]),
            ));
            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                break;
            }
        }
    }
    let _ = unsafe { windows::Win32::Foundation::CloseHandle(snapshot) };

    let Some(parent) = processes
        .iter()
        .find(|(pid, _, _)| *pid == me)
        .map(|(_, parent, _)| *parent)
    else {
        return false;
    };
    processes
        .iter()
        .any(|(pid, _, name)| *pid == parent && name.eq_ignore_ascii_case("services.exe"))
}

/// 等待服务进入 `Running`（最多 `timeout`）。安装后立刻连接会抢跑。
pub fn wait_until_running(timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if state().is_running() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    state().is_running()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_name_is_stable() {
        // 服务名是注册表键名：改了会让旧版本装的服务变成孤儿
        assert_eq!(SERVICE_NAME, "rastflow-index");
    }

    #[test]
    fn pipe_name_is_a_local_pipe() {
        assert!(PIPE_NAME.starts_with(r"\\.\pipe\"));
        // 名字固定是安全的：客户端会校验服务端是 LocalSystem
        assert!(PIPE_NAME.ends_with("rastflow-index-service"));
    }

    #[test]
    fn state_probe_does_not_panic() {
        // 没装服务时应当报 NotInstalled（本机装了 Everything 的服务，不是我们的）
        let state = state();
        assert!(matches!(
            state,
            ServiceState::NotInstalled
                | ServiceState::Stopped
                | ServiceState::Starting
                | ServiceState::Running
                | ServiceState::Unknown
        ));
    }

    #[test]
    fn installed_and_running_predicates_agree() {
        assert!(!ServiceState::NotInstalled.is_running());
        assert!(!ServiceState::Stopped.is_running());
        assert!(ServiceState::Running.is_running());
    }

    #[test]
    fn close_of_unknown_service_is_reported_as_missing() {
        // 只验证「查不存在的服务」这条路径不会 panic，也不会误判成没权限
        let manager = open_manager(SC_MANAGER_CONNECT);
        if let Ok(manager) = manager {
            let missing = open_service(manager, SERVICE_QUERY_STATUS);
            let _ = unsafe { CloseServiceHandle(manager) };
            // 本机没装 rastflow 的服务 → 应当报 ERROR_SERVICE_DOES_NOT_EXIST
            if let Err(CoreError::Win32 { err, .. }) = missing {
                assert_eq!(win32_code(&err), ERROR_SERVICE_DOES_NOT_EXIST.0);
            }
        }
    }
}
