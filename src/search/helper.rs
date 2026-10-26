//! 索引服务协议与桌面客户端。服务只提供元数据，索引与查询留在桌面进程。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::index::{JournalMark, MftEntry};
use super::monitor::MonitorEvent;
use super::win::pipe::{self, PipeHandle};
use super::win::process;
use super::win::service;
use super::{CoreError, Result};

/// 协议版本。字段一改就要加一，两边不一致时直接失败而不是读出垃圾。
pub const PROTOCOL_VERSION: u32 = 1;

/// 连常驻索引服务的超时。
///
/// 服务没在跑时 `CreateFileW` 会直接报「文件不存在」，所以这里主要是给
/// 「服务刚启动、管道还没建好」留一点窗口，不必像 UAC 那样等一分钟。
const SERVICE_CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);

// ---------- 消息类型 ----------

/// 主进程 → 子进程
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// 全量枚举这些盘（先清掉这些盘的旧条目）
    Scan { disks: Vec<char> },
    /// 从这些位点把日志重放到当前末尾
    CatchUp { marks: Vec<(char, JournalMark)> },
    /// 开始实时跟随（从子进程记下的位点接上）
    Monitor,
    /// 退出
    Stop,
}

/// 子进程 → 主进程
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// 握手：协议版本
    Ready {
        version: u32,
    },
    /// 开始枚举某个盘；`mark` 是本次枚举覆盖到的日志位点
    ScanStarted {
        disk: char,
        mark: JournalMark,
    },
    /// 一批 MFT 记录
    Records(Vec<MftEntry>),
    ScanFinished {
        disk: char,
        records: u64,
    },
    /// 重放完成；`mark` 是新的位点
    CatchUpFinished {
        disk: char,
        events: u64,
        mark: JournalMark,
    },
    /// 位点已失效，这个盘只能全量重扫
    NeedsRebuild {
        disk: char,
    },
    /// 这个盘现在碰不了（多半没权限）
    Unavailable {
        disk: char,
        reason: String,
    },
    /// 一批增量事件（重放与实时跟随共用）
    Events(Vec<MonitorEvent>),
    Warning {
        disk: char,
        reason: String,
    },
    /// 所有盘的跟随线程已就位；`monitoring` 为假表示一个盘都盯不上
    Monitoring {
        monitoring: bool,
    },
    /// 子进程要退出了
    Fatal(String),
    Bye,
}

// ---------- 编码 ----------

fn put_u8(buf: &mut Vec<u8>, value: u8) {
    buf.push(value);
}

fn put_u16(buf: &mut Vec<u8>, value: u16) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(buf: &mut Vec<u8>, value: u64) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn put_i64(buf: &mut Vec<u8>, value: i64) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn put_str(buf: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    // 名字与原因都不会超过 64 KB（NTFS 文件名上限 255 字符）
    put_u16(buf, bytes.len().min(u16::MAX as usize) as u16);
    buf.extend_from_slice(&bytes[..bytes.len().min(u16::MAX as usize)]);
}

fn put_mark(buf: &mut Vec<u8>, disk: char, mark: JournalMark) {
    put_u8(buf, disk as u8);
    put_u64(buf, mark.journal_id);
    put_i64(buf, mark.last_usn);
}

fn put_entry(buf: &mut Vec<u8>, entry: &MftEntry) {
    put_u8(buf, entry.disk as u8);
    put_u64(buf, entry.frn);
    put_u64(buf, entry.parent);
    put_u8(buf, u8::from(entry.is_dir));
    put_str(buf, &entry.name);
}

