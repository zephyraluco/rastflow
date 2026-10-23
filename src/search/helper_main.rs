//! 提权索引子进程：**只做 MFT/USN**，不持有索引、不落地、不搜索。
//!
//! 主进程用同一个可执行文件 + `--index-helper` 把自己拉起来（`runas`），
//! 通过命名管道下指令、收记录与事件。这样「打开卷句柄」这件需要管理员的事
//! 被关进这个小进程里，主程序可以以普通用户身份常驻托盘 —— 和 Everything
//! 用后台服务替 GUI 建库是同一个思路，只是这里不需要装服务。
//!
//! 三条退出路径都必须干净：主进程发 `Stop`、管道断开、父进程消失（[`watch_parent`]）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::helper::{self, Message, PipeReader, PipeWriter, Request, HELPER_FLAG};
use super::index::{self, IndexProgress, JournalMark, MftEntry, ScanSink};
use super::monitor::{self, EventSink, MonitorConfig, MonitorEvent, MonitorHandle};
use super::win::pipe::PipeHandle;
use super::win::process;
use super::win::usn::UsnRecord;
use super::{CoreError, Result};

/// 一批往主进程发多少条扫描记录。
///
/// 太小会让管道与解码开销占主导，太大会让「索引进度」跳得很粗；2048 条约 60 KB，
/// 一百多万文件也就几百条消息。
const RECORD_BATCH: usize = 2048;

/// 连接主进程管道的超时（主进程要先建好服务端）
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 当前进程是不是被当成索引子进程拉起来的
pub fn is_invocation() -> bool {
    std::env::args().skip(1).any(|arg| arg == HELPER_FLAG)
}

/// 子进程入口。返回进程退出码。
pub fn run() -> i32 {
    match run_inner() {
        Ok(()) => 0,
        Err(err) => {
            // 子进程没有控制台（windows_subsystem = "windows"），这条日志只在
            // 从终端手动启动时看得到；主进程那边的现象是「管道断开」
            eprintln!("[rastflow-index-helper] {err}");
            1
        }
    }
}

fn run_inner() -> Result<()> {
    let options = Options::parse()
        .ok_or_else(|| CoreError::InvalidArgument("索引子进程参数不完整".to_string()))?;

    // 父进程不管怎么退出（包括崩溃），都不能留下一个提权的孤儿进程
    process::exit_with_parent(options.parent_pid);

    let handle = PipeHandle::connect(&options.pipe, CONNECT_TIMEOUT)?;
    let writer = Arc::new(PipeWriter::new(Arc::clone(&handle)));
    let reader = PipeReader::new(handle);

    writer.send(&Message::Ready {
        version: helper::PROTOCOL_VERSION,
    })?;

    // 每个盘「已经读到哪」：扫描与重放都会更新它，实时跟随从这里接着读
    let mut known: HashMap<char, JournalMark> = HashMap::new();

    loop {
        let Some(request) = reader.recv_request()? else {
            return Ok(()); // 主进程没了
        };
        match request {
            Request::Scan { disks } => scan_all(&writer, &disks, &mut known)?,
            Request::CatchUp { marks } => catch_up_all(&writer, &marks, &mut known)?,
            Request::Monitor => {
                follow(&writer, &reader, &known)?;
                return Ok(());
            }
            Request::Stop => return Ok(()),
        }
    }
}

// ---------- 全量扫描 ----------

/// 把一条记录批次化后发给主进程
struct PipeScanSink<'a> {
    writer: &'a PipeWriter,
    disk: char,
    batch: Vec<MftEntry>,
}

impl PipeScanSink<'_> {
    fn flush(&mut self) {
        if self.batch.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut self.batch);
        // 发送失败（管道断开）不用在这里报错：扫描结束后发 `ScanFinished` 会失败，
        // 那条错误会把整轮扫描带出去
        let _ = self.writer.send(&Message::Records(batch));
    }
}

