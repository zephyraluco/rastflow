//! 搜索调度与索引生命周期。
//!
//! # 一次搜索
//!
//! 1. **优先目录预扫**：桌面 / 开始菜单 / 用户指定的「优先文件夹」直接遍历文件系统。
//! 2. **扫内存索引**：顺序扫一遍，只比文件名 —— 见 [`FileIndex`] 与
//!    [`super::matcher::SearchQuery::matches_name`]。
//! 3. **按优先级档发布**：程序类（`exe`/`lnk`）先出，然后是脚本、普通文件，最后是目录。
//!    分档在一次扫描里完成，每档都会被扫到，所以优先级只影响先后。
//!
//! 命中时顺手校验文件是否存在，不存在就从索引里摘掉（索引可能过期）。
//! ⚠️ 摘除必须等**读锁释放之后**再拿写锁 —— `RwLock` 不可重入。
//!
//! # 索引生命周期
//!
//! ```text
//! Engine::open      读单文件快照（有就用，没有就空着）
//!     ↓
//! Engine::catch_up  从快照里记的 USN 位点重放日志
//!     ↓             （位点被日志覆盖掉了 → NeedsRebuild，退回全量重扫）
//! Engine::start_monitor  实时增量
//!     ↓
//! Engine::save_snapshot  定期落盘（顺便压实名称池、收掉孤儿）
//! ```

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{
    Arc, Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
use std::thread;
use std::time::Duration;

use super::index::{
    self as index_mod, FileIndex, IndexConfig, IndexProgress, IndexReport, JournalMark, PathScratch,
    SaveStats,
};
use super::matcher::SearchQuery;
use super::monitor::{self, MonitorConfig, MonitorEvent, MonitorHandle};
use super::pathutil::{self, IgnoreRules};
use super::priority::{DIR_PRIORITY, PriorityTable};
use super::win::disk;
use super::win::known_folder;
use super::win::usn::is_root;
use super::win::volume::Volume;
use super::{DEFAULT_MAX_RESULTS, MAX_SEARCH_TEXT_LEN, Result};

/// 打开引擎所需的配置
#[derive(Clone, Debug)]
pub struct CoreConfig {
    /// 快照存放目录
    pub data_dir: PathBuf,
    /// 要索引的盘；留空表示自动探测所有「本地固定盘 + NTFS」
    pub disks: Vec<char>,
    /// 忽略的目录（整棵子树）
    pub ignore_paths: Vec<String>,
    /// 单次搜索最多返回多少条
    pub max_results: usize,
    /// 除开始菜单/桌面之外，额外优先预扫的目录
    pub priority_folders: Vec<PathBuf>,
    /// 预扫时递归的最大深度
    pub scan_depth: usize,
    /// 是否预扫开始菜单/桌面
    pub scan_shortcuts: bool,
}

impl CoreConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            disks: Vec::new(),
            ignore_paths: vec![
                // 默认忽略回收站：里面全是被删掉的东西，命中只会碍事
                r"C:\$Recycle.Bin".to_string(),
            ],
            max_results: DEFAULT_MAX_RESULTS,
            priority_folders: Vec::new(),
            scan_depth: 4,
            scan_shortcuts: true,
        }
    }
}

/// 启动时日志重放的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatchUp {
    /// 快照里没记位点（首次运行，或本来就是空索引）
    NoMark,
    /// 从位点补齐了这么多条变更
    Replayed { events: u64 },
    /// 位点已经失效（日志被重建或已被覆盖），只能全量重扫
    NeedsRebuild,
    /// 补不了（多半是非管理员打不开卷），索引会停留在快照时的状态
    Unavailable(String),
}

/// 搜索核心。需要放进 `Arc` 才能发起搜索（[`Engine::search`] 会起后台线程）。
pub struct Engine {
    config: CoreConfig,
    disks: Vec<char>,
    /// 常驻内存的索引。搜索持读锁，增量更新持写锁
    index: RwLock<FileIndex>,
    priorities: RwLock<PriorityTable>,
    /// 每个盘已经处理到的 USN 位点（随快照落盘）
    marks: Mutex<HashMap<char, JournalMark>>,
    /// 单文件快照的路径
    snapshot: PathBuf,
    /// 预编译的忽略规则
    ignore: IgnoreRules,
}

impl Engine {
    /// 打开引擎：读快照（有就用）、确定盘、准备优先级表。
    ///
    /// 打不开快照不算错误，重扫一次即可。
    pub fn open(config: CoreConfig) -> Result<Self> {
        let disks: Vec<char> = if config.disks.is_empty() {
            disk::available_disks()
        } else {
            config
                .disks
                .iter()
                .map(|disk| disk.to_ascii_uppercase())
                .collect()
        };
        for disk in &disks {
            // 显式配置了不可索引的盘就直接报错，便于定位问题
            disk::ensure_supported(*disk)?;
        }

        std::fs::create_dir_all(&config.data_dir)?;
        cleanup_legacy_index(&config.data_dir);
        let snapshot = index_mod::snapshot_path(&config.data_dir);
        let ignore = IgnoreRules::new(&config.ignore_paths);

        let (mut index, mut priorities, marks) = match FileIndex::load(&snapshot)? {
            Some(loaded) => (loaded.index, loaded.priorities, loaded.marks),
            None => (
                FileIndex::new(),
                PriorityTable::with_builtin_defaults(),
                HashMap::new(),
            ),
        };

        // 快照里可能有这次不再索引的盘（用户改了配置），把它们的条目清掉
        for disk in index.disks().collect::<Vec<char>>() {
            if !disks.contains(&disk) {
                index.clear_disk(disk);
            }
        }
        if priorities.is_empty() {
            // 首次启动用内置默认值，但**不落盘** —— 用户真正改设置时才写回去，
            // 这样「默认值」与「用户选择」在文件里始终区分得开
            priorities = PriorityTable::with_builtin_defaults();
        }

        Ok(Self {
            config,
            disks,
            index: RwLock::new(index),
            priorities: RwLock::new(priorities),
            marks: Mutex::new(marks),
            snapshot,
            ignore,
        })
    }