fn put_event(buf: &mut Vec<u8>, event: &MonitorEvent) {
    match event {
        MonitorEvent::Added {
            disk,
            frn,
            parent,
            name,
            is_dir,
            renamed,
        } => {
            put_u8(buf, 1);
            put_u8(buf, *disk as u8);
            put_u64(buf, *frn);
            put_u64(buf, *parent);
            put_u8(buf, u8::from(*is_dir));
            put_u8(buf, u8::from(*renamed));
            put_str(buf, name);
        }
        MonitorEvent::Removed { disk, frn, name } => {
            put_u8(buf, 2);
            put_u8(buf, *disk as u8);
            put_u64(buf, *frn);
            put_str(buf, name);
        }
        MonitorEvent::DiskWarning { disk, message } => {
            put_u8(buf, 3);
            put_u8(buf, *disk as u8);
            put_str(buf, message);
        }
        MonitorEvent::DiskStopped(disk) => {
            put_u8(buf, 4);
            put_u8(buf, *disk as u8);
        }
    }
}

/// 把消息编码进 `buf`（`buf` 只装 payload，长度前缀由 [`encode_frame`] 补）
fn encode_payload(message: &Message) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    match message {
        Message::Ready { version } => {
            put_u8(&mut buf, 1);
            put_u32(&mut buf, *version);
        }
        Message::ScanStarted { disk, mark } => {
            put_u8(&mut buf, 2);
            put_mark(&mut buf, *disk, *mark);
        }
        Message::Records(entries) => {
            put_u8(&mut buf, 3);
            put_u32(&mut buf, entries.len() as u32);
            for entry in entries {
                put_entry(&mut buf, entry);
            }
        }
        Message::ScanFinished { disk, records } => {
            put_u8(&mut buf, 4);
            put_u8(&mut buf, *disk as u8);
            put_u64(&mut buf, *records);
        }
        Message::CatchUpFinished { disk, events, mark } => {
            put_u8(&mut buf, 5);
            put_u8(&mut buf, *disk as u8);
            put_u64(&mut buf, *events);
            put_u64(&mut buf, mark.journal_id);
            put_i64(&mut buf, mark.last_usn);
        }
        Message::NeedsRebuild { disk } => {
            put_u8(&mut buf, 6);
            put_u8(&mut buf, *disk as u8);
        }
        Message::Unavailable { disk, reason } => {
            put_u8(&mut buf, 7);
            put_u8(&mut buf, *disk as u8);
            put_str(&mut buf, reason);
        }
        Message::Events(events) => {
            put_u8(&mut buf, 8);
            put_u32(&mut buf, events.len() as u32);
            for event in events {
                put_event(&mut buf, event);
            }
        }
        Message::Warning { disk, reason } => {
            put_u8(&mut buf, 9);
            put_u8(&mut buf, *disk as u8);
            put_str(&mut buf, reason);
        }
        Message::Monitoring { monitoring } => {
            put_u8(&mut buf, 10);
            put_u8(&mut buf, u8::from(*monitoring));
        }
        Message::Fatal(reason) => {
            put_u8(&mut buf, 11);
            put_str(&mut buf, reason);
        }
        Message::Bye => put_u8(&mut buf, 12),
    }
    buf
}

fn encode_request(request: &Request) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32);
    match request {
        Request::Scan { disks } => {
            put_u8(&mut buf, 20);
            put_u8(&mut buf, disks.len() as u8);
            for disk in disks {
                put_u8(&mut buf, *disk as u8);
            }
        }
        Request::CatchUp { marks } => {
            put_u8(&mut buf, 21);
            put_u8(&mut buf, marks.len() as u8);
            for (disk, mark) in marks {
                put_mark(&mut buf, *disk, *mark);
            }
        }
        Request::Monitor => put_u8(&mut buf, 22),
        Request::Stop => put_u8(&mut buf, 23),
    }
    buf
}

fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

// ---------- 解码 ----------

struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
}

fn corrupt() -> CoreError {
    CoreError::Helper("协议数据损坏".to_string())
}

