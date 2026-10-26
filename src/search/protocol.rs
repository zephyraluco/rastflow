//! 有界二进制协议。纯编解码不依赖 Windows API。
use super::index::{JournalMark, MftEntry};
use super::model::MonitorEvent;
use super::{CoreError, Result};
pub const PROTOCOL_VERSION: u32 = 2;

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
        MonitorEvent::Checkpoint { disk, mark } => {
            put_u8(buf, 5);
            put_mark(buf, *disk, *mark);
        }
    }
}

/// 把消息编码进 `buf`（`buf` 只装 payload，长度前缀由 [`encode_frame`] 补）
pub(crate) fn encode_payload(message: &Message) -> Vec<u8> {
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

pub(crate) fn encode_request(request: &Request) -> Vec<u8> {
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

pub(crate) fn frame(payload: &[u8]) -> Vec<u8> {
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
            5 => {
                let (disk, mark) = self.mark()?;
                Ok(MonitorEvent::Checkpoint { disk, mark })
            }
            _ => Err(corrupt()),
        }
    }
}

pub(crate) fn decode_message(payload: &[u8]) -> Result<Message> {
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
    if decoder.pos != payload.len() {
        return Err(corrupt());
    }
    Ok(message)
}

pub(crate) fn decode_request(payload: &[u8]) -> Result<Request> {
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
    if decoder.pos != payload.len() {
        return Err(corrupt());
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut payload = encode_request(&Request::Stop);
        payload.push(0);
        assert!(decode_request(&payload).is_err());
        let mut payload = encode_payload(&Message::Bye);
        payload.push(0);
        assert!(decode_message(&payload).is_err());
    }
    #[test]
    fn realtime_checkpoint_roundtrip() {
        let message = Message::Events(vec![MonitorEvent::Checkpoint {
            disk: 'C',
            mark: JournalMark {
                journal_id: 42,
                last_usn: 100,
            },
        }]);
        assert_eq!(roundtrip(&message), message);
    }
}
