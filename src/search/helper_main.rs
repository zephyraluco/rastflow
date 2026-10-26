//! Windows 索引服务：只做 MFT/USN，通过命名管道批量返回元数据。
//! 索引、快照与搜索由普通用户桌面进程持有；客户端断开后返回等待连接。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::Result;
use super::helper::{self, Message, PipeReader, PipeWriter, Request};
use super::index::{self, IndexProgress, JournalMark, MftEntry, ScanSink};
use super::monitor::{self, EventSink, MonitorConfig, MonitorEvent, MonitorHandle};
use super::win::pipe::{self, PipeHandle};
use super::win::service;
use super::win::usn::UsnRecord;

/// 一批往主进程发多少条扫描记录。
///
/// 太小会让管道与解码开销占主导，太大会让「索引进度」跳得很粗；2048 条约 60 KB，
/// 一百多万文件也就几百条消息。
const RECORD_BATCH: usize = 2048;

/// 服务 accept 时每次轮询的时长。
///
/// `ConnectNamedPipe` 没有超时参数，只能靠 `CancelSynchronousIo` 打断，所以用
/// 短周期轮询来响应 SCM 的停止请求。服务空闲时每 2 秒一次，开销可以忽略。
const ACCEPT_POLL: Duration = Duration::from_secs(2);

/// 当前进程是不是被 SCM 当成索引服务拉起来的
pub fn is_service_invocation() -> bool {
    std::env::args()
        .skip(1)
        .any(|arg| arg == service::SERVICE_FLAG)
}

/// 当前进程是不是在执行「安装/卸载索引服务」
pub fn service_admin_action() -> Option<bool> {
    let mut install = None;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            value if value == service::INSTALL_FLAG => install = Some(true),
            value if value == service::UNINSTALL_FLAG => install = Some(false),
            _ => {}
        }
    }
    install
}

/// 服务模式的主循环：反复接受主进程的连接，一次只服务一个会话。
///
/// 服务是长期存在的，所以这里**不**退出：会话结束（客户端断开或发 `Stop`）后
/// 回到 accept，等下一次连接。`stop` 由服务控制处理器置位。
///
/// `pipe_name` 由调用方给：服务用固定的 [`service::PIPE_NAME`]，测试用临时名字，
/// 免得与机器上真装着的服务抢同一个管道。
pub fn run_service_loop(stop: &AtomicBool, pipe_name: &str) {
    service::report_running();
    eprintln!("[rastflow-index-service] 已就绪，等待主进程连接");

    while !stop.load(Ordering::SeqCst) {
        // 每次 accept 都用**新**实例：超时后旧实例可能卡在内核里，
        // 复用它会让下一次 accept 立刻返回莫名其妙的错误
        let server = match PipeHandle::create_server_multi(pipe_name) {
            Ok(server) => server,
            Err(err) => {
                eprintln!("[rastflow-index-service] 建管道失败：{err}");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };

        match pipe::accept(&server, ACCEPT_POLL) {
            Ok(()) => {}
            // 超时是常态（没人连），回头再检查一次停止标志
            Err(_) => continue,
        }

        // 会话期间才有意义：拒绝远程客户端已经由 PIPE_REJECT_REMOTE_CLIENTS 保证
        let reader = PipeReader::new(Arc::clone(&server));
        let writer = Arc::new(PipeWriter::new(Arc::clone(&server)));
        if let Err(err) = serve_session(&reader, &writer, Some(stop)) {
            eprintln!("[rastflow-index-service] 会话结束：{err}");
        }
        // `server` 在这里被 drop → 断开并关闭管道实例，下一次循环重建
    }

    eprintln!("[rastflow-index-service] 收到停止请求，正在退出");
}

/// 一个完整会话：握手 → 处理指令 → 直到 `Stop`、服务停止或客户端断开。
fn serve_session(
    reader: &PipeReader,
    writer: &Arc<PipeWriter>,
    service_stop: Option<&AtomicBool>,
) -> Result<()> {
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
            Request::Scan { disks } => scan_all(writer, &disks, &mut known)?,
            Request::CatchUp { marks } => catch_up_all(writer, &marks, &mut known)?,
            Request::Monitor => {
                follow(writer, reader, &known, service_stop)?;
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
        let outcome = monitor::replay_disk(*disk, *mark, &mut |_| {}, &mut |batch| {
            let _ = writer.send(&Message::Events(batch.to_vec()));
        });
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
    service_stop: Option<&AtomicBool>,
) -> Result<()> {
    let sink_writer = Arc::clone(writer);
    let sink: EventSink =
        Arc::new(move |batch: &[MonitorEvent]| sink_writer.send(&Message::Events(batch.to_vec())));

    let disks: Vec<char> = known.keys().copied().collect();
    let config = MonitorConfig::new(disks).with_start(known.clone());
    let (handle, live) = MonitorHandle::start(&config, sink)?;
    writer.send(&Message::Monitoring {
        monitoring: live > 0,
    })?;

    // 只有 `Stop`（或主进程断开、服务被停）能让它停下来。跟随线程自己跑在别的
    // 线程上，所以这里阻塞读不会影响它们。
    loop {
        if service_stop.is_some_and(|stop| stop.load(Ordering::SeqCst)) {
            // `sc stop`：客户端还连着，但我们必须结束会话。
            // 主线程的阻塞读已经被控制处理器用 `CancelSynchronousIo` 打断，
            // 这里的判断是双保险（也可能正好在刚读完一批的时候走到这里）。
            break;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::helper::{Message, PipeReader, PipeWriter, Request};
    use std::sync::atomic::AtomicBool;

    /// 服务循环是「长期 accept + 每连接一个会话」，这条测试走两轮会话，
    /// 验证它真的回到了 accept 而不是一次性退出。
    ///
    /// 管道不需要管理员，会话里也只发 `Monitor`（没有任何盘），所以这条用例
    /// 可以在普通权限下跑，不碰任何卷。
    #[test]
    fn service_loop_serves_rounds_and_stops() {
        let pipe_name = format!(
            r"\\.\pipe\rastflow-test-service-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let stop = Arc::clone(&stop);
            let name = pipe_name.clone();
            std::thread::spawn(move || run_service_loop(&stop, &name))
        };

        for round in 0..2 {
            let handle = PipeHandle::connect(&pipe_name, Duration::from_secs(10))
                .unwrap_or_else(|err| panic!("第 {round} 轮连不上服务：{err}"));
            let reader = PipeReader::new(Arc::clone(&handle));
            let writer = PipeWriter::new(handle);

            match reader.recv().expect("握手消息应当读得到") {
                Some(Message::Ready { version }) => assert_eq!(version, helper::PROTOCOL_VERSION),
                other => panic!("第 {round} 轮握手消息不对：{other:?}"),
            }

            writer.send_request(&Request::Monitor).expect("发 Monitor");
            match reader.recv().expect("应当收到 Monitoring") {
                // 没有盘 → 一个跟随线程都起不来
                Some(Message::Monitoring { monitoring }) => assert!(!monitoring),
                other => panic!("第 {round} 轮应当是 Monitoring，实际 {other:?}"),
            }

            writer.send_request(&Request::Stop).expect("发 Stop");
            // 客户端松手，服务应当回到 accept 等下一轮
        }

        stop.store(true, Ordering::SeqCst);
        worker.join().expect("服务循环应当正常退出");
    }
}