    pub fn disks(&self) -> &[char] {
        &self.disks
    }

    /// 是否已经有索引
    pub fn is_indexed(&self) -> bool {
        !read(&self.index).is_empty()
    }

    /// 重建索引。**会阻塞**，请放到后台线程里调用。完成后落一次快照。
    pub fn build_index(
        &self,
        progress: Option<&mut dyn FnMut(IndexProgress)>,
    ) -> Result<IndexReport> {
        let index_config =
            IndexConfig::new(self.disks.clone()).with_ignore_paths(self.config.ignore_paths.clone());
        let mut noop = |_: IndexProgress| {};
        let progress: &mut dyn FnMut(IndexProgress) = match progress {
            Some(callback) => callback,
            None => &mut noop,
        };

        let report = {
            let mut index = write(&self.index);
            index_mod::rebuild_all(&mut index, &read(&self.priorities), &index_config, progress)?
        };

        // 位点重新对齐到「现在」：旧位点对应的日志区间已经由这次全量扫描覆盖了
        let mut marks = lock(&self.marks);
        marks.clear();
        for disk in &self.disks {
            let Ok(volume) = Volume::open(*disk) else {
                continue;
            };
            let Ok(journal) = volume.ensure_journal() else {
                continue;
            };
            marks.insert(
                *disk,
                JournalMark {
                    last_usn: journal.NextUsn,
                    journal_id: journal.UsnJournalID,
                },
            );
        }
        drop(marks);

        let _ = self.save_snapshot();
        Ok(report)
    }

    /// 落一次快照：顺带压实名称池、收掉孤儿条目
    pub fn save_snapshot(&self) -> Result<SaveStats> {
        let priorities = read(&self.priorities).clone();
        let marks = lock(&self.marks).clone();
        let mut index = write(&self.index);
        index.prune_orphans();
        index.save(&self.snapshot, &priorities, &marks)
    }

    /// 启动时的「补课」：从快照里记的位点把日志重放到当前末尾。
    ///
    /// 每个盘独立判断，一个盘的日志没用了不会拖累其它盘。
    pub fn catch_up(&self, progress: &mut dyn FnMut(IndexProgress)) -> CatchUp {
        let marks = lock(&self.marks).clone();
        if marks.is_empty() {
            return CatchUp::NoMark;
        }

        let mut replayed = 0u64;
        for disk in self.disks.clone() {
            let Some(mark) = marks.get(&disk).copied() else {
                continue;
            };
            match self.catch_up_disk(disk, mark, progress) {
                Ok(CatchUp::Replayed { events }) => replayed += events,
                Ok(CatchUp::NeedsRebuild) => return CatchUp::NeedsRebuild,
                Ok(_) => {}
                Err(err) => return CatchUp::Unavailable(err.to_string()),
            }
        }
        CatchUp::Replayed { events: replayed }
    }

    fn catch_up_disk(
        &self,
        disk: char,
        mark: JournalMark,
        progress: &mut dyn FnMut(IndexProgress),
    ) -> Result<CatchUp> {
        let volume = Volume::open(disk)?;
        let journal = volume.query_journal()?;

        // 日志被重建过 → 位点对新日志没有意义
        if journal.UsnJournalID != mark.journal_id {
            return Ok(CatchUp::NeedsRebuild);
        }
        // 日志已经跑满一圈、把位点覆盖掉了 → 中间的变更找不回来了
        if mark.last_usn < journal.FirstUsn {
            return Ok(CatchUp::NeedsRebuild);
        }

        let target = journal.NextUsn;
        let mut cursor = mark.last_usn;
        let mut seen = 0u64;

        // 一批一批读：每次 read 返回一缓冲区的记录，处理完再接着读，
        // 这样内存不会因为「积了半年的变更」而爆掉
        while cursor < target {
            let mut events: Vec<MonitorEvent> = Vec::new();
            let (next, count) =
                volume.read_usn_records(journal.UsnJournalID, cursor, false, |record| {
                    if let Some(event) = monitor::record_to_event(disk, &record) {
                        events.push(event);
                    }
                })?;
            // 没有前进就说明读完了（或日志在这一刻被清空），防死循环
            if next <= cursor {
                break;
            }
            cursor = next;

            let mut index = write(&self.index);
            for event in &events {
                apply_to_index(&mut index, event, &self.ignore, &read(&self.priorities));
            }
            drop(index);

            seen += u64::from(count);
            progress(IndexProgress {
                disk,
                scanned: seen,
                written: seen,
            });
        }

        // 重放是幂等的（按 FRN upsert / remove），所以位点略微重叠也无害
        lock(&self.marks).insert(
            disk,
            JournalMark {
                last_usn: cursor.max(target),
                journal_id: journal.UsnJournalID,
            },
        );
        Ok(CatchUp::Replayed { events: seen })
    }

    /// 启动增量监控
    pub fn start_monitor(&self) -> Result<MonitorHandle> {
        MonitorHandle::start(&MonitorConfig::new(self.disks.clone()))
    }

    /// 把一条监控事件应用到索引（不重扫全盘，只改一条记录）。
    ///
    /// 返回 `Ok(true)` 表示确实改动了索引。日志重放也走这里。
    pub fn apply_event(&self, event: &MonitorEvent) -> Result<bool> {
        let mut index = write(&self.index);
        Ok(apply_to_index(
            &mut index,
            event,
            &self.ignore,
            &read(&self.priorities),
        ))
    }

