//! USN 日志的两个入口：**重放**（把一段历史补齐）与**实时跟随**（阻塞等新记录）。
//!
//! 两者都只产出结构化事件（[`MonitorEvent`]），不带路径 —— 索引按 FRN 组织，
//! 路径是否命中忽略规则由拿着索引的调用方去判（`docs/search-engine.md` §6）。
//!
//! 事件按批交付（一次 `FSCTL_READ_USN_JOURNAL` 返回多少条就是一批）：
//! 跨进程发送与落索引都有固定开销，逐条交付会把这些开销放大几万倍。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::System::Ioctl::USN_JOURNAL_DATA_V0;

use super::index::JournalMark;
use super::win::cancel_synchronous_io;
use super::win::current_thread_id;
use super::win::usn::{
    USN_REASON_CLOSE, USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE, USN_REASON_RENAME_NEW_NAME,
    USN_REASON_RENAME_OLD_NAME, UsnRecord,
};
use super::win::volume::Volume;
use super::{CoreError, Result};

/// 事件汇：一批事件交付一次。返回 `Err` 表示下游已经没了（管道断开等），
/// 跟随线程会据此退出。
pub type EventSink = Arc<dyn Fn(&[MonitorEvent]) -> Result<()> + Send + Sync>;

pub use super::model::MonitorEvent;

// ---------- 重放 ----------

/// 重放一个盘的结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayOutcome {
    /// 补齐成功；`mark` 是新的位点，可以直接交给实时跟随
    Replayed { events: u64, mark: JournalMark },
    /// 位点在这份日志里已经没有意义（日志被重建、或已被覆盖）→ 只能全量重扫
    NeedsRebuild,
}

/// 从 `mark` 把 USN 日志重放到当前末尾。
///
/// `Err` 一律视为「这个盘现在碰不了」（多半是非管理员或卷出问题），
/// 调用方应当保留已有索引、只把结果标记为可能滞后。
pub fn replay_disk(
    disk: char,
    mark: JournalMark,
    identity: Option<&super::model::VolumeInfo>,
    on_progress: &mut dyn FnMut(u64),
    on_batch: &mut dyn FnMut(&[MonitorEvent]) -> Result<()>,
) -> Result<ReplayOutcome> {
    let volume = match identity {
        Some(identity) => Volume::open_identity(identity)?,
        None => Volume::open(disk)?,
    };
    let journal = volume.query_journal()?;

    // 日志被重建过 → 位点对新日志没有意义
    if journal.UsnJournalID != mark.journal_id {
        return Ok(ReplayOutcome::NeedsRebuild);
    }
    // 日志已经跑满一圈、把位点覆盖掉了 → 中间的变更找不回来了
    if mark.last_usn < journal.FirstUsn || mark.last_usn > journal.NextUsn {
        return Ok(ReplayOutcome::NeedsRebuild);
    }

    let target = journal.NextUsn;
    let mut cursor = mark.last_usn;
    let mut seen = 0u64;

    // 一批一批读：内存不会因为「积了半年的变更」而爆掉
    while cursor < target {
        let mut batch: Vec<MonitorEvent> = Vec::new();
        let (next, count) =
            volume.read_usn_records(journal.UsnJournalID, cursor, false, |record| {
                if let Some(event) = record_to_event(disk, &record) {
                    batch.push(event);
                }
            })?;
        // 没有前进就说明读完了（或日志在这一刻被清空），防死循环
        if next <= cursor {
            return Ok(ReplayOutcome::NeedsRebuild);
        }
        cursor = next;
        on_batch(&batch)?;
        seen += u64::from(count);
        on_progress(seen);
    }

    // 重放是幂等的（按 FRN upsert / remove），位点略微重叠也无害
    Ok(ReplayOutcome::Replayed {
        events: seen,
        mark: JournalMark {
            last_usn: cursor.max(target),
            journal_id: journal.UsnJournalID,
        },
    })
}

// ---------- 实时跟随 ----------

/// 跟随参数
#[derive(Debug, Clone, Default)]
pub struct MonitorConfig {
    pub volumes: HashMap<char, super::model::VolumeInfo>,
    pub disks: Vec<char>,
    /// 起始位点。
    ///
    /// 有就**从这个位点接着读**：重放/扫描结束到线程真正开始读之间总有几百微秒，
    /// 直接从 `NextUsn` 起读会把这期间的变更整段丢掉。没有位点才从当前末尾开始。
    pub start: HashMap<char, JournalMark>,
}

impl MonitorConfig {
    pub fn new(disks: Vec<char>) -> Self {
        Self {
            disks,
            volumes: HashMap::new(),
            start: HashMap::new(),
        }
    }

    pub fn with_start(mut self, start: HashMap<char, JournalMark>) -> Self {
        self.start = start;
        self
    }
}

