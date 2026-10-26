//! 索引服务协议与桌面客户端。服务只提供元数据，索引与查询留在桌面进程。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::win::pipe::PipeHandle;
use super::win::process;
use super::win::service;
use super::{CoreError, Result};

/// 协议版本。字段一改就要加一，两边不一致时直接失败而不是读出垃圾。
pub use super::protocol::{Message, PROTOCOL_VERSION, Request};
use super::protocol::{decode_message, decode_request, encode_payload, encode_request, frame};

/// 连常驻索引服务的超时。
///
/// 服务没在跑时 `CreateFileW` 会直接报「文件不存在」，所以这里主要是给
/// 「服务刚启动、管道还没建好」留一点窗口，不必像 UAC 那样等一分钟。
const SERVICE_CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);

// ---------- 管道封装 ----------

/// 写端。内部持有编码缓冲，多个线程（扫描线程、每条跟随线程、心跳）共用。
pub struct PipeWriter {
    handle: Arc<PipeHandle>,
    /// 整帧必须一次写出：多线程同时写时，帧不能交错
    buffer: Mutex<Vec<u8>>,
}

impl PipeWriter {
    pub fn new(handle: Arc<PipeHandle>) -> Self {
        Self {
            handle,
            buffer: Mutex::new(Vec::with_capacity(64 * 1024)),
        }
    }

    pub fn send(&self, message: &Message) -> Result<()> {
        let payload = encode_payload(message);
        let frame = frame(&payload);
        let mut buffer = match self.buffer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        buffer.clear();
        buffer.extend_from_slice(&frame);
        self.handle.write_all(&buffer)
    }

    pub fn send_request(&self, request: &Request) -> Result<()> {
        let payload = encode_request(request);
        let frame = frame(&payload);
        let mut buffer = match self.buffer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        buffer.clear();
        buffer.extend_from_slice(&frame);
        self.handle.write_all(&buffer)
    }
}

/// 读端
pub struct PipeReader {
    handle: Arc<PipeHandle>,
}

impl PipeReader {
    pub fn new(handle: Arc<PipeHandle>) -> Self {
        Self { handle }
    }

    /// 读一条消息；`None` 表示对端已关闭
    pub fn recv(&self) -> Result<Option<Message>> {
        let Some(payload) = self.read_frame()? else {
            return Ok(None);
        };
        decode_message(&payload).map(Some)
    }

    /// 读一条请求；`None` 表示对端已关闭
    pub fn recv_request(&self) -> Result<Option<Request>> {
        let Some(payload) = self.read_frame()? else {
            return Ok(None);
        };
        decode_request(&payload).map(Some)
    }

    fn read_frame(&self) -> Result<Option<Vec<u8>>> {
        let mut header = [0u8; 4];
        if !self.handle.read_exact(&mut header)? {
            return Ok(None);
        }
        let len = u32::from_le_bytes(header) as usize;
        // 上限保护：损坏的长度会让这里申请巨量内存
        if len > MAX_FRAME_LEN {
            return Err(CoreError::Helper(format!("消息长度异常：{len}")));
        }
        let mut payload = vec![0u8; len];
        if !self.handle.read_exact(&mut payload)? {
            return Err(CoreError::Helper("管道在半条消息处断开".to_string()));
        }
        Ok(Some(payload))
    }
}

/// 单条消息上限。扫描批是 2048 条记录（约 60 KB），留足余量。
const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

// ---------- 主进程侧会话 ----------

/// 桌面与常驻 Windows 服务的一次连接。不会创建或终止服务进程。
pub struct HelperSession {
    reader: PipeReader,
    writer: PipeWriter,
    pid: u32,
}

impl HelperSession {
    /// 只连常驻索引服务，**绝不提权**。
    ///
    /// 启动路径用它：服务不可用时宁可这轮没有实时增量，也不为了它弹 UAC。
    pub fn connect_running_service() -> Result<Self> {
        let session = Self::connect_service()?;
        eprintln!(
            "[rastflow] 已连接索引服务（pid {}），无需管理员权限",
            session.pid
        );
        Ok(session)
    }

    /// 连常驻服务。管道名是固定的，所以必须确认对面真是以 `LocalSystem` 运行的服务。
    fn connect_service() -> Result<Self> {
        let handle = PipeHandle::connect(service::PIPE_NAME, SERVICE_CONNECT_TIMEOUT)?;
        let pid = handle
            .server_pid()
            .ok_or_else(|| CoreError::Helper("取不到索引服务的进程 id".to_string()))?;
        if service::process_id() != Some(pid) {
            // 有人抢先把管道名占了：拒绝，而不是把文件名列表交给一个陌生进程
            return Err(CoreError::Helper(format!(
                "索引管道的服务端不是索引服务（pid {pid}）"
            )));
        }
        Self::handshake(handle, pid)
    }

