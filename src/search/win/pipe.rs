//! 命名管道：主进程（服务端）与提权索引子进程（客户端）之间唯一的通道。
//!
//! 服务端由**主进程**创建、子进程去连。反过来的话，任何本地进程都能连上那个管道，
//! 把一个提权进程看到的整盘文件名列表要出来。
//!
//! 只做同步（阻塞）读写：一个方向一条线程，正好对上「一侧读、另一侧写」的用法。

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_NO_DATA, ERROR_PIPE_BUSY,
    ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL, INVALID_HANDLE_VALUE,
    LocalFree,
};
use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_MODE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile,
    WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    GetNamedPipeServerProcessId, NAMED_PIPE_MODE, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows::core::PCWSTR;

use super::super::{CoreError, Result};
use super::{cancel_synchronous_io, current_thread_id, to_wide};

/// 管道内核缓冲区大小。扫描时一批记录约 30 KB，留足余量避免写端阻塞。
const PIPE_BUFFER_LEN: u32 = 64 * 1024;

/// 等待连接/连接重试时每次等待的毫秒数
const CONNECT_POLL_MS: u32 = 250;

/// 索引服务管道显式 DACL。
///
/// 不指定的话，管道继承服务进程（`LocalSystem`）的默认 DACL —— 里面
/// `Authenticated Users` 只有读权限，而客户端 [`PipeHandle::connect`] 请求的是
/// `GENERIC_READ | GENERIC_WRITE`，于是**普通用户连接一律 Access Denied**。
/// 主进程随后会误判「服务不可用」而回退到 `runas`，每次启动都弹 UAC。
///
/// 这里给：SYSTEM / Administrators 完全控制，`Authenticated Users` 读写。
/// 加上 `PIPE_REJECT_REMOTE_CLIENTS`，仍然只对本机已登录用户开放。
const SERVICE_PIPE_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)";

/// `SDDL_REVISION_1`
const SDDL_REVISION_1: u32 = 1;

/// 取 `windows::core::Error` 里的原始 Win32 错误码。
///
/// windows-rs 会把 Win32 错误码包成 `HRESULT_FROM_WIN32(code)`（`0x8007_0000 | code`），
/// 但也有保持原值的情况，所以只取低 16 位 —— 两种编码都能对上。
pub fn win32_code(err: &windows::core::Error) -> u32 {
    (err.code().0 as u32) & 0xFFFF
}

/// 把 [`SERVICE_PIPE_SDDL`] 转成可传给 `CreateNamedPipeW` 的安全属性。
///
/// 返回描述符的所有权在调用方：必须活到 `CreateNamedPipeW` 返回，然后用
/// `LocalFree` 释放（由 [`PipeHandle::create_server_multi`] 负责）。
fn service_security_attributes() -> Result<(SECURITY_ATTRIBUTES, PSECURITY_DESCRIPTOR)> {
    let sddl = to_wide(SERVICE_PIPE_SDDL);
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    }
    .map_err(|err| {
        CoreError::win32(
            "ConvertStringSecurityDescriptorToSecurityDescriptorW",
            err,
        )
    })?;

    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    Ok((attributes, descriptor))
}

/// 一端的管道句柄。
///
/// `HANDLE` 是内核对象句柄，本身可以在线程间使用：读线程与写线程各持一份
/// `Arc<PipeHandle>` 是管道支持的用法。
pub struct PipeHandle {
    handle: HANDLE,
    server: bool,
}

unsafe impl Send for PipeHandle {}
unsafe impl Sync for PipeHandle {}

impl PipeHandle {
    /// 建服务端（一次性提权子进程用）：只允许一个实例。
    pub fn create_server(name: &str) -> Result<Arc<Self>> {
        Self::create_server_with(name, 1, None)
    }