/// 一组跟随线程。`Drop` 会请求停止，但要真正回收线程仍需 [`MonitorHandle::join`]。
pub struct MonitorHandle {
    stop: Arc<AtomicBool>,
    thread_ids: Arc<Mutex<Vec<u32>>>,
    joins: Vec<JoinHandle<()>>,
}

impl MonitorHandle {
    /// 为每个盘起一条跟随线程。
    ///
    /// 返回 `(句柄, 成功跟上的盘数)`：卷打不开的盘会先报一条 `DiskWarning` 再跳过，
    /// 不影响其它盘。`live == 0` 意味着「一个盘都盯不上」，调用方据此把状态标成
    /// 「索引可用但没有实时增量」。
    pub fn start(config: &MonitorConfig, sink: EventSink) -> Result<(Self, usize)> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_ids = Arc::new(Mutex::new(Vec::new()));
        let mut joins = Vec::with_capacity(config.disks.len());
        let mut live = 0usize;

        for disk in &config.disks {
            let disk = disk.to_ascii_uppercase();
            // 在起线程之前就把卷打开：这样「跟不跟得上」是确定的，不用等线程回报
            let opened = match config.volumes.get(&disk) {
                Some(identity) => Volume::open_identity(identity),
                None => Volume::open(disk),
            }
            .and_then(|volume| volume.ensure_journal().map(|journal| (volume, journal)));
            let (volume, journal) = match opened {
                Ok(pair) => pair,
                Err(err) => {
                    let _ = sink(&[MonitorEvent::DiskWarning {
                        disk,
                        message: err.to_string(),
                    }]);
                    continue;
                }
            };

            let start = config.start.get(&disk).copied();
            let stop = Arc::clone(&stop);
            let thread_ids = Arc::clone(&thread_ids);
            let sink = Arc::clone(&sink);
            joins.push(
                thread::Builder::new()
                    .name(format!("rastflow-usn-{disk}"))
                    .spawn(move || {
                        follow_volume(disk, volume, journal, start, &stop, &thread_ids, &sink)
                    })
                    .map_err(CoreError::Io)?,
            );
            live += 1;
        }

