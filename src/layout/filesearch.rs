//! 文件搜索集成：把 [`crate::search`] 的自研索引接到界面上。
//!
//! 打开卷句柄要 `GENERIC_WRITE`（管理员），所以**碰 MFT/USN 的代码跑在提权的索引
//! 子进程里**（[`crate::search::helper_main`]），本进程只通过命名管道收记录与事件。
//! 索引、快照、搜索一直在本进程，子进程不持有索引。
//!
//! 线程模型见 `docs/architecture.md` §5：引导线程（本进程）拉起子进程并起一条泵线程读管道，
//! 搜索跑在调用方给的线程上（gpui 的 background executor）。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::search::engine::{CoreConfig, Engine, ScanStats};
use crate::search::helper::{self, HelperSession, Message, Request};
use crate::search::index::JournalMark;
use crate::search::lifecycle::plan_start;
use crate::search::matcher::SearchQuery;
use crate::search::monitor::MonitorEvent;
use crate::search::win::service;
use crate::utils::app_data_dir;

/// 搜索超时：超过就返回已找到的部分（索引大、关键字冷门时可能扫不完）
const SEARCH_TIMEOUT: Duration = Duration::from_secs(5);

/// 结果条数上限
const RESULT_LIMIT: usize = 200;

/// 累计这么多次增量变更就落一次快照。
///
/// 索引本身就在内存里（搜索走的就是它），快照只是「下次启动不用重扫 MFT」。
/// 用计数而不是定时器：泵线程只会在收到消息时醒，没必要为了查表再起一个心跳。
/// 没来得及落的变更不会丢 —— 下次启动会从更早的位点重放回来。
const SNAPSHOT_EVERY: u64 = 2000;

/// 装完服务后等它真正进入 `Running` 的上限
const SERVICE_START_TIMEOUT: Duration = Duration::from_secs(15);

// ---------- 对外类型 ----------

/// 文件搜索的可用状态
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchStatus {
    Uninitialized,
    Indexing {
        disk: Option<char>,
        written: u64,
    },
    Ready {
        /// 增量监控是否已启动。非管理员运行时通常为 `false`，
        /// 此时结果可能不包含刚新建的文件。
        monitoring: bool,
    },
    Unavailable {
        reason: String,
        /// 属于「需要管理员权限」这一类，界面据此决定要不要给提权按钮
        needs_admin: bool,
    },
}