impl<'a> Decoder<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(len).ok_or_else(corrupt)?;
        let slice = self.buf.get(self.pos..end).ok_or_else(corrupt)?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }

    fn disk(&mut self) -> Result<char> {
        let code = self.u8()?;
        if !code.is_ascii_alphabetic() {
            return Err(corrupt());
        }
        Ok(code.to_ascii_uppercase() as char)
    }

    fn text(&mut self) -> Result<String> {
        let len = self.u16()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| corrupt())
    }

    fn mark(&mut self) -> Result<(char, JournalMark)> {
        let disk = self.disk()?;
        let journal_id = self.u64()?;
        let last_usn = self.i64()?;
        Ok((
            disk,
            JournalMark {
                last_usn,
                journal_id,
            },
        ))
    }

    fn entry(&mut self) -> Result<MftEntry> {
        Ok(MftEntry {
            disk: self.disk()?,
            frn: self.u64()?,
            parent: self.u64()?,
            is_dir: self.u8()? != 0,
            name: self.text()?,
        })
    }

    fn event(&mut self) -> Result<MonitorEvent> {
        match self.u8()? {
            1 => Ok(MonitorEvent::Added {
                disk: self.disk()?,
                frn: self.u64()?,
                parent: self.u64()?,
                is_dir: self.u8()? != 0,
                renamed: self.u8()? != 0,
                name: self.text()?,
            }),
            2 => Ok(MonitorEvent::Removed {
                disk: self.disk()?,
                frn: self.u64()?,
                name: self.text()?,
            }),
            3 => Ok(MonitorEvent::DiskWarning {
                disk: self.disk()?,
                message: self.text()?,
            }),
            4 => Ok(MonitorEvent::DiskStopped(self.disk()?)),
            _ => Err(corrupt()),
        }
    }
}

fn decode_message(payload: &[u8]) -> Result<Message> {
    let mut decoder = Decoder::new(payload);
    let message = match decoder.u8()? {
        1 => Message::Ready {
            version: decoder.u32()?,
        },
        2 => {
            let (disk, mark) = decoder.mark()?;
            Message::ScanStarted { disk, mark }
        }
        3 => {
            let count = decoder.u32()? as usize;
            let mut entries = Vec::with_capacity(count.min(4096));
            for _ in 0..count {
                entries.push(decoder.entry()?);
            }
            Message::Records(entries)
        }
        4 => Message::ScanFinished {
            disk: decoder.disk()?,
            records: decoder.u64()?,
        },
        5 => {
            let disk = decoder.disk()?;
            let events = decoder.u64()?;
            let journal_id = decoder.u64()?;
            let last_usn = decoder.i64()?;
            Message::CatchUpFinished {
                disk,
                events,
                mark: JournalMark {
                    last_usn,
                    journal_id,
                },
            }
        }
        6 => Message::NeedsRebuild {
            disk: decoder.disk()?,
        },
        7 => Message::Unavailable {
            disk: decoder.disk()?,
            reason: decoder.text()?,
        },
        8 => {
            let count = decoder.u32()? as usize;
            let mut events = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                events.push(decoder.event()?);
            }
            Message::Events(events)
        }
        9 => Message::Warning {
            disk: decoder.disk()?,
            reason: decoder.text()?,
        },
        10 => Message::Monitoring {
            monitoring: decoder.u8()? != 0,
        },
        11 => Message::Fatal(decoder.text()?),
        12 => Message::Bye,
        _ => return Err(corrupt()),
    };
    Ok(message)
}

