//! 实时增量监控：每个盘一个常驻线程，阻塞读 USN 日志，产出结构化的变更事件。
//!
//! 线程阻塞在 `FSCTL_READ_USN_JOURNAL` 上（`BytesToWaitFor = 1`），有新记录才返回。
//! 从 `NextUsn`（当前末尾）开始读，不回溯历史。
//!
//! 事件过滤：`FILE_CREATE|FILE_DELETE` 同时出现则跳过；删除要求 `FILE_DELETE|CLOSE`
//! 同时成立；重命名只采用「新名」那条记录。
//!
//! 事件带 `(FRN, 父 FRN, 名字, 是不是目录)`，不带路径。回收站过滤在 [`super::engine`]。
//!
//! 停止用 `CancelSynchronousIo` 打断阻塞中的读取（[`super::win::cancel_synchronous_io`]）。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::win::cancel_synchronous_io;
use super::win::usn::{
    USN_REASON_CLOSE, USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE, USN_REASON_RENAME_NEW_NAME,
    UsnRecord,
};
use super::win::volume::Volume;
use super::Result;

/// 监控线程抛出的事件
///
/// 事件内容是**结构化的**，不是完整路径：内存索引按 FRN 组织，
/// 有这几个字段就能直接落进索引，不需要先拼路径（见模块文档）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorEvent {
    /// 新增，或重命名/移动之后的**新样子**。
    ///
    /// 落到索引上是幂等 upsert：同一个 FRN 再收到一次就只是刷新名字与父目录。
    Added {
        disk: char,
        /// 文件引用号 `(序列号 << 48) | 记录号`
        frn: u64,
        /// 父目录的 FRN
        parent: u64,
        /// 文件名（不含路径）
        name: String,
        /// 是不是目录
        is_dir: bool,
    },
    /// 已删除（或已移出本盘）
    Removed { disk: char, frn: u64 },
    /// 某个盘出问题（打不开卷、日志不可用等），线程会继续重试
    DiskWarning { disk: char, message: String },
    /// 某个盘的监控线程已退出
    DiskStopped(char),
}

/// 监控配置
#[derive(Debug, Clone)]
pub struct MonitorConfig {
    pub disks: Vec<char>,
}

impl MonitorConfig {
    pub fn new(disks: Vec<char>) -> Self {
        Self { disks }
    }
}

/// 监控句柄：持有事件接收端与停止控制
pub struct MonitorHandle {
    stop: Arc<AtomicBool>,
    thread_ids: Arc<Mutex<Vec<u32>>>,
    live: Arc<AtomicUsize>,
    joins: Vec<JoinHandle<()>>,
    events: Receiver<MonitorEvent>,
}

impl MonitorHandle {
    /// 启动所有盘的监控线程
    pub fn start(config: &MonitorConfig) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_ids = Arc::new(Mutex::new(Vec::new()));
        let live = Arc::new(AtomicUsize::new(0));
        let (sender, events) = mpsc::channel();

        let mut joins = Vec::with_capacity(config.disks.len());
        for disk in &config.disks {
            let disk = disk.to_ascii_uppercase();
            let stop = Arc::clone(&stop);
            let thread_ids = Arc::clone(&thread_ids);
            let live = Arc::clone(&live);
            let sender = sender.clone();
            joins.push(
                thread::Builder::new()
                    .name(format!("rastflow-core-monitor-{disk}"))
                    .spawn(move || {
                        live.fetch_add(1, Ordering::SeqCst);
                        if let Err(error) = run_disk(disk, &stop, &thread_ids, &sender) {
                            let _ = sender.send(MonitorEvent::DiskWarning {
                                disk,
                                message: error.to_string(),
                            });
                        }
                        let _ = sender.send(MonitorEvent::DiskStopped(disk));
                        live.fetch_sub(1, Ordering::SeqCst);
                    })
                    .map_err(super::CoreError::Io)?,
            );
        }
        drop(sender);