    /// 建服务端（常驻索引服务用）。
    ///
    /// 实例数取上限而不是 1：服务每次 accept 超时后都会丢弃旧实例重新建，
    /// 万一前一个实例还卡在内核里，「已存在同名实例」会让新实例建不出来。
    ///
    /// 安全性靠 `A;;GA;;;BA` + `A;;GRGW;;;AU` 的 DACL（见 [`SERVICE_PIPE_SDDL`]）。
    pub fn create_server_multi(name: &str) -> Result<Arc<Self>> {
        let (attributes, descriptor) = service_security_attributes()?;
        let result = Self::create_server_with(
            name,
            PIPE_UNLIMITED_INSTANCES,
            Some(&attributes),
        );
        // 描述符只要活到 `CreateNamedPipeW` 返回即可
        if !descriptor.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(descriptor.0)));
            }
        }
        result
    }

    fn create_server_with(
        name: &str,
        instances: u32,
        security: Option<&SECURITY_ATTRIBUTES>,
    ) -> Result<Arc<Self>> {
        let wide = to_wide(name);
        let mode = NAMED_PIPE_MODE(
            PIPE_TYPE_BYTE.0 | PIPE_READMODE_BYTE.0 | PIPE_WAIT.0 | PIPE_REJECT_REMOTE_CLIENTS.0,
        );
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                mode,
                instances,
                PIPE_BUFFER_LEN,
                PIPE_BUFFER_LEN,
                0,
                security.map(|attributes| attributes as *const SECURITY_ATTRIBUTES),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(CoreError::win32(
                "CreateNamedPipeW",
                windows::core::Error::from_thread(),
            ));
        }
        Ok(Arc::new(Self {
            handle,
            server: true,
        }))
    }

    /// 连服务端（子进程侧）。管道还没建好时会重试到 `timeout`。
    pub fn connect(name: &str, timeout: Duration) -> Result<Arc<Self>> {
        let wide = to_wide(name);
        let deadline = Instant::now() + timeout;
        loop {
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(wide.as_ptr()),
                    GENERIC_READ.0 | GENERIC_WRITE.0,
                    FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    None,
                )
            };
            match handle {
                Ok(handle) => {
                    return Ok(Arc::new(Self {
                        handle,
                        server: false,
                    }));
                }
                Err(err) => {
                    let code = win32_code(&err);
                    // `PIPE_BUSY`：实例都占满了；`FILE_NOT_FOUND`：管道还没被建出来
                    // （服务刚启动 / 主进程刚拉起子进程）。两种都值得再等一会儿。
                    let retry = code == ERROR_PIPE_BUSY.0 || code == ERROR_FILE_NOT_FOUND.0;
                    if !retry || Instant::now() >= deadline {
                        return Err(CoreError::win32("CreateFileW(索引管道)", err));
                    }
                    // 等一会儿再试
                    unsafe {
                        let _ = WaitNamedPipeW(PCWSTR(wide.as_ptr()), CONNECT_POLL_MS);
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    /// 连上来的客户端进程 id（用于确认对面确实是我们要的那个子进程）
    pub fn client_pid(&self) -> Option<u32> {
        let mut pid = 0u32;
        unsafe { GetNamedPipeClientProcessId(self.handle, &mut pid) }
            .ok()
            .map(|()| pid)
    }

    /// 对面的服务端进程 id（客户端侧用，确认它不是别人冒充的管道）
    pub fn server_pid(&self) -> Option<u32> {
        let mut pid = 0u32;
        unsafe { GetNamedPipeServerProcessId(self.handle, &mut pid) }
            .ok()
            .map(|()| pid)
    }

    /// 读满 `buf`；返回 `false` 表示对端已关闭（正常结束，不是错误）
    pub fn read_exact(&self, buf: &mut [u8]) -> Result<bool> {
        let mut filled = 0usize;
        while filled < buf.len() {
            let mut read = 0u32;
            let result = unsafe {
                ReadFile(
                    self.handle,
                    Some(&mut buf[filled..]),
                    Some(&mut read),
                    None,
                )
            };
            match result {
                Ok(()) if read > 0 => filled += read as usize,
                // 0 字节就是「对端关了」
                Ok(()) => return Ok(false),
                Err(err) => {
                    let code = win32_code(&err);
                    if code == ERROR_BROKEN_PIPE.0 || code == ERROR_NO_DATA.0 {
                        return Ok(false);
                    }
                    return Err(CoreError::win32("ReadFile(索引管道)", err));
                }
            }
        }
        Ok(true)
    }

    pub fn write_all(&self, data: &[u8]) -> Result<()> {
        let mut written_total = 0usize;
        while written_total < data.len() {
            let mut written = 0u32;
            if let Err(err) = unsafe {
                WriteFile(
                    self.handle,
                    Some(&data[written_total..]),
                    Some(&mut written),
                    None,
                )
            } {
                let code = win32_code(&err);
                if code == ERROR_BROKEN_PIPE.0 || code == ERROR_NO_DATA.0 {
                    return Err(CoreError::Helper("索引管道已断开".to_string()));
                }
                return Err(CoreError::win32("WriteFile(索引管道)", err));
            }
            if written == 0 {
                return Err(CoreError::Helper("索引管道写入 0 字节".to_string()));
            }
            written_total += written as usize;
        }
        Ok(())
    }
}

impl Drop for PipeHandle {
    fn drop(&mut self) {
        unsafe {
            if self.server {
                // 先断开再关闭：客户端会立刻收到「管道已关闭」，不会等超时
                let _ = DisconnectNamedPipe(self.handle);
            }
            let _ = CloseHandle(self.handle);
        }
    }
}

/// 等服务端连上客户端。
///
/// `ConnectNamedPipe` 没有超时参数，所以放到一条线程里等、主线程等结果；
/// 超时后要 `CancelSynchronousIo` 把那条线程叫醒，否则它会永远挂着。
pub fn accept(handle: &PipeHandle, timeout: Duration) -> Result<()> {
    // 只传句柄的数值：`HANDLE` 内部是裸指针，不是 `Send`（而且闭包会按字段捕获，
    // 光给外层结构体加 `unsafe impl Send` 是不够的）
    let raw = handle.handle.0 as usize;
    // 跨线程只传原始错误码：`windows::core::Error` 内部也带裸指针
    let (sender, receiver) = mpsc::channel::<std::result::Result<(), u32>>();
    let thread_id = Arc::new(AtomicU32::new(0));
    let id_slot = Arc::clone(&thread_id);
    let worker = std::thread::Builder::new()
        .name("rastflow-pipe-accept".to_string())
        .spawn(move || {
            id_slot.store(current_thread_id(), Ordering::SeqCst);
            let named = HANDLE(raw as *mut c_void);
            let result = unsafe { ConnectNamedPipe(named, None) };
            let _ = sender.send(result.map_err(|err| win32_code(&err)));
        })
        .map_err(CoreError::Io)?;

    match receiver.recv_timeout(timeout) {
        // 客户端在我们调用 ConnectNamedPipe 之前就连上了：这也是成功
        Ok(Err(code)) if code == ERROR_PIPE_CONNECTED.0 => {
            let _ = worker.join();
            Ok(())
        }
        Ok(Err(code)) => {
            let _ = worker.join();
            Err(CoreError::Io(std::io::Error::from_raw_os_error(code as i32)))
        }
        Ok(Ok(())) => {
            let _ = worker.join();
            Ok(())
        }
        Err(_) => {
            let id = thread_id.load(Ordering::SeqCst);
            for _ in 0..100 {
                if worker.is_finished() {
                    break;
                }
                let _ = cancel_synchronous_io(id);
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = worker.join();
            Err(CoreError::Helper(
                "等待索引子进程连接超时（提权可能被取消）".to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 服务端管道必须让普通用户（`Authenticated Users`）真的能读写连接。
    ///
    /// 这条断言是有鉴别力的：DACL 里只有 `SYSTEM` / `Administrators` / `AU` 三类 ACE，
    /// 没有单独的「当前用户」授权；而 [`PipeHandle::connect`] 请求的是
    /// `GENERIC_READ | GENERIC_WRITE`。所以只要 `AU` 的授权不够，这里就会
    /// `Access Denied` —— 也就是修复前主进程连服务失败、回退 `runas` 弹 UAC 的样子。
    #[test]
    fn service_pipe_grants_authenticated_users_read_write() {
        let name = format!(r"\\.\pipe\rastflow-test-acl-{}", std::process::id());
        let server = PipeHandle::create_server_multi(&name).expect("创建服务端管道");

        let client = PipeHandle::connect(&name, Duration::from_secs(1))
            .expect("普通用户应当能连上服务端管道（需要读+写权限）");

        drop(client);
        drop(server);
    }

    /// 服务端管道的 DACL 必须真的授予 `Authenticated Users` 读写。
    ///
    /// 修复前的 bug：`CreateNamedPipeW(..., None)` 让管道继承服务进程
    /// （`LocalSystem`）的默认 DACL，里面只有 `Everyone: FR`（只读）。而
    /// [`PipeHandle::connect`] 请求 `GENERIC_READ | GENERIC_WRITE`，于是普通用户
    /// 一律 `Access Denied` → 主进程误判服务不可用 → 回退 `runas` → 每次启动弹 UAC。
    ///
    /// 这条断言是看**读回来的 DACL**，而不是「能不能连上」：测试进程自己创建的
    /// 默认管道会把完全控制直接授给当前用户 SID，连接照样成功，没有鉴别力。
    #[test]
    fn service_pipe_dacl_grants_read_write_to_authenticated_users() {
        let service_name = format!(r"\\.\pipe\rastflow-test-acl-{}", std::process::id());
        let server = PipeHandle::create_server_multi(&service_name).expect("创建服务端管道");
        let dacl = pipe_dacl_sddl(server.handle);
        drop(server);

        // `0x12019f` = GENERIC_READ | GENERIC_WRITE | SYNCHRONIZE
        assert!(
            dacl.contains("0x12019f;;;AU)"),
            "服务端管道没有给 Authenticated Users 读写权限：{dacl}"
        );
    }

    /// 读回管道对象上真实生效的 DACL，转成 SDDL 字符串
    fn pipe_dacl_sddl(handle: HANDLE) -> String {
        use windows::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SE_KERNEL_OBJECT,
        };
        use windows::Win32::Security::{ACL, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};
        use windows::core::PWSTR;

        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let status = unsafe {
            GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut dacl),
                None,
                Some(&mut descriptor),
            )
        };
        assert_eq!(status.0, 0, "GetSecurityInfo 失败");

        let mut sddl = PWSTR::null();
        let mut length = 0u32;
        unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                1,
                DACL_SECURITY_INFORMATION,
                &mut sddl,
                Some(&mut length),
            )
        }
        .expect("DACL 转 SDDL");
        let text = unsafe { sddl.to_string() }.unwrap_or_default();

        unsafe {
            let _ = LocalFree(Some(HLOCAL(sddl.0 as *mut c_void)));
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        }
        text
    }
}