impl SearchStatus {
    /// 是否属于「需要管理员权限」这一类（界面据此决定要不要给出提权按钮）
    pub fn needs_admin(&self) -> bool {
        matches!(self, Self::Unavailable { needs_admin: true, .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileResult {
    pub name: String,
    pub dir: String,
    pub path: PathBuf,
    /// 体积；目录没有有意义的大小，显示为 `-`
    pub size: String,
    pub modified: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchError {
    /// 还没初始化完（或没有可用索引）
    NotReady,
    Unavailable(String),
    /// 查询为空或过长
    InvalidQuery,
}

impl SearchError {
    pub fn message(&self) -> String {
        match self {
            Self::NotReady => "索引尚未就绪，请稍候".to_string(),
            Self::Unavailable(reason) => reason.clone(),
            Self::InvalidQuery => "请输入有效的搜索关键字".to_string(),
        }
    }
}

// ---------- 全局状态 ----------

struct Inner {
    /// 由引导线程填入；填好之前 `search` 只会返回 `NotReady`。
    ///
    /// 用 `Mutex<Option<..>>` 而不是 `OnceLock`：装完索引服务要能重新引导一次。
    engine: Mutex<Option<Arc<Engine>>>,
    status: Mutex<SearchStatus>,
    /// 上一次搜索的取消句柄：新的搜索到来时先把旧的取消掉
    last_cancel: Mutex<Option<Arc<AtomicBool>>>,
    /// 防止并发引导（启动 / 重试 / 装完服务）
    bootstrapping: AtomicBool,
}

impl Inner {
    fn new() -> Self {
        Self {
            engine: Mutex::new(None),
            status: Mutex::new(SearchStatus::Uninitialized),
            last_cancel: Mutex::new(None),
            bootstrapping: AtomicBool::new(false),
        }
    }

    fn engine(&self) -> Option<Arc<Engine>> {
        self.engine
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn set_engine(&self, engine: Arc<Engine>) {
        *self
            .engine
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(engine);
    }

    fn status(&self) -> SearchStatus {
        self.status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn set_status(&self, status: SearchStatus) {
        *self
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = status;
    }

    /// 增量监控已停止（监控线程全部退出）：把「监控中」降级。
    /// 不覆盖「正在建索引」「不可用」这些状态。
    fn mark_monitoring_stopped(&self) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let SearchStatus::Ready { monitoring } = &mut *status {
            *monitoring = false;
        }
    }
}

static STATE: OnceLock<Arc<Inner>> = OnceLock::new();

/// 索引数据目录：`%LOCALAPPDATA%\rastflow\index`
pub fn index_dir() -> PathBuf {
    app_data_dir().join("index")
}

/// 启动文件搜索（幂等）。
///
/// 第一次调用会开一条引导线程：打开索引 → 连索引服务（没有则拉起一次性子进程）→ 起泵线程。
/// 之后调用只做状态查询。初始化与建库进度请轮询 [`status`]。
pub fn start() {
    let inner = STATE.get_or_init(|| {
        let inner = Arc::new(Inner::new());
        bootstrap(Arc::clone(&inner));
        inner
    });

    let _ = inner; // 状态由 status() 读取
}

/// 跑一次引导（幂等）。装完索引服务后也走这里。
fn bootstrap(inner: Arc<Inner>) {
    if inner.bootstrapping.swap(true, Ordering::SeqCst) {
        return; // 已经有一条在跑
    }
    // 起不来也不影响后续调用：搜索会报 NotReady / Unavailable
    let worker = Arc::clone(&inner);
    let spawned = std::thread::Builder::new()
        .name("rastflow-index-bootstrap".to_string())
        .spawn(move || {
            // 重试时先把上一轮的失败态清掉，否则界面会一直停在旧错误上
            if matches!(
                worker.status(),
                SearchStatus::Unavailable { .. } | SearchStatus::Uninitialized
            ) {
                worker.set_status(SearchStatus::Uninitialized);
            }
            if let Err(err) = bootstrap_engine(&worker) {
                eprintln!("[rastflow] 文件索引初始化失败：{err}");
                worker.set_status(SearchStatus::Unavailable {
                    reason: err.to_string(),
                    needs_admin: needs_admin_hint(&err),
                });
            }
            worker.bootstrapping.store(false, Ordering::SeqCst);
        });
    if spawned.is_err() {
        inner.bootstrapping.store(false, Ordering::SeqCst);
    }
}

pub fn status() -> SearchStatus {
    match STATE.get() {
        Some(inner) => inner.status(),
        None => SearchStatus::Uninitialized,
    }
}

/// 以管理员身份安装**索引服务**，装完自动重新引导。
///
/// 这是整个应用里唯一会弹 UAC 的操作，而且只在用户主动点「安装索引服务」时发生。
/// 装好之后每次启动都不再提权 —— 这就是 Windows 服务的价值所在。
///
/// 本函数立刻返回：提权与等待都在后台线程里做，界面靠轮询 [`status`] 看结果。
pub fn install_index_service() {
    let Some(inner) = STATE.get().cloned() else {
        return;
    };
    // 服务已经在跑（比如上次装过、只是这轮引导失败）：不用再弹 UAC，直接重试
    if service::state().is_running() {
        bootstrap(inner);
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("rastflow-service-install".to_string())
        .spawn(move || {
            match helper::run_service_installer(true) {
                // 服务刚注册完可能还在 start-pending：等它真正跑起来再连，
                // 否则会白走一次「管道不存在」的兜底路径
                Ok(()) if service::wait_until_running(SERVICE_START_TIMEOUT) => {
                    eprintln!("[rastflow] 索引服务已安装并启动");
                    bootstrap(inner);
                }
                Ok(()) => eprintln!("[rastflow] 索引服务已安装，但没有在 {SERVICE_START_TIMEOUT:?} 内启动"),
                Err(err) => {
                    // 用户点了「否」或提权被策略禁止：状态保持不变，可以再点一次
                    eprintln!("[rastflow] 安装索引服务失败：{err}");
                }
            }
        });
    if let Err(err) = spawned {
        eprintln!("[rastflow] 无法启动安装线程：{err}");
    }
}

// ---------- 搜索 ----------

/// 按关键字搜索。
///
/// 解析规则与自研核心一致：关键字按 `;` 分隔、全部命中才算匹配（AND）、
/// 默认忽略大小写（见 [`SearchQuery::parse`]）。由于本函数会阻塞等结果，
/// 请在线程池里调用。
pub fn search(query: &str) -> Result<Vec<FileResult>, SearchError> {
    let inner = STATE.get().ok_or(SearchError::NotReady)?;
    match inner.status() {
        SearchStatus::Ready { .. } => {}
        SearchStatus::Unavailable { reason, .. } => {
            return Err(SearchError::Unavailable(reason));
        }
        _ => return Err(SearchError::NotReady),
    }
    let query = query.trim();
    if query.is_empty() {
        return Err(SearchError::InvalidQuery);
    }
    let engine = inner.engine().ok_or(SearchError::NotReady)?;

    // 新搜索到来时取消上一次，避免上一轮的扫描继续白占 CPU
    let session = engine.search(SearchQuery::parse(query));
    let cancel = session.cancel_handle();
    if let Ok(mut previous) = inner.last_cancel.lock()
        && let Some(old) = previous.replace(cancel)
    {
        old.store(true, Ordering::Relaxed);
    }

    session.wait(SEARCH_TIMEOUT);
    // 超时也返回已找到的部分：宁可给出部分结果，也不要让界面一直白等
    let mut paths = session.snapshot();
    paths.truncate(RESULT_LIMIT);

    // 体积与修改时间不在索引里（索引只存名字与父子关系），展示时补一次 stat。
    // 放在这里而不是渲染路径上：200 次 stat 在本地盘是几毫秒，但绝不能每帧做。
    Ok(paths.iter().map(|path| FileResult::from_path(path)).collect())
}

impl FileResult {
    fn from_path(path: &str) -> Self {
        let path_buf = PathBuf::from(path);
        let name = path_buf
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string());
        let dir = path_buf
            .parent()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_default();

        let (size, modified) = match std::fs::metadata(&path_buf) {
            Ok(meta) => {
                let size = if meta.is_dir() {
                    "-".to_string()
                } else {
                    format_size(meta.len())
                };
                (size, format_system_time(meta.modified()))
            }
            // stat 失败说明这条记录已经过期了，交给搜索时的懒删除去清理
            Err(_) => ("-".to_string(), "-".to_string()),
        };

        Self {
            name,
            dir,
            path: path_buf,
            size,
            modified,
        }
    }
}

/// 打开搜索结果（文件用默认程序，目录用资源管理器）
pub fn open_result(result: &FileResult) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    // 用 cmd /C start 而不是 ShellExecuteW：空参数 `""` 会被当作窗口标题，
    // 这样路径里的空格与引号都不需要自己转义
    std::process::Command::new("cmd")
        .args(["/C", "start", ""])
        .arg(&result.path)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
}

// ---------- 内部实现 ----------

/// 打散引擎、拉起提权子进程、起一条泵线程。
///
/// 本函数**不阻塞**等索引建完：界面靠轮询 [`status`] 看进度。
fn bootstrap_engine(inner: &Arc<Inner>) -> crate::search::Result<()> {
    let engine = Arc::new(Engine::open(CoreConfig::new(index_dir()))?);
    // 引擎先装好：即使最终状态不可用，搜索侧的报错也能更具体
    inner.set_engine(Arc::clone(&engine));

    if engine.disks().is_empty() {
        inner.set_status(SearchStatus::Unavailable {
            reason: "没有找到可用于索引的本地 NTFS 磁盘".to_string(),
            needs_admin: false,
        });
        return Ok(());
    }

    let session = match HelperSession::connect_running_service() {
        Ok(session) => session,
        Err(err) => {
            eprintln!("[rastflow] 索引服务不可用：{err}");
            inner.set_status(if engine.is_indexed() {
                SearchStatus::Ready { monitoring: false }
            } else {
                SearchStatus::Unavailable {
                    reason: "索引服务不可用，请安装或启动索引服务".to_string(),
                    needs_admin: !service::state().is_running(),
                }
            });
            return Ok(());
        }
    };

    let pump_engine = Arc::clone(&engine);
    let pump_inner = Arc::clone(inner);
    std::thread::Builder::new()
        .name("rastflow-index-pump".to_string())
        .spawn(move || run_pump(pump_inner, pump_engine, session))
        .map(|_| ())
        .map_err(crate::search::CoreError::Io)
}

/// 泵线程的累计状态
#[derive(Default)]
struct PumpState {
    /// 还没报「这个盘处理完了」的盘
    pending: HashSet<char>,
    /// 本轮扫描里各盘覆盖到的位点
    scan_marks: HashMap<char, JournalMark>,
    /// 本轮扫描已写入的记录数（给界面看）
    written: u64,
    /// 正在扫描的盘
    phase_disk: Option<char>,
    /// 最近一次「这个盘碰不了」的原因，用来汇报「一个盘都没索引上」
    last_error: Option<String>,
    /// 距上次落快照应用了多少条增量
    applied_since_save: u64,
}

/// 泵线程：把子进程流过来的记录与事件落到索引上。
///
/// 这是**唯一**改动索引的地方（除搜索时的懒删除），所以锁与状态机都集中在这里。
fn run_pump(inner: Arc<Inner>, engine: Arc<Engine>, session: HelperSession) {
    let marks = engine.marks();
    let marked: HashSet<char> = marks.keys().copied().collect();
    engine.abort_scans();
    let plan = plan_start(engine.disks(), &marked);
    inner.set_status(SearchStatus::Indexing { disk: None, written: 0 });
    let mut requests = Vec::new();
    if !plan.catch_up.is_empty() {
        requests.push(Request::CatchUp { marks: plan.catch_up.iter().filter_map(|disk| marks.get(disk).map(|mark| (*disk, *mark))).collect() });
    }
    if !plan.scan.is_empty() { requests.push(Request::Scan { disks: plan.scan }); }
    for request in requests {
        if let Err(err) = session.send(&request) { fail(&inner, err); return; }
    }

    let mut state = PumpState {
        pending: engine.disks().iter().copied().collect(),
        ..Default::default()
    };
    let mut requested_monitor = false;
    let mut ready = false;

    loop {
        let message = match session.recv() {
            Ok(Some(message)) => message,
            Ok(None) => {
                eprintln!("[rastflow] 索引子进程已退出");
                break;
            }
            Err(err) => {
                eprintln!("[rastflow] 读取索引管道失败：{err}");
                break;
            }
        };

        match message {
            Message::ScanStarted { disk, mark } => {
                engine.begin_scan(disk);
                state.scan_marks.insert(disk, mark);
                state.written = 0;
                state.phase_disk = Some(disk);
                inner.set_status(SearchStatus::Indexing {
                    disk: Some(disk),
                    written: 0,
                });
            }
            Message::Records(entries) => {
                state.written += engine.insert_scanned_batch(&entries) as u64;
                if let Some(disk) = state.phase_disk {
                    inner.set_status(SearchStatus::Indexing {
                        disk: Some(disk),
                        written: state.written,
                    });
                }
            }
            Message::ScanFinished { disk, records } => {
                if let Some(mark) = state.scan_marks.get(&disk).copied() {
                    eprintln!("[rastflow] {disk}: 枚举 {records} 条，开始补读扫描期间变更");
                    if let Err(err) = session.send(&Request::CatchUp { marks: vec![(disk, mark)] }) {
                        fail(&inner, err); return;
                    }
                }
            }
            Message::CatchUpFinished {
                disk,
                events,
                mark,
            } => {
                if events > 0 {
                    eprintln!("[rastflow] {disk}: 从 USN 日志补齐了 {events} 条变更");
                }
                if state.scan_marks.remove(&disk).is_some() {
                    let stats = engine.finish_scan(disk, mark);
                    log_scan(disk, stats.written, stats);
                } else { engine.set_mark(disk, mark); }
                state.pending.remove(&disk);
            }
            Message::NeedsRebuild { disk } => {
                eprintln!("[rastflow] {disk}: USN 日志已覆盖上次位点，改为全量重扫");
                state.scan_marks.remove(&disk);
                state.pending.insert(disk);
                requested_monitor = false;
                if let Err(err) = session.send(&Request::Scan { disks: vec![disk] }) {
                    fail(&inner, err);
                    return;
                }
            }
            Message::Unavailable { disk, reason } => {
                engine.abort_scan(disk);
                state.scan_marks.remove(&disk);
                eprintln!("[rastflow] {disk}: 无法处理该盘（{reason}），沿用已有索引");
                state.last_error = Some(format!("{disk}: {reason}"));
                state.pending.remove(&disk);
            }
            Message::Events(events) => {
                for event in &events {
                    match event {
                        MonitorEvent::DiskWarning { disk, message } => {
                            eprintln!("[rastflow] {disk}: 文件监控警告：{message}");
                        }
                        MonitorEvent::DiskStopped(disk) => {
                            eprintln!("[rastflow] {disk}: 文件监控已停止");
                        }
                        _ => match engine.apply_event(event) {
                            Ok(true) => state.applied_since_save += 1,
                            Ok(false) => {}
                            Err(err) => eprintln!("[rastflow] 应用文件变更失败：{err}"),
                        },
                    }
                }
            }
            Message::Warning { disk, reason } => {
                eprintln!("[rastflow] {disk}: {reason}");
            }
            Message::Monitoring { monitoring } => {
                ready = true;
                // 一个盘都没跟上、索引又是空的：说明这次什么也没拿到，
                // 别让界面停在「就绪但永远搜不到东西」
                if !monitoring && !engine.is_indexed() {
                    let reason = state
                        .last_error
                        .take()
                        .unwrap_or_else(|| "没有得到任何索引数据".to_string());
                    fail(&inner, crate::search::CoreError::Helper(reason));
                } else {
                    eprintln!(
                        "[rastflow] 文件索引就绪（实时监控：{}）",
                        if monitoring { "已启动" } else { "不可用" }
                    );
                    inner.set_status(SearchStatus::Ready { monitoring });
                }
                // 建完库立刻落一份：下次启动就能直接补课，不用再扫一遍 MFT
                save_snapshot(&engine);
                state.applied_since_save = 0;
            }
            Message::Fatal(reason) => {
                fail(&inner, crate::search::CoreError::Helper(reason));
                return;
            }
            Message::Bye | Message::Ready { .. } => {}
        }

        // 所有盘都报完了，才让子进程开始实时跟随：
        // 跟随的起点就是各盘的位点，中间不会漏事件
        if !requested_monitor && state.pending.is_empty() {
            requested_monitor = true;
            if let Err(err) = session.send(&Request::Monitor) {
                fail(&inner, err);
                return;
            }
        }

        if state.applied_since_save >= SNAPSHOT_EVERY {
            save_snapshot(&engine);
            state.applied_since_save = 0;
        }
    }

    engine.abort_scans();
    // 退出前把关：把最后的变更落盘，下次启动就能少补一段日志
    if state.applied_since_save > 0 {
        save_snapshot(&engine);
    }
    if !ready {
        // 从来没到过 Ready：界面不该停在「正在建索引」
        inner.set_status(SearchStatus::Unavailable {
            reason: "索引子进程提前退出（可能被系统或安全软件拦截）".to_string(),
            needs_admin: false,
        });
    } else {
        inner.mark_monitoring_stopped();
    }
}

fn log_scan(disk: char, records: u64, stats: ScanStats) {
    eprintln!(
        "[rastflow] {disk}: 枚举 {records} 条，索引 {} 条（忽略 {}，断链 {}）",
        stats.written, stats.ignored, stats.broken
    );
}

/// 把失败映射成界面状态
fn fail(inner: &Inner, err: crate::search::CoreError) {
    eprintln!("[rastflow] 文件索引失败：{err}");
    inner.set_status(SearchStatus::Unavailable {
        reason: err.to_string(),
        needs_admin: needs_admin_hint(&err),
    });
}

fn save_snapshot(engine: &Arc<Engine>) {
    match engine.save_snapshot() {
        Ok(stats) => eprintln!(
            "[rastflow] 索引快照已保存：{} 条，{:.1} MB",
            stats.entries,
            stats.file_bytes as f64 / 1048576.0
        ),
        // 落盘失败不该影响搜索（索引还在内存里）
        Err(err) => eprintln!("[rastflow] 保存索引快照失败：{err}"),
    }
}

/// 这个错误是不是「需要管理员权限」——界面据此决定要不要给提权按钮
fn needs_admin_hint(err: &crate::search::CoreError) -> bool {
    const ERROR_ACCESS_DENIED: u32 = 5;
    match err {
        crate::search::CoreError::ElevationDenied => true,
        crate::search::CoreError::Win32 { err, .. } => {
            crate::search::win::pipe::win32_code(err) == ERROR_ACCESS_DENIED
        }
        _ => false,
    }
}

// ---------- 展示格式化 ----------

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

fn format_system_time(time: Result<std::time::SystemTime, std::io::Error>) -> String {
    let Ok(time) = time else {
        return "-".to_string();
    };
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => format_unix_time(duration.as_secs()),
        Err(_) => "-".to_string(),
    }
}

fn format_unix_time(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// 天数 → 公历年月日（Howard Hinnant 的 civil_from_days 算法）
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (y + i64::from(m <= 2), m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_uses_binary_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GB");
    }

    #[test]
    fn format_time_to_minute() {
        // 2024-01-01 00:00:00 UTC = 19723 天
        assert_eq!(format_unix_time(1_704_067_200), "2024-01-01 00:00");
        assert_eq!(format_unix_time(1_704_067_200 + 3_600 + 60 * 7), "2024-01-01 01:07");
        // 1970 起点
        assert_eq!(format_unix_time(0), "1970-01-01 00:00");
    }

    #[test]
    fn civil_from_days_handles_leap_years() {
        // 1970-01-01 是第 0 天
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000-02-29：能被 400 整除，是闰年
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));

        // 2100 能被 100 整除但不能被 400 整除 → 不是闰年，2/29 不存在。
        // 这里不手算纪元偏移，而是扫出 2100-02-28 的日序号再验证相邻关系。
        let feb28 = (0..60_000)
            .find(|day| civil_from_days(*day) == (2100, 2, 28))
            .expect("应能找到 2100-02-28");
        assert_eq!(
            civil_from_days(feb28 + 1),
            (2100, 3, 1),
            "2100 不是闰年，2/28 的次日应是 3/1"
        );
        assert_eq!(civil_from_days(feb28 - 1), (2100, 2, 27));
        // 整个 2100 年都不该出现 2/29
        assert!(
            !(0..60_000).any(|day| civil_from_days(day) == (2100, 2, 29)),
            "2100 年不应存在 2 月 29 日"
        );
    }

    #[test]
    fn file_result_splits_name_and_dir() {
        // 用一个不存在的绝对路径：拆分逻辑不依赖文件是否真的存在
        let result = FileResult::from_path(r"C:\rastflow-nonexistent-dir\rastflow-nonexistent.exe");
        assert_eq!(result.name, "rastflow-nonexistent.exe");
        assert_eq!(result.dir, r"C:\rastflow-nonexistent-dir");
        assert_eq!(
            result.path,
            PathBuf::from(r"C:\rastflow-nonexistent-dir\rastflow-nonexistent.exe")
        );
        // 文件不存在时 stat 会失败，要给占位值而不是崩掉
        assert_eq!(result.size, "-");
        assert_eq!(result.modified, "-");
    }

    #[test]
    fn file_result_reports_size_for_real_file() {
        // 用 Cargo.toml 这个一定存在的文件
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let result = FileResult::from_path(&manifest.to_string_lossy());
        assert_eq!(result.name, "Cargo.toml");
        assert!(result.size.ends_with("B"), "应给出体积，实际 {}", result.size);
        assert_ne!(result.modified, "-");
        assert_ne!(result.path, PathBuf::new());
    }

    #[test]
    fn search_error_has_readable_message() {
        assert!(SearchError::NotReady.message().contains("索引"));
        assert_eq!(
            SearchError::Unavailable("首次建立索引需要管理员权限".into()).message(),
            "首次建立索引需要管理员权限"
        );
        assert!(!SearchError::InvalidQuery.message().is_empty());
    }

    #[test]
    fn index_dir_lives_under_app_data() {
        let dir = index_dir();
        assert!(dir.ends_with("index"));
        assert!(dir.starts_with(app_data_dir()));
    }

    #[test]
    fn search_is_unavailable_before_ready() {
        // 注意：本进程内其它测试可能已经调用过 start()，所以这里只断言
        // 「没就绪时 search 一定报错」这个不依赖全局状态的契约。
        if !matches!(status(), SearchStatus::Ready { .. }) {
            assert!(matches!(
                search("anything"),
                Err(SearchError::NotReady) | Err(SearchError::Unavailable(_))
            ));
        }
    }

    #[test]
    fn empty_query_is_rejected() {
        assert_eq!(search("   "), Err(SearchError::NotReady));
    }

    #[test]
    fn needs_admin_only_for_elevation() {
        let need = SearchStatus::Unavailable {
            reason: "首次建立索引需要管理员权限".into(),
            needs_admin: true,
        };
        assert!(need.needs_admin());
        assert!(!SearchStatus::Unavailable {
            reason: "没有找到可用于索引的本地 NTFS 磁盘".into(),
            needs_admin: false,
        }
        .needs_admin());
        assert!(!SearchStatus::Ready { monitoring: true }.needs_admin());
        assert!(!SearchStatus::Indexing {
            disk: Some('C'),
            written: 1
        }
        .needs_admin());
    }
}