impl ScanSink for PipeScanSink<'_> {
    fn on_start(&mut self, disk: char, mark: JournalMark) {
        self.disk = disk;
        let _ = self.writer.send(&Message::ScanStarted { disk, mark });
    }

    fn on_record(&mut self, record: &UsnRecord<'_>) {
        if let Some(entry) = MftEntry::from_record(self.disk, record) {
            self.batch.push(entry);
            if self.batch.len() >= RECORD_BATCH {
                self.flush();
            }
        }
    }

    /// 进度按批走，不必每条都往主进程报
    fn on_progress(&mut self, _progress: IndexProgress) {}
}

fn scan_all(
    writer: &PipeWriter,
    disks: &[char],
    known: &mut HashMap<char, JournalMark>,
) -> Result<()> {
    for disk in disks {
        let mut sink = PipeScanSink {
            writer,
            disk: *disk,
            batch: Vec::with_capacity(RECORD_BATCH),
        };
        match index::scan_disk(*disk, &mut sink) {
            Ok(outcome) => {
                sink.flush();
                known.insert(*disk, outcome.mark);
                writer.send(&Message::ScanFinished {
                    disk: *disk,
                    records: outcome.records,
                })?;
            }
            Err(err) => {
                writer.send(&Message::Unavailable {
                    disk: *disk,
                    reason: err.to_string(),
                })?;
            }
        }
    }
    Ok(())
}

// ---------- 日志重放 ----------

fn catch_up_all(
    writer: &PipeWriter,
    marks: &[(char, JournalMark)],
    known: &mut HashMap<char, JournalMark>,
) -> Result<()> {
    for (disk, mark) in marks {
        let outcome = monitor::replay_disk(
            *disk,
            *mark,
            &mut |_| {},
            &mut |batch| {
                let _ = writer.send(&Message::Events(batch.to_vec()));
            },
        );
        match outcome {
            Ok(monitor::ReplayOutcome::Replayed { events, mark }) => {
                known.insert(*disk, mark);
                writer.send(&Message::CatchUpFinished {
                    disk: *disk,
                    events,
                    mark,
                })?;
            }
            Ok(monitor::ReplayOutcome::NeedsRebuild) => {
                writer.send(&Message::NeedsRebuild { disk: *disk })?;
            }
            Err(err) => {
                writer.send(&Message::Unavailable {
                    disk: *disk,
                    reason: err.to_string(),
                })?;
            }
        }
    }
    Ok(())
}

// ---------- 实时跟随 ----------

fn follow(
    writer: &Arc<PipeWriter>,
    reader: &PipeReader,
    known: &HashMap<char, JournalMark>,
) -> Result<()> {
    let sink_writer = Arc::clone(writer);
    let sink: EventSink = Arc::new(move |batch: &[MonitorEvent]| {
        sink_writer.send(&Message::Events(batch.to_vec()))
    });

    let disks: Vec<char> = known.keys().copied().collect();
    let config = MonitorConfig::new(disks).with_start(known.clone());
    let (handle, live) = MonitorHandle::start(&config, sink)?;
    writer.send(&Message::Monitoring { monitoring: live > 0 })?;

    // 只有 `Stop`（或主进程断开）能让它停下来。跟随线程自己跑在别的线程上，
    // 所以这里阻塞读不会影响它们。
    loop {
        match reader.recv_request()? {
            Some(Request::Stop) | None => break,
            // 跟随期间不再接受扫描/重放：主进程要重建会先 Stop 再重新开会话
            Some(_) => {}
        }
    }

    handle.join();
    let _ = writer.send(&Message::Bye);
    Ok(())
}

// ---------- 参数 ----------

struct Options {
    pipe: String,
    parent_pid: u32,
}

impl Options {
    fn parse() -> Option<Self> {
        let mut pipe = None;
        let mut parent_pid = None;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--pipe" => pipe = args.next(),
                "--parent-pid" => parent_pid = args.next().and_then(|value| value.parse().ok()),
                _ => {}
            }
        }
        Some(Self {
            pipe: pipe?,
            parent_pid: parent_pid?,
        })
    }
}