    fn handshake(handle: Arc<PipeHandle>, pid: u32) -> Result<Self> {
        let reader = PipeReader::new(Arc::clone(&handle));
        let writer = PipeWriter::new(handle);

        let version = match reader.recv()? {
            Some(Message::Ready { version }) => version,
            Some(other) => {
                return Err(CoreError::Helper(format!("握手消息不对：{other:?}")));
            }
            None => return Err(CoreError::Helper("对端在握手前就退出了".to_string())),
        };
        if version != PROTOCOL_VERSION {
            return Err(CoreError::Helper(format!(
                "协议版本不一致（对端 {version}，主进程 {PROTOCOL_VERSION}）"
            )));
        }

        Ok(Self {
            reader,
            writer,
            pid,
        })
    }

    pub fn volumes(&self) -> Result<Vec<super::model::VolumeInfo>> {
        self.send(&Request::ListVolumes)?;
        match self.recv()? {
            Some(Message::Volumes(volumes)) => Ok(volumes),
            _ => Err(CoreError::Helper("服务未返回卷列表".into())),
        }
    }

    /// 会话对端的进程 id（服务会话时是服务的 pid）
    pub fn child_pid(&self) -> u32 {
        self.pid
    }

    pub fn send(&self, request: &Request) -> Result<()> {
        self.writer.send_request(request)
    }

    /// 收一条消息；`None` 表示对端已退出
    pub fn recv(&self) -> Result<Option<Message>> {
        self.reader.recv()
    }
}

impl Drop for HelperSession {
    fn drop(&mut self) {
        // 先好好说一声，让对端停掉跟随线程并结束会话。
        // 服务会回到 accept 继续等下一个客户端，所以这里不能杀它。
        let _ = self.send(&Request::Stop);
    }
}

// ---------- 服务管理（供设置/状态面板调用） ----------

/// 安装并启动服务。**需要管理员**：调用方应当先提权（见 [`run_service_installer`]）。
pub fn install_service() -> Result<()> {
    service::install()
}

/// 停止并删除服务。**需要管理员**。
pub fn uninstall_service() -> Result<()> {
    service::uninstall()
}

/// 以管理员身份执行一次 `--install-service` / `--uninstall-service`。
///
/// 这是**唯一**会弹 UAC 的地方，而且只在用户主动点「安装索引服务」时发生。
pub fn run_service_installer(install: bool) -> Result<()> {
    let flag = if install {
        service::INSTALL_FLAG
    } else {
        service::UNINSTALL_FLAG
    };
    // 用 WaitForSingleObject 等它跑完，这样返回时状态查询就是准确的
    process::run_elevated_and_wait(flag)
}

#[cfg(test)]
mod tests {
    use super::super::win::pipe;
    use super::*;
    use crate::search::index::MftEntry;

    fn pipe_name() -> String {
        format!(
            r"\\.\pipe\rastflow-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    /// 真的建一对命名管道，把一条大消息（远超单次 `ReadFile` 的返回量）送过去。
    ///
    /// 覆盖的是协议之外的管道行为：连接、分片读、帧边界。
    #[test]
    fn pipe_carries_a_large_batch_between_threads() {
        let name = pipe_name();
        let server = PipeHandle::create_server(&name).expect("建服务端失败");

        let client_name = name.clone();
        let client = std::thread::spawn(move || {
            let handle =
                PipeHandle::connect(&client_name, Duration::from_secs(5)).expect("连接服务端失败");
            let writer = PipeWriter::new(handle);
            let entries: Vec<MftEntry> = (0..2048)
                .map(|n| MftEntry {
                    disk: 'C',
                    frn: n as u64,
                    parent: 5,
                    is_dir: n % 2 == 0,
                    name: format!("rastflow-测试-{n}.txt"),
                })
                .collect();
            writer
                .send(&Message::Records(entries.clone()))
                .expect("发送失败");
            entries.len()
        });

        pipe::accept(&server, Duration::from_secs(5)).expect("服务端应当连上");
        let reader = PipeReader::new(Arc::clone(&server));
        match reader.recv().expect("应当能读到").expect("应当有消息") {
            Message::Records(entries) => {
                assert_eq!(entries.len(), client.join().unwrap());
                assert_eq!(entries[7].name, "rastflow-测试-7.txt");
                assert!(entries[8].is_dir);
            }
            other => panic!("应当是 Records，实际 {other:?}"),
        }
    }

    #[test]
    fn accept_gives_up_when_nobody_connects() {
        let name = pipe_name();
        let server = PipeHandle::create_server(&name).expect("建服务端失败");
        let started = std::time::Instant::now();
        let result = pipe::accept(&server, Duration::from_millis(100));
        assert!(result.is_err(), "没人连上就该超时");
        // 超时之后必须能把等待线程收回来，不能挂着
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "超时后不该长时间卡住"
        );
    }
}