        Ok(Self {
            stop,
            thread_ids,
            live,
            joins,
            events,
        })
    }

    /// 带超时取一条事件。
    ///
    /// ⚠️ `Err(Disconnected)` 表示所有盘的监控线程都已退出，不会再有事件到来。
    /// 这时它会**不等待**地反复返回 `Disconnected`，轮询方必须据此停下，
    /// 否则就是把 500 ms 的等待退化成空转。
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<MonitorEvent, RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }

    /// 请求停止：置标志，并取消阻塞中的同步 IO。
    ///
    /// 取消要重试几轮（每轮 20ms）：线程可能刚通过「检查停止标志」、还没来得及
    /// 真正阻塞，那一次取消是空操作。
    pub fn stop(&mut self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return; // 幂等
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let ids: Vec<u32> = match self.thread_ids.lock() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            };
            for thread_id in ids {
                // 线程可能已经退出，失败是正常的
                let _ = cancel_synchronous_io(thread_id);
            }
            if self.live.load(Ordering::SeqCst) == 0 || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// 停止并等待所有线程退出
    pub fn join(mut self) {
        self.stop();
        for handle in self.joins.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for MonitorHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 一个盘的监控线程主体
fn run_disk(
    disk: char,
    stop: &AtomicBool,
    thread_ids: &Mutex<Vec<u32>>,
    events: &Sender<MonitorEvent>,
) -> Result<()> {
    let volume = Volume::open(disk)?;
    let journal = volume.ensure_journal()?;
    let mut watcher = DiskWatcher::new(
        disk,
        journal.UsnJournalID,
        journal.NextUsn,
        events.clone(),
    );

    // 先登记线程 id，再复查停止标志，最后才进入可能阻塞的循环
    let thread_id = super::win::current_thread_id();
    match thread_ids.lock() {
        Ok(mut guard) => guard.push(thread_id),
        Err(poisoned) => poisoned.into_inner().push(thread_id),
    }
    if stop.load(Ordering::SeqCst) {
        return Ok(());
    }

    while !stop.load(Ordering::SeqCst) {
        // wait = true → BytesToWaitFor = 1，没有新记录就阻塞在这里
        let outcome = volume.read_usn_records(
            watcher.journal_id,
            watcher.last_usn,
            true,
            |record| watcher.handle(record),
        );

        match outcome {
            Ok((next_usn, _)) => {
                if next_usn != watcher.last_usn {
                    watcher.last_usn = next_usn;
                }
            }
            Err(error) => {
                // 被 CancelSynchronousIo 打断是正常的停止路径
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let _ = events.send(MonitorEvent::DiskWarning {
                    disk,
                    message: error.to_string(),
                });
                thread::sleep(Duration::from_millis(500));

                // 日志可能被重建（索引被别的程序重建、或卷被 remount），
                // 重新取一次；位置倒退了就跟着退，避免读到「不存在」的区域
                match volume.ensure_journal() {
                    Ok(journal) => {
                        watcher.journal_id = journal.UsnJournalID;
                        if journal.NextUsn < watcher.last_usn {
                            watcher.last_usn = journal.NextUsn;
                        }
                    }
                    Err(inner) => {
                        let _ = events.send(MonitorEvent::DiskWarning {
                            disk,
                            message: inner.to_string(),
                        });
                        thread::sleep(Duration::from_millis(500));
                    }
                }
            }
        }
    }

    Ok(())
}

/// 一个盘的解析器：把 USN 记录翻译成增删事件。
///
/// 它**不认识路径**，也不认识索引 —— 只做过滤与字段搬运。
/// 路径与「该不该忽略」都由 [`super::engine`] 拿着索引去判。
struct DiskWatcher {
    disk: char,
    journal_id: u64,
    last_usn: i64,
    events: Sender<MonitorEvent>,
}

impl DiskWatcher {
    fn new(disk: char, journal_id: u64, last_usn: i64, events: Sender<MonitorEvent>) -> Self {
        Self {
            disk,
            journal_id,
            last_usn,
            events,
        }
    }

    /// 处理一条 USN 记录：翻译成事件后转发出去
    fn handle(&mut self, record: UsnRecord<'_>) {
        if let Some(event) = record_to_event(self.disk, &record) {
            let _ = self.events.send(event);
        }
    }
}

/// 把一条 USN 记录翻译成事件；返回 `None` 表示这条记录与索引无关。
///
/// 自由函数，供实时监控与启动时的日志重放共用。不做路径级过滤（回收站、忽略目录），
/// 那些在 [`super::engine`] 里做。
pub fn record_to_event(disk: char, record: &UsnRecord<'_>) -> Option<MonitorEvent> {
    let reason = record.reason;

    // 有些系统文件会「同时创建又删除」，这类噪音直接跳过
    if reason & USN_REASON_FILE_CREATE != 0 && reason & USN_REASON_FILE_DELETE != 0 {
        return None;
    }

    // 重命名：只采用「新名」。旧名那条记录里的名字已经过时，拿它建条目会写出错误的路径。
    //
    // Windows 通常把旧名/新名拆成两条记录，但这一点没有契约保证，所以判据写成
    // 「是重命名 且 不带新名」而不是「带旧名」—— 万一两条合在一起，也要按新名处理，
    // 不能把这次重命名整个丢掉。
    if record.is_rename() && reason & USN_REASON_RENAME_NEW_NAME == 0 {
        return None;
    }

    // 删除：要求 CLOSE 同时成立，避免「打开后删除」被重复上报
    if reason & USN_REASON_FILE_DELETE != 0 && reason & USN_REASON_CLOSE != 0 {
        return Some(MonitorEvent::Removed {
            disk,
            frn: record.frn,
        });
    }

    // 新增要等 CLOSE（拿到最终状态）；重命名看新名那条
    let created = reason & USN_REASON_FILE_CREATE != 0 && reason & USN_REASON_CLOSE != 0;
    let renamed = reason & USN_REASON_RENAME_NEW_NAME != 0;
    if !created && !renamed {
        return None;
    }

    if record.name_bytes.is_empty() {
        return None; // 盘根那条无名记录
    }
    Some(MonitorEvent::Added {
        disk,
        frn: record.frn,
        parent: record.parent_frn,
        name: record.name_lossy(),
        is_dir: record.is_directory(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::win::usn::USN_REASON_RENAME_OLD_NAME;
    /// 造一条只有 reason / 名字有意义的记录
    fn record(reason: u32, name: &str) -> Vec<u8> {
        let name_bytes: Vec<u8> = name
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        let record_len = 60 + name_bytes.len();
        let mut buf = vec![0u8; record_len];
        buf[0..4].copy_from_slice(&(record_len as u32).to_le_bytes());
        buf[8..16].copy_from_slice(&42u64.to_le_bytes()); // frn
        buf[16..24].copy_from_slice(&7u64.to_le_bytes()); // parent
        buf[40..44].copy_from_slice(&reason.to_le_bytes());
        buf[56..58].copy_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf[58..60].copy_from_slice(&60u16.to_le_bytes());
        buf[60..].copy_from_slice(&name_bytes);
        buf
    }

    fn watcher() -> (DiskWatcher, Receiver<MonitorEvent>) {
        let (sender, receiver) = mpsc::channel();
        (DiskWatcher::new('C', 1, 0, sender), receiver)
    }

    #[test]
    fn creation_is_reported_only_after_close() {
        let (mut watcher, events) = watcher();
        // 只有 CREATE，没有 CLOSE → 不上报
        let buf = record(USN_REASON_FILE_CREATE, "a.txt");
        watcher.handle(UsnRecord::parse(&buf, 0).unwrap());
        assert!(events.try_recv().is_err());

        let buf = record(USN_REASON_FILE_CREATE | USN_REASON_CLOSE, "a.txt");
        watcher.handle(UsnRecord::parse(&buf, 0).unwrap());
        let event = events.try_recv().unwrap();
        assert_eq!(
            event,
            MonitorEvent::Added {
                disk: 'C',
                frn: 42,
                parent: 7,
                name: "a.txt".to_string(),
                is_dir: false,
            }
        );
    }

    #[test]
    fn create_then_delete_noise_is_dropped() {
        let (mut watcher, events) = watcher();
        let buf = record(
            USN_REASON_FILE_CREATE | USN_REASON_FILE_DELETE | USN_REASON_CLOSE,
            "noise.tmp",
        );
        watcher.handle(UsnRecord::parse(&buf, 0).unwrap());
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn deletion_is_reported_only_after_close() {
        let (mut watcher, events) = watcher();
        let buf = record(USN_REASON_FILE_DELETE, "a.txt");
        watcher.handle(UsnRecord::parse(&buf, 0).unwrap());
        assert!(events.try_recv().is_err());

        let buf = record(USN_REASON_FILE_DELETE | USN_REASON_CLOSE, "a.txt");
        watcher.handle(UsnRecord::parse(&buf, 0).unwrap());
        assert_eq!(
            events.try_recv().unwrap(),
            MonitorEvent::Removed { disk: 'C', frn: 42 }
        );
    }

    #[test]
    fn rename_reports_new_name_only() {
        let (mut watcher, events) = watcher();
        // 旧名：不报
        let buf = record(USN_REASON_RENAME_OLD_NAME | USN_REASON_CLOSE, "old.txt");
        watcher.handle(UsnRecord::parse(&buf, 0).unwrap());
        assert!(events.try_recv().is_err());

        // 新名：报一条 Added，而且带的是新名字
        let buf = record(USN_REASON_RENAME_NEW_NAME | USN_REASON_CLOSE, "new.txt");
        watcher.handle(UsnRecord::parse(&buf, 0).unwrap());
        match events.try_recv().unwrap() {
            MonitorEvent::Added { name, frn, .. } => {
                assert_eq!(name, "new.txt");
                assert_eq!(frn, 42, "重命名前后 FRN 不变");
            }
            other => panic!("应当是 Added，实际 {other:?}"),
        }
    }

    #[test]
    fn nameless_record_is_dropped() {
        let (mut watcher, events) = watcher();
        let buf = record(USN_REASON_FILE_CREATE | USN_REASON_CLOSE, "");
        watcher.handle(UsnRecord::parse(&buf, 0).unwrap());
        assert!(events.try_recv().is_err());
    }
}