    /// 发起一次搜索，立即返回（结果在后台线程里渐进式填充）
    pub fn search(self: &Arc<Self>, query: SearchQuery) -> SearchSession {
        let state = Arc::new(SessionState::default());
        let engine = Arc::clone(self);
        let worker_state = Arc::clone(&state);
        // 发射后不管：结果由 `state` 渐进式发布，取消靠 `state.cancelled`。
        // 不保留 JoinHandle —— 会话销毁时 Drop 会置取消位，扫描线程很快自行退出，
        // 没有任何调用方需要 join 它。
        let _ = thread::Builder::new()
            .name("rastflow-core-search".to_string())
            .spawn(move || {
                engine.run_search(&query, &worker_state);
                worker_state.finish();
            });

        SearchSession { state }
    }

    // ── 搜索主体 ───────────────────────────────────────────────────────

    fn run_search(&self, query: &SearchQuery, state: &SessionState) {
        // 过长或为空的查询直接结束，不必白扫一遍索引
        if query.is_empty() || query.search_text.len() > MAX_SEARCH_TEXT_LEN {
            return;
        }
        let max = self.config.max_results;

        // ── 第一阶段：预扫「几乎一定要用」的目录 ───────────────────────
        for root in self.preset_roots() {
            if state.should_stop() || state.len() >= max {
                return;
            }
            self.scan_folder(&root, query, state, 0);
        }

        // ── 第二阶段：扫内存索引 ──────────────────────────────────────
        let mut ghosts: Vec<u32> = Vec::new();
        {
            let index = read(&self.index);
            self.scan_index(&index, query, state, &mut ghosts);
        }

        // 懒删除：命中了但文件已经不在 → 从索引里摘掉。
        // 必须等读锁释放后再拿写锁 —— `RwLock` 不可重入。
        if !ghosts.is_empty() {
            let mut index = write(&self.index);
            for slot in ghosts {
                index.discard(slot);
            }
        }
    }

    /// 结果分档顺序：文件按后缀优先级降序，最后才是目录
    fn priority_tiers(&self) -> Vec<i32> {
        let mut tiers = read(&self.priorities).priorities();
        tiers.push(DIR_PRIORITY);
        tiers
    }

    fn scan_index(
        &self,
        index: &FileIndex,
        query: &SearchQuery,
        state: &SessionState,
        ghosts: &mut Vec<u32>,
    ) {
        let max = self.config.max_results;
        let tiers = self.priority_tiers();
        let last_tier = tiers.len() - 1;
        let needs_dir = query.needs_dir_for_match();

        // 一档一个桶，每档最多收 max 条
        let mut buckets: Vec<Vec<u32>> = tiers.iter().map(|_| Vec::new()).collect();

        let mut scratch = PathScratch::new();
        let mut dir_buf = String::new();

        for (slot, entry) in index.iter_live() {
            if state.should_stop() {
                return;
            }
            let Some(name_bytes) = index.name_bytes(slot) else {
                continue;
            };

            if needs_dir {
                // 含路径关键字的查询：必须拼出完整路径才能判。
                //
                // 这类查询很少见，代价可以接受 —— 实测 120 万条扫一遍：
                // 纯文件名关键字约 25ms，带路径关键字约 **400ms**（慢 16 倍，
                // 因为每条候选都要走一遍祖先链）。所以别拿它去扫整个索引，
                // 但也不必为它换写法：用户很少在启动器里敲反斜杠。
                let Some(parent) = index.parent_of(entry) else {
                    continue;
                };
                dir_buf.clear();
                if !index.path_into(parent, &mut scratch, &mut dir_buf) {
                    continue;
                }
                let name = std::str::from_utf8(name_bytes).unwrap_or("");
                if !query.matches_parts(name, &dir_buf) {
                    continue;
                }
            } else if !query.matches_name(name_bytes) {
                continue;
            }

            // 表里没有的优先级（用户改过设置但没重算索引）落到最后一档：
            // 排得靠后可以，漏结果不行
            let tier = tiers
                .iter()
                .position(|value| *value == entry.priority)
                .unwrap_or(last_tier);
            if buckets[tier].len() < max {
                buckets[tier].push(slot);
            }
        }

        // ── 按档发布 ──
        for bucket in &buckets {
            for slot in bucket {
                if state.should_stop() || state.len() >= max {
                    return;
                }
                let mut path = String::new();
                if !index.path_into(*slot, &mut scratch, &mut path) {
                    continue; // 链条断了（孤儿），跳过
                }
                if Path::new(&path).exists() {
                    state.push(&path);
                } else {
                    ghosts.push(*slot);
                }
            }
        }
    }

    /// 预扫目录：优先文件夹 + 开始菜单/桌面
    fn preset_roots(&self) -> Vec<PathBuf> {
        let mut roots = self.config.priority_folders.clone();
        if self.config.scan_shortcuts {
            roots.extend(known_folder::shortcut_roots());
        }
        roots
    }

    /// 直接遍历目录（不查索引）。预扫用，深度受 `scan_depth` 限制
    fn scan_folder(&self, root: &Path, query: &SearchQuery, state: &SessionState, depth: usize) {
        if depth > self.config.scan_depth {
            return;
        }
        let Ok(entries) = std::fs::read_dir(root) else {
            return; // 没权限或目录不存在都直接跳过
        };
        for entry in entries.flatten() {
            if state.should_stop() {
                return;
            }
            // 用 file_type 而不是 metadata：不跟随链接，也更快
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue; // 避免目录联接造成的环
            }
            let is_dir = file_type.is_dir();
            let path = entry.path();
            let Some(text) = path.to_str() else { continue };
            if query.matches(text) {
                state.push(text);
                if state.len() >= self.config.max_results {
                    return;
                }
            }
            if is_dir {
                self.scan_folder(&path, query, state, depth + 1);
            }
        }
    }
}