fn decode_request(payload: &[u8]) -> Result<Request> {
    let mut decoder = Decoder::new(payload);
    let request = match decoder.u8()? {
        20 => {
            let count = decoder.u8()? as usize;
            let mut disks = Vec::with_capacity(count);
            for _ in 0..count {
                disks.push(decoder.disk()?);
            }
            Request::Scan { disks }
        }
        21 => {
            let count = decoder.u8()? as usize;
            let mut marks = Vec::with_capacity(count);
            for _ in 0..count {
                marks.push(decoder.mark()?);
            }
            Request::CatchUp { marks }
        }
        22 => Request::Monitor,
        23 => Request::Stop,
        _ => return Err(corrupt()),
    };
    Ok(request)
}

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
        if !process::is_trusted_server_process(pid) {
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
    use super::*;

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

    fn roundtrip(message: &Message) -> Message {
        let payload = encode_payload(message);
        decode_message(&payload).expect("应当能解回来")
    }

    #[test]
    fn every_message_survives_a_roundtrip() {
        let mark = JournalMark {
            last_usn: -12,
            journal_id: 0xDEAD_BEEF,
        };
        let messages = vec![
            Message::Ready {
                version: PROTOCOL_VERSION,
            },
            Message::ScanStarted { disk: 'C', mark },
            Message::Records(vec![MftEntry {
                disk: 'D',
                frn: 0x0005_0000_0000_1234,
                parent: 0x0005_0000_0000_0005,
                is_dir: true,
                name: "中文目录".to_string(),
            }]),
            Message::ScanFinished {
                disk: 'C',
                records: 1_234_567,
            },
            Message::CatchUpFinished {
                disk: 'E',
                events: 42,
                mark,
            },
            Message::NeedsRebuild { disk: 'C' },
            Message::Unavailable {
                disk: 'C',
                reason: "拒绝访问".to_string(),
            },
            Message::Events(vec![
                MonitorEvent::Added {
                    disk: 'C',
                    frn: 7,
                    parent: 5,
                    name: "a.txt".to_string(),
                    is_dir: false,
                    renamed: true,
                },
                MonitorEvent::Removed {
                    disk: 'C',
                    frn: 7,
                    name: "a.txt".to_string(),
                },
                MonitorEvent::DiskWarning {
                    disk: 'C',
                    message: "日志被重建".to_string(),
                },
                MonitorEvent::DiskStopped('C'),
            ]),
            Message::Warning {
                disk: 'C',
                reason: "卷忙".to_string(),
            },
            Message::Monitoring { monitoring: false },
            Message::Fatal("子进程要退出了".to_string()),
            Message::Bye,
        ];
        for message in messages {
            assert_eq!(roundtrip(&message), message);
        }
    }

    #[test]
    fn every_request_survives_a_roundtrip() {
        let mark = JournalMark {
            last_usn: 7,
            journal_id: 9,
        };
        let requests = vec![
            Request::Scan {
                disks: vec!['C', 'D'],
            },
            Request::CatchUp {
                marks: vec![('C', mark), ('D', mark)],
            },
            Request::Monitor,
            Request::Stop,
        ];
        for request in requests {
            let payload = encode_request(&request);
            assert_eq!(decode_request(&payload).expect("应当能解回来"), request);
        }
    }

    #[test]
    fn garbage_payload_is_rejected() {
        assert!(decode_message(&[]).is_err());
        assert!(decode_message(&[99, 1, 2, 3]).is_err());
        // 声称有 4 条记录但一条也没有：不能 panic，要报错
        assert!(decode_message(&[3, 4, 0, 0, 0]).is_err());
        assert!(decode_request(&[20, 200]).is_err());
    }

    #[test]
    fn frames_carry_their_length() {
        let payload = encode_request(&Request::Stop);
        let framed = frame(&payload);
        assert_eq!(&framed[..4], &(payload.len() as u32).to_le_bytes());
        assert_eq!(&framed[4..], payload.as_slice());
    }

    /// 真的装一遍服务、连上它、再卸掉。
    ///
    /// ⚠️ 需要**管理员**，而且会**真的修改系统**（注册后立即删除一个 Windows 服务）。
    /// 手动跑：
    ///
    /// ```powershell
    /// cargo test --bin rastflow service_install -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "需要管理员权限，会真的注册/删除 Windows 服务"]
    fn service_install_roundtrip() {
        // 起点必须是「没装」，否则测不出安装本身
        if service::state().is_running() {
            eprintln!("索引服务已在运行，先跳过（请先卸载再跑）");
            return;
        }

        install_service().expect("安装服务失败（多半不是管理员）");
        assert!(
            service::wait_until_running(Duration::from_secs(15)),
            "服务应当进入 Running"
        );

        // 服务在跑，主进程连它不需要任何提权
        let session = HelperSession::connect_running_service().expect("应当能连上索引服务");
        drop(session);

        uninstall_service().expect("卸载服务失败");
        assert!(
            matches!(
                service::state(),
                crate::search::win::service::ServiceState::NotInstalled
            ),
            "卸载后应当查不到服务"
        );
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