        Ok((
            Self {
                stop,
                thread_ids,
                joins,
            },
            live,
        ))
    }

    /// 请求停止：置标志，并取消阻塞中的同步 IO。
    ///
    /// 取消要重试几轮（每轮 20ms）：线程可能刚通过「检查停止标志」、还没来得及
    /// 真正阻塞，那一次取消是空操作。
    pub fn stop(&self) {
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
            if self.joins.iter().all(|join| join.is_finished()) || Instant::now() >= deadline {
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

/// 一条跟随线程的主体：阻塞读日志，把每批事件交给 `sink`。
fn follow_volume(
    disk: char,
    volume: Volume,
    journal: USN_JOURNAL_DATA_V0,
    start: Option<JournalMark>,
    stop: &AtomicBool,
    thread_ids: &Mutex<Vec<u32>>,
    sink: &EventSink,
) {
    let journal_id = journal.UsnJournalID;
    // 有有效位点就接着读；位点失效（日志被重建）则只能从现在开始
    let mut last_usn = match start {
        Some(mark)
            if mark.journal_id == journal.UsnJournalID
                && mark.last_usn >= journal.FirstUsn
                && mark.last_usn <= journal.NextUsn =>
        {
            mark.last_usn
        }
        _ => {
            let _ = sink(&[
                MonitorEvent::DiskWarning {
                    disk,
                    message: "USN 位点失效，需要重新同步".into(),
                },
                MonitorEvent::DiskStopped(disk),
            ]);
            return;
        }
    };

    // 先登记线程 id，再复查停止标志，最后才进入可能阻塞的循环
    let thread_id = current_thread_id();
    match thread_ids.lock() {
        Ok(mut guard) => guard.push(thread_id),
        Err(poisoned) => poisoned.into_inner().push(thread_id),
    }
    if stop.load(Ordering::SeqCst) {
        return;
    }

    while !stop.load(Ordering::SeqCst) {
        let mut batch: Vec<MonitorEvent> = Vec::new();
        // wait = true → BytesToWaitFor = 1，没有新记录就阻塞在这里
        let outcome = volume.read_usn_records(journal_id, last_usn, true, |record| {
            if let Some(event) = record_to_event(disk, &record) {
                batch.push(event);
            }
        });

        match outcome {
            Ok((next, _)) => {
                if next > last_usn {
                    last_usn = next;
                }
                batch.push(MonitorEvent::Checkpoint {
                    disk,
                    mark: JournalMark {
                        journal_id,
                        last_usn,
                    },
                });
                if sink(&batch).is_err() {
                    break; // 下游没了（管道断开），没必要继续读
                }
            }
            Err(error) => {
                // 被 CancelSynchronousIo 打断是正常的停止路径
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let _ = sink(&[MonitorEvent::DiskWarning {
                    disk,
                    message: error.to_string(),
                }]);
                thread::sleep(Duration::from_millis(500));

                // 日志可能被重建（别的程序重建了、或卷被 remount），重新取一次
                match volume.ensure_journal() {
                    Ok(refreshed) => {
                        if refreshed.UsnJournalID != journal_id
                            || last_usn < refreshed.FirstUsn
                            || last_usn > refreshed.NextUsn
                        {
                            break; // 由客户端从检查点重新补读，失效时重建，不能跳到日志末尾。
                        }
                    }
                    Err(inner) => {
                        let _ = sink(&[MonitorEvent::DiskWarning {
                            disk,
                            message: inner.to_string(),
                        }]);
                        thread::sleep(Duration::from_millis(500));
                    }
                }
            }
        }
    }

    let _ = sink(&[MonitorEvent::DiskStopped(disk)]);
}

/// 把一条 USN 记录翻译成事件；返回 `None` 表示这条记录与索引无关。
///
/// 自由函数，供实时监控与日志重放共用。不做路径级过滤（回收站、忽略目录），
/// 那些由调用方拿着索引去做。
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
    if reason & USN_REASON_RENAME_OLD_NAME != 0 && reason & USN_REASON_RENAME_NEW_NAME == 0 {
        return Some(MonitorEvent::Removed {
            disk,
            frn: record.frn,
            parent: record.parent_frn,
            name: record.name_lossy(),
        });
    }

    // 删除：要求 CLOSE 同时成立，避免「打开后删除」被重复上报
    if reason & USN_REASON_FILE_DELETE != 0 && reason & USN_REASON_CLOSE != 0 {
        return Some(MonitorEvent::Removed {
            disk,
            frn: record.frn,
            parent: record.parent_frn,
            name: record.name_lossy(),
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
        renamed,
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

    /// 走「记录 → 事件」这一步（监控与重放共用同一条翻译）
    fn event_of(reason: u32, name: &str) -> Option<MonitorEvent> {
        let buf = record(reason, name);
        let parsed = UsnRecord::parse(&buf, 0).unwrap();
        record_to_event('C', &parsed)
    }

    #[test]
    fn creation_is_reported_only_after_close() {
        // 只有 CREATE，没有 CLOSE → 不上报
        assert_eq!(event_of(USN_REASON_FILE_CREATE, "a.txt"), None);
        assert_eq!(
            event_of(USN_REASON_FILE_CREATE | USN_REASON_CLOSE, "a.txt"),
            Some(MonitorEvent::Added {
                disk: 'C',
                frn: 42,
                parent: 7,
                name: "a.txt".to_string(),
                is_dir: false,
                renamed: false,
            })
        );
    }

    #[test]
    fn create_then_delete_noise_is_dropped() {
        assert_eq!(
            event_of(
                USN_REASON_FILE_CREATE | USN_REASON_FILE_DELETE | USN_REASON_CLOSE,
                "noise.tmp",
            ),
            None
        );
    }

    #[test]
    fn deletion_is_reported_only_after_close() {
        assert_eq!(event_of(USN_REASON_FILE_DELETE, "a.txt"), None);
        // 删除事件必须带名字：硬链接文件删掉一个名字不代表整个文件没了
        assert_eq!(
            event_of(USN_REASON_FILE_DELETE | USN_REASON_CLOSE, "a.txt"),
            Some(MonitorEvent::Removed {
                disk: 'C',
                frn: 42,
                parent: 7,
                name: "a.txt".to_string()
            })
        );
    }

    #[test]
    fn rename_removes_exact_old_link_then_adds_new_name() {
        // 旧名：不报
        assert_eq!(
            event_of(USN_REASON_RENAME_OLD_NAME | USN_REASON_CLOSE, "old.txt"),
            Some(MonitorEvent::Removed {
                disk: 'C',
                frn: 42,
                parent: 7,
                name: "old.txt".into()
            })
        );
        // 新名：报一条 Added，而且带的是新名字、并标明是重命名
        match event_of(USN_REASON_RENAME_NEW_NAME | USN_REASON_CLOSE, "new.txt") {
            Some(MonitorEvent::Added {
                name, frn, renamed, ..
            }) => {
                assert_eq!(name, "new.txt");
                assert_eq!(frn, 42, "重命名前后 FRN 不变");
                assert!(renamed, "要标明这是重命名，索引才能就地改名");
            }
            other => panic!("应当是 Added，实际 {other:?}"),
        }
    }

    #[test]
    fn nameless_record_is_dropped() {
        assert_eq!(
            event_of(USN_REASON_FILE_CREATE | USN_REASON_CLOSE, ""),
            None
        );
    }

    #[test]
    fn monitor_config_keeps_start_marks() {
        let mark = JournalMark {
            last_usn: 99,
            journal_id: 7,
        };
        let config = MonitorConfig::new(vec!['C']).with_start(HashMap::from([('C', mark)]));
        assert_eq!(config.start.get(&'C'), Some(&mark));
    }
}