/// 清掉旧版索引（SQLite 分表）留下的库文件。只删认识的那几个名字，不做通配匹配。
fn cleanup_legacy_index(dir: &Path) {
    // 旧版每盘一个库，外加两个共享的小库
    let mut names: Vec<String> = vec!["priority.db".to_string(), "weight.db".to_string()];
    for code in 0..26u8 {
        names.push(format!("{}.db", char::from(b'A' + code)));
    }

    let mut freed = 0u64;
    for name in names {
        // WAL 模式会额外留下 -wal / -shm
        for suffix in ["", "-wal", "-shm"] {
            let path = dir.join(format!("{name}{suffix}"));
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if std::fs::remove_file(&path).is_ok() {
                freed += meta.len();
            }
        }
    }
    if freed > 0 {
        eprintln!(
            "[rastflow] 已清理旧版 SQLite 索引文件，释放 {:.1} MB",
            freed as f64 / 1048576.0
        );
    }
}

/// 把一条事件落到索引上。**不碰锁**，返回是否真的改动了索引。
fn apply_to_index(
    index: &mut FileIndex,
    event: &MonitorEvent,
    ignore: &IgnoreRules,
    priorities: &PriorityTable,
) -> bool {
    match event {
        MonitorEvent::Added {
            disk,
            frn,
            parent,
            name,
            is_dir,
        } => {
            // 父目录必须已经在索引里。不在的话这条记录永远解析不出路径、
            // 搜不到，插进去只是白占内存 —— 等下次全量重建再收它。
            if !is_root(*parent) && index.lookup(*disk, *parent).is_none() {
                return false;
            }
            let priority = if *is_dir {
                DIR_PRIORITY
            } else {
                priorities.of_suffix(pathutil::suffix_str(name))
            };
            let Some(slot) = index.insert(*disk, *frn, *parent, name, *is_dir, priority) else {
                return false;
            };
            // 路径级过滤：忽略目录（默认表里就含回收站）。
            // 先插进去才能拼路径，所以是先插后判、判不过再摘掉。
            let mut scratch = PathScratch::new();
            let mut buf = String::new();
            if index.path_into(slot, &mut scratch, &mut buf)
                && (ignore.matches(&buf) || pathutil::is_recycle_bin(&buf))
            {
                index.discard(slot);
                return false;
            }
            true
        }
        MonitorEvent::Removed { disk, frn } => {
            // 删到回收站不算删除：否则用户还没清空回收站，搜索里就找不到了
            let recycled = index
                .lookup(*disk, *frn)
                .and_then(|slot| index.path_of(slot))
                .is_some_and(|path| pathutil::is_recycle_bin(&path));
            if recycled {
                return false;
            }
            index.remove(*disk, *frn)
        }
        // 这两种只是通知，不涉及索引内容
        MonitorEvent::DiskWarning { .. } | MonitorEvent::DiskStopped(_) => false,
    }
}

fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 搜索会话的共享状态
#[derive(Default)]
struct SessionResults {
    paths: Vec<String>,
    seen: HashSet<String>,
}

struct SessionState {
    results: Mutex<SessionResults>,
    done: Mutex<bool>,
    finished: Condvar,
    cancelled: Arc<AtomicBool>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            results: Mutex::new(SessionResults::default()),
            done: Mutex::new(false),
            finished: Condvar::new(),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl SessionState {
    fn push(&self, path: &str) {
        let mut results = lock(&self.results);
        if results.seen.insert(path.to_string()) {
            results.paths.push(path.to_string());
        }
    }

    fn len(&self) -> usize {
        lock(&self.results).paths.len()
    }

    fn should_stop(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    fn finish(&self) {
        *lock(&self.done) = true;
        self.finished.notify_all();
    }
}

/// 一次搜索的句柄：可以随时取当前结果快照，也可以取消
pub struct SearchSession {
    state: Arc<SessionState>,
}

impl SearchSession {
    /// 当前已找到的结果（顺序即命中顺序）
    pub fn snapshot(&self) -> Vec<String> {
        lock(&self.state.results).paths.clone()
    }

    /// 搜索是否已结束
    pub fn is_done(&self) -> bool {
        *lock(&self.state.done)
    }

    /// 请求取消（后台线程会在下一次检查时退出）
    pub fn cancel(&self) {
        self.state.cancelled.store(true, Ordering::Relaxed);
    }

    /// 取一个取消句柄，让调用方在任意位置取消本次搜索
    pub fn cancel_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.state.cancelled)
    }

    /// 等待搜索结束，返回是否已结束
    pub fn wait(&self, timeout: Duration) -> bool {
        let guard = lock(&self.state.done);
        if *guard {
            return true;
        }
        let (guard, _) = self
            .state
            .finished
            .wait_timeout(guard, timeout)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard
    }
}

impl Drop for SearchSession {
    fn drop(&mut self) {
        // 还没跑完就丢掉会话时，别让后台线程继续白扫
        if !self.is_done() {
            self.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::win::usn::ROOT_MFT_INDEX;
    use crate::search::priority::DEFAULT_PRIORITY;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rastflow-core-engine-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 预扫开始菜单/桌面会让单测变慢且结果不可控，统一关掉
    fn test_config(data_dir: &Path) -> CoreConfig {
        let mut config = CoreConfig::new(data_dir);
        config.scan_shortcuts = false;
        config
    }

    /// 走真实的 `Engine::open`（会读写快照）打开一个只索引指定盘的引擎。
    fn open_engine(data_dir: &Path, disks: &[char]) -> Engine {
        let mut config = test_config(data_dir);
        config.disks = disks.to_vec();
        Engine::open(config).expect("打开引擎失败")
    }

    fn open_in_memory(config: CoreConfig, disks: Vec<char>) -> Arc<Engine> {
        let snapshot = index_mod::snapshot_path(&config.data_dir);
        let ignore = IgnoreRules::new(&config.ignore_paths);
        let mut config = config;
        config.disks = disks.clone();
        Arc::new(Engine {
            config,
            disks,
            index: RwLock::new(FileIndex::new()),
            priorities: RwLock::new(PriorityTable::with_builtin_defaults()),
            marks: Mutex::new(HashMap::new()),
            snapshot,
            ignore,
        })
    }

    /// 从路径合成一个稳定的 FRN（同路径同值）。
    ///
    /// ⚠️ MFT 记录号必须落在真实量级：查找表拿它当下标，给一个 2^48 级别的值
    /// 会真的去申请那么大的内存。
    fn synthetic_frn(path: &str) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in path.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        // +0x1000 是为了避开根记录号 5（否则会被当成盘根）
        let index = hash % 4_000_000 + 0x1000;
        (1u64 << 48) | index
    }

    /// 把一个完整路径塞进索引，沿途缺的目录会补出来。最后一段当**文件**。
    fn index_path(engine: &Engine, path: &str, priority: i32) {
        index_path_as(engine, path, priority, false);
    }

    /// 同上，但最后一段是**目录**
    fn index_dir_path(engine: &Engine, path: &str) {
        index_path_as(engine, path, DIR_PRIORITY, true);
    }

    fn index_path_as(engine: &Engine, path: &str, priority: i32, is_dir: bool) {
        let disk = path
            .chars()
            .next()
            .expect("测试路径要带盘符")
            .to_ascii_uppercase();
        let mut index = write(&engine.index);
        index.ensure_root(disk);

        let mut current = format!("{disk}:");
        let mut parent_frn = ROOT_MFT_INDEX;
        let mut segments = path.split('\\').skip(1).peekable();
        while let Some(segment) = segments.next() {
            let last = segments.peek().is_none();
            let own_dir = if last { is_dir } else { true };
            let full = format!("{current}\\{segment}");
            let frn = synthetic_frn(&full);
            let own = if own_dir { DIR_PRIORITY } else { priority };
            index
                .insert(disk, frn, parent_frn, segment, own_dir, own)
                .expect("插入失败");
            parent_frn = frn;
            current = full;
        }
    }

    fn search(engine: &Arc<Engine>, text: &str) -> Vec<String> {
        let session = engine.search(SearchQuery::parse(text));
        assert!(session.wait(Duration::from_secs(10)), "搜索应当结束");
        session.snapshot()
    }

    #[test]
    fn default_config_values() {
        let config = CoreConfig::new(r"D:\idx");
        assert_eq!(config.max_results, DEFAULT_MAX_RESULTS);
        assert!(config.scan_shortcuts);
        assert_eq!(config.scan_depth, 4);
        assert!(config.disks.is_empty());
        // 默认忽略回收站
        assert!(
            config
                .ignore_paths
                .iter()
                .any(|item| item.to_lowercase().contains("recycle"))
        );
    }

    #[test]
    fn priority_tiers_put_directories_last() {
        let dir = temp_dir("tiers");
        let engine = open_in_memory(test_config(&dir), vec!['C']);

        let tiers = engine.priority_tiers();
        assert_eq!(*tiers.last().unwrap(), DIR_PRIORITY, "目录排在最后");
        // 文件档是降序
        let files = &tiers[..tiers.len() - 1];
        assert!(files.windows(2).all(|pair| pair[0] >= pair[1]));
        // 兜底档一定在（没命中后缀规则的文件都落这里）
        assert!(files.contains(&DEFAULT_PRIORITY));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_existing_file_from_index() {
        let data_dir = temp_dir("search-data");
        let files_dir = temp_dir("search-files");
        let exe = files_dir.join("rastflow-notepad.exe");
        let txt = files_dir.join("rastflow-notepad.txt");
        std::fs::write(&exe, b"x").unwrap();
        std::fs::write(&txt, b"x").unwrap();

        let engine = open_in_memory(test_config(&data_dir), vec!['C']);
        index_path(&engine, &exe.to_string_lossy(), 30);
        index_path(&engine, &txt.to_string_lossy(), 0);

        let hits = search(&engine, "rastflow-notepad");
        assert!(hits.contains(&exe.to_string_lossy().to_string()), "命中 exe");
        assert!(hits.contains(&txt.to_string_lossy().to_string()), "命中 txt");
        // 高优先级的 exe 排在前面
        assert_eq!(hits[0], exe.to_string_lossy().to_string());

        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_dir_all(&files_dir);
    }

    #[test]
    fn lazy_delete_removes_records_that_no_longer_exist() {
        let data_dir = temp_dir("lazy-data");
        let ghost = r"C:\rastflow-ghost-notepad.exe";
        let engine = open_in_memory(test_config(&data_dir), vec!['C']);
        index_path(&engine, ghost, 30);
        assert_eq!(read(&engine.index).iter_live().count(), 2, "盘根 + 幽灵文件");

        // 文件并不存在 → 不该返回，而且会被摘掉
        assert!(search(&engine, "rastflow-ghost-notepad").is_empty());
        assert_eq!(read(&engine.index).iter_live().count(), 1, "幽灵被摘掉，只剩盘根");
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn results_are_capped_by_limit() {
        let data_dir = temp_dir("limit-data");
        let files_dir = temp_dir("limit-files");
        let engine = open_in_memory({
            let mut config = test_config(&data_dir);
            config.max_results = 3;
            config
        }, vec!['C']);
        for index in 0..10 {
            let file = files_dir.join(format!("rastflow-limit-{index}.exe"));
            std::fs::write(&file, b"x").unwrap();
            index_path(&engine, &file.to_string_lossy(), 30);
        }

        assert_eq!(search(&engine, "rastflow-limit").len(), 3);
        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_dir_all(&files_dir);
    }

    #[test]
    fn results_are_deduped_and_stable() {
        let data_dir = temp_dir("dedup-data");
        let files_dir = temp_dir("dedup-files");
        let file = files_dir.join("rastflow-dedup.exe");
        std::fs::write(&file, b"x").unwrap();
        let text = file.to_string_lossy().to_string();

        let engine = open_in_memory(test_config(&data_dir), vec!['C']);
        index_path(&engine, &text, 30);
        // 同一条路径再用另一个 FRN 插一次：索引里确实变成了两条记录，
        // 但搜索结果必须去重（`SessionState` 用 seen 集合兜底）
        {
            let mut index = write(&engine.index);
            let parent_frn = synthetic_frn(pathutil::parent_path(&text));
            index
                .insert(
                    'C',
                    (2u64 << 48) | 0x0099_0000,
                    parent_frn,
                    pathutil::file_name(&text),
                    false,
                    30,
                )
                .expect("插入失败");
        }

        assert_eq!(search(&engine, "rastflow-dedup").len(), 1);
        // 再搜一次结果顺序不变
        assert_eq!(
            search(&engine, "rastflow-dedup"),
            search(&engine, "rastflow-dedup")
        );
        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_dir_all(&files_dir);
    }

    #[test]
    fn empty_and_oversized_queries_scan_nothing() {
        let data_dir = temp_dir("empty-data");
        let engine = open_in_memory(test_config(&data_dir), vec!['C']);

        assert!(search(&engine, "").is_empty());
        let long = "a".repeat(MAX_SEARCH_TEXT_LEN + 1);
        assert!(search(&engine, &long).is_empty());
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn priority_folder_files_found_without_index() {
        let data_dir = temp_dir("prio-data");
        let folder = temp_dir("prio-folder");
        let nested = folder.join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        let deep = nested.join("rastflow-priority.txt");
        std::fs::write(&deep, b"x").unwrap();

        let engine = open_in_memory(
            {
                let mut config = test_config(&data_dir);
                config.priority_folders = vec![folder.clone()];
                config
            },
            vec!['C'],
        );

        // 索引里什么都不放，只靠目录预扫就应该能命中
        assert_eq!(search(&engine, "rastflow-priority"), vec![deep.to_string_lossy().to_string()]);

        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn cancelled_session_finishes_soon() {
        let data_dir = temp_dir("cancel-data");
        let engine = open_in_memory(test_config(&data_dir), vec!['C']);
        let session = engine.search(SearchQuery::parse("anything"));
        session.cancel();
        let _ = session.wait(Duration::from_secs(10));
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn cancel_handle_works_outside_the_session() {
        let data_dir = temp_dir("cancel-handle");
        let engine = open_in_memory(test_config(&data_dir), vec!['C']);
        let session = engine.search(SearchQuery::parse("anything"));
        let handle = session.cancel_handle();
        // 不需要 &SearchSession 就能取消
        handle.store(true, Ordering::Relaxed);
        assert!(session.wait(Duration::from_secs(10)));
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// `(disk, frn, parent, name, is_dir)`
    fn added(disk: char, frn: u64, parent: u64, name: &str, is_dir: bool) -> MonitorEvent {
        MonitorEvent::Added {
            disk,
            frn,
            parent,
            name: name.to_string(),
            is_dir,
        }
    }

    #[test]
    fn monitor_events_land_in_index_and_can_be_removed() {
        let data_dir = temp_dir("apply-event");
        let files_dir = temp_dir("apply-files");
        let engine = open_in_memory(test_config(&data_dir), vec!['C']);
        write(&engine.index).ensure_root('C');

        // 父目录的链条先铺好（相当于全量建索引时已经收进去了）
        let dir_text = files_dir.to_string_lossy().to_string();
        index_dir_path(&engine, &dir_text);
        let dir_frn = synthetic_frn(&dir_text);

        // 再模拟「这个目录里新建了一个文件」
        let file = files_dir.join("rastflow-event.exe");
        std::fs::write(&file, b"x").unwrap();
        let file_text = file.to_string_lossy().to_string();
        let file_frn = synthetic_frn(&file_text);
        assert!(engine
            .apply_event(&added('C', file_frn, dir_frn, "rastflow-event.exe", false))
            .unwrap());
        // 同一条重复上报是幂等的：就地更新，不该多出一条记录
        let before = read(&engine.index).iter_live().count();
        assert!(engine
            .apply_event(&added('C', file_frn, dir_frn, "rastflow-event.exe", false))
            .unwrap());
        assert_eq!(
            read(&engine.index).iter_live().count(),
            before,
            "重复上报不该让索引变大"
        );
        assert_eq!(search(&engine, "rastflow-event"), vec![file_text.clone()]);

        // 删除
        assert!(engine
            .apply_event(&MonitorEvent::Removed {
                disk: 'C',
                frn: file_frn
            })
            .unwrap());
        assert!(read(&engine.index).lookup('C', file_frn).is_none());
        assert!(search(&engine, "rastflow-event").is_empty());
        // 再删一次就「没改动」
        assert!(!engine
            .apply_event(&MonitorEvent::Removed {
                disk: 'C',
                frn: file_frn
            })
            .unwrap());

        // DiskWarning / DiskStopped 不涉及索引内容
        assert!(!engine
            .apply_event(&MonitorEvent::DiskWarning {
                disk: 'C',
                message: "x".into()
            })
            .unwrap());
        assert!(!engine.apply_event(&MonitorEvent::DiskStopped('C')).unwrap());

        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_dir_all(&files_dir);
    }

    #[test]
    fn event_with_unknown_parent_is_rejected() {
        let data_dir = temp_dir("orphan-event");
        let engine = open_in_memory(test_config(&data_dir), vec!['C']);
        write(&engine.index).ensure_root('C');

        // 父目录从没建过 → 这条插进去也搜不到，直接拒
        let unknown_parent = synthetic_frn(r"C:\never-created");
        assert!(!engine
            .apply_event(&added('C', 9001, unknown_parent, "orphan.txt", false))
            .unwrap());
        assert!(read(&engine.index).lookup('C', 9001).is_none());
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn renaming_directory_updates_all_child_paths() {
        // 这是层次化内存索引相对「存完整路径」最大的好处
        let data_dir = temp_dir("rename-dir");
        let engine = open_in_memory(test_config(&data_dir), vec!['C']);
        let dir_frn = synthetic_frn(r"C:\old-dir");
        {
            let mut index = write(&engine.index);
            index.ensure_root('C');
            index.insert('C', dir_frn, ROOT_MFT_INDEX, "old-dir", true, DIR_PRIORITY);
            index.insert('C', 7001, dir_frn, "kept.exe", false, 30);
        }
        assert_eq!(
            read(&engine.index)
                .path_of(read(&engine.index).lookup('C', 7001).unwrap())
                .as_deref(),
            Some(r"C:\old-dir\kept.exe")
        );

        // 目录改名：只更新目录那一条
        assert!(engine
            .apply_event(&added('C', dir_frn, ROOT_MFT_INDEX, "new-dir", true))
            .unwrap());

        // 子文件的路径自动变成新名字，**不需要**逐条更新
        let index = read(&engine.index);
        let slot = index.lookup('C', 7001).unwrap();
        assert_eq!(index.path_of(slot).as_deref(), Some(r"C:\new-dir\kept.exe"));
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn ignored_paths_and_recycle_bin_are_skipped() {
        let data_dir = temp_dir("apply-ignore");
        let engine = Arc::new(Engine {
            config: {
                let mut config = test_config(&data_dir);
                config.disks = vec!['C'];
                config
            },
            disks: vec!['C'],
            index: RwLock::new(FileIndex::new()),
            priorities: RwLock::new(PriorityTable::with_builtin_defaults()),
            marks: Mutex::new(HashMap::new()),
            snapshot: PathBuf::new(),
            ignore: IgnoreRules::new(&[r"C:\$Recycle.Bin".to_string(), r"C:\skipme".to_string()]),
        });
        write(&engine.index).ensure_root('C');

        // 回收站下的新增：先插进去拼出路径，再被回收站规则摘掉
        let recycle_frn = synthetic_frn(r"C:\$Recycle.Bin");
        engine
            .apply_event(&added('C', recycle_frn, ROOT_MFT_INDEX, "$Recycle.Bin", true))
            .unwrap();
        let index = read(&engine.index);
        assert!(
            index.lookup('C', recycle_frn).is_none(),
            "回收站目录本身也被忽略"
        );
        drop(index);

        // 忽略目录同理
        assert!(!engine
            .apply_event(&added('C', synthetic_frn(r"C:\skipme"), ROOT_MFT_INDEX, "skipme", true))
            .unwrap());

        // 不在忽略列表里的正常目录能进去
        assert!(engine
            .apply_event(&added('C', synthetic_frn(r"C:\keepme"), ROOT_MFT_INDEX, "keepme", true))
            .unwrap());

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn snapshot_roundtrip_keeps_results_searchable() {
        let data_dir = temp_dir("snapshot-roundtrip");
        let files_dir = temp_dir("snapshot-files");
        let file = files_dir.join("rastflow-persist.exe");
        std::fs::write(&file, b"x").unwrap();
        let text = file.to_string_lossy().to_string();

        {
            let engine = open_engine(&data_dir, &['C']);
            index_path(&engine, &text, 30);
            assert!(engine.is_indexed());
            engine.save_snapshot().unwrap();
        }

        // 重新打开：索引应当直接从快照里恢复
        let engine = Arc::new(open_engine(&data_dir, &['C']));
        assert!(engine.is_indexed(), "快照应当被读回来");
        assert!(search(&engine, "rastflow-persist").contains(&text));

        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_dir_all(&files_dir);
    }

    #[test]
    fn stale_entries_of_unused_disks_are_dropped_on_open() {
        let data_dir = temp_dir("stale-disk");
        let files_dir = temp_dir("stale-files");
        let file = files_dir.join("rastflow-stale.exe");
        std::fs::write(&file, b"x").unwrap();

        {
            let engine = open_engine(&data_dir, &['C']);
            index_path(&engine, &file.to_string_lossy(), 30);
            engine.save_snapshot().unwrap();
        }
        // 快照里只有 C 盘的数据，但配置改成只要 D 盘 —— 读回来时 C 的条目要清掉
        let engine = open_engine(&data_dir, &['D']);
        assert!(!engine.is_indexed());
        assert_eq!(read(&engine.index).disks().count(), 0);

        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_dir_all(&files_dir);
    }

    #[test]
    fn catch_up_is_skipped_without_journal_marks() {
        let data_dir = temp_dir("catch-up-nomark");
        let engine = open_engine(&data_dir, &['C']);
        let mut noop = |_: IndexProgress| {};
        assert_eq!(engine.catch_up(&mut noop), CatchUp::NoMark);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// 量一下真实量级的搜索耗时。
    ///
    /// ```powershell
    /// cargo test --release --bin rastflow benchmark_search_at_real_scale -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "性能测量，不进常规测试"]
    fn benchmark_search_at_real_scale() {
        const TOTAL: u64 = 1_200_000;
        const DIRS: u64 = 200_000;

        let data_dir = temp_dir("perf");
        let engine = open_in_memory(test_config(&data_dir), vec!['C']);
        let build_started = std::time::Instant::now();
        {
            let mut index = write(&engine.index);
            index.ensure_root('C');
            // 20 万个目录 + 100 万个文件，比例接近真实盘（一个目录放几个文件）
            for d in 0..DIRS {
                index
                    .insert(
                        'C',
                        (1u64 << 48) | (10_000 + d),
                        ROOT_MFT_INDEX,
                        &format!("dir{d}"),
                        true,
                        DIR_PRIORITY,
                    )
                    .expect("插入目录失败");
            }
            for f in 0..(TOTAL - DIRS) {
                let parent = (1u64 << 48) | (10_000 + f % DIRS);
                index
                    .insert(
                        'C',
                        (2u64 << 48) | (1_000_000 + f),
                        parent,
                        &format!("document-{f}.txt"),
                        false,
                        0,
                    )
                    .expect("插入文件失败");
            }
        }
        let (entries, bytes) = {
            let index = read(&engine.index);
            (index.iter_live().count(), index.used_bytes())
        };
        println!(
            "索引：{entries} 条，约 {:.1} MB，构建耗时 {:?}",
            bytes as f64 / 1048576.0,
            build_started.elapsed()
        );

        for (label, text) in [
            ("常见关键字（命中极多）", "document-1"),
            ("较窄的关键字", "document-12345"),
            ("几乎不命中", "zzzzz"),
        ] {
            let started = std::time::Instant::now();
            let session = engine.search(SearchQuery::parse(text));
            assert!(session.wait(Duration::from_secs(60)));
            println!(
                "{label}：返回 {} 条，用时 {:?}",
                session.snapshot().len(),
                started.elapsed()
            );
        }

        // 带路径关键字的查询要逐条拼路径，贵得多 —— 量出来才知道差多少
        let started = std::time::Instant::now();
        let session = engine.search(SearchQuery::parse(r"dir1234\document-9"));
        assert!(session.wait(Duration::from_secs(60)));
        println!(
            "路径关键字：返回 {} 条，用时 {:?}",
            session.snapshot().len(),
            started.elapsed()
        );

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// 端到端验证：真的去读 MFT 建索引，再从索引里搜索。
    ///
    /// 需要**管理员权限**与真实 NTFS 卷，所以默认 `#[ignore]`。手动跑：
    ///
    /// ```powershell
    /// $env:RASTFLOW_CORE_E2E = "C"        # 要索引的盘符
    /// cargo test --bin rastflow e2e -- --ignored --nocapture
    /// ```
    ///
    /// 想顺带验证增量监控，把 `RASTFLOW_CORE_E2E_MONITOR` 也设上。
    #[test]
    #[ignore = "需要管理员权限与真实 NTFS 卷"]
    fn e2e_build_index_and_search() {
        let Ok(letter) = std::env::var("RASTFLOW_CORE_E2E") else {
            eprintln!("未设置 RASTFLOW_CORE_E2E，跳过端到端验证");
            return;
        };
        let Some(disk) = letter.chars().next() else {
            eprintln!("RASTFLOW_CORE_E2E 是空的，跳过");
            return;
        };

        let data_dir = temp_dir("e2e");
        let engine = Arc::new({
            let mut config = CoreConfig::new(&data_dir);
            config.disks = vec![disk];
            config.scan_shortcuts = false;
            Engine::open(config).expect("打开引擎失败（多半是没有管理员权限）")
        });
        println!("已打开引擎，盘：{}", engine.disks().iter().collect::<String>());

        let started = std::time::Instant::now();
        let report = engine.build_index(None).expect("建索引失败");
        println!("建索引完成：{report:?}");

        let (entries, bytes) = {
            let index = read(&engine.index);
            (index.iter_live().count(), index.used_bytes())
        };
        println!(
            "索引常驻内存：{entries} 条，约 {:.1} MB，用时 {:.1}s",
            bytes as f64 / 1048576.0,
            started.elapsed().as_secs_f64()
        );

        let session = engine.search(SearchQuery::parse("notepad"));
        assert!(session.wait(Duration::from_secs(30)), "搜索应当结束");
        let hits = session.snapshot();
        println!("命中 {} 条，前 10 条：", hits.len());
        for path in hits.iter().take(10) {
            println!("  {path}");
        }
        assert!(!hits.is_empty(), "系统盘上应当能搜到 notepad");

        // 快照往返：重新打开应当直接可用，不用再扫 MFT
        let snapshot = index_mod::snapshot_path(&data_dir);
        println!("快照：{}（{} 字节）", snapshot.display(), std::fs::metadata(&snapshot).map(|m| m.len()).unwrap_or(0));
        let reopened = {
            let mut config = CoreConfig::new(&data_dir);
            config.disks = vec![disk];
            config.scan_shortcuts = false;
            Engine::open(config).expect("重新打开失败")
        };
        assert!(reopened.is_indexed(), "重新打开应当直接从快照恢复");
        let entries2 = read(&reopened.index).iter_live().count();
        assert_eq!(entries, entries2, "恢复出来的条目数应当一致");
        println!("从快照恢复 {entries2} 条 ✓");

        // 可选的增量监控验证：启动监控，让用户手动建/删文件。
        // 这里直接打印事件内容，便于人工核对「新建/改名/删除」分别对应什么。
        if std::env::var("RASTFLOW_CORE_E2E_MONITOR").is_ok() {
            let monitor = engine.start_monitor().expect("启动监控失败");
            println!("监控已启动，10 秒内随意新建/删除文件…");
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                let Some(event) = monitor.recv_timeout(Duration::from_millis(500)) else {
                    continue;
                };
                println!("  事件 {event:?}");
                let _ = engine.apply_event(&event);
            }
            monitor.join();
            println!("监控已停止");
        }

        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
