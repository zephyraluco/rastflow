//! 文件搜索集成模块
//!
//! 直接用 [`crate::search`] 的自研索引：索引由 `FSCTL_ENUM_USN_DATA` 枚举 MFT 建立，
//! 增量由 USN 日志阻塞读取维护，索引常驻内存，磁盘上只留一份快照。
//!
//! # 一个固有代价：需要管理员
//!
//! `CreateFileW(r"\\.\C:")` 请求了 `GENERIC_WRITE`，所以**建索引与实时监控需要管理员**。
//! 非管理员运行时仍然可以**搜索已有索引**，只是不能在本机首次建索引、也拿不到实时增量。
//! 这个代价由 [`startup_action`] 显式建模：它把「有没有索引 / 是不是管理员」直接映射成
//! 该做什么，不靠静默降级。
//!
//! 首次可用要等建索引跑完（几十秒）；索引里只存路径，体积与修改时间展示时按需 `stat`。
//!
//! # 线程模型
//!
//! - **引导线程**（[`start`]）：打开索引库 → 有索引就绪，没有则建索引 → 启监控；
//! - **监控泵线程**：把 USN 事件喂给 [`Engine::apply_event`]，落到索引里。
//!   整个进程只允许有一条（[`ensure_monitor`] 幂等），重建索引也不会换掉它；
//! - 搜索跑在调用方给的线程上（gpui 的 background executor），所以可以阻塞等待。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::PCWSTR;

use crate::search::engine::{CatchUp, CoreConfig, Engine};
use crate::search::matcher::SearchQuery;
use crate::search::monitor::MonitorHandle;
use crate::utils::app_data_dir;

/// 搜索超时：超过就返回已找到的部分（索引大、关键字冷门时可能扫不完）
const SEARCH_TIMEOUT: Duration = Duration::from_secs(5);

/// 结果条数上限
const RESULT_LIMIT: usize = 200;

/// 增量变更停下来这么久之后才落快照。
///
/// 索引本身就在内存里（搜索走的就是它），快照只是「下次启动不用重扫 MFT」，
/// 所以频繁写盘没有意义，反而会和用户的磁盘 IO 抢带宽。
const SNAPSHOT_IDLE: Duration = Duration::from_secs(60);

/// 泵线程单次等事件的时长：超时说明这段时间没有变更，顺便看看该不该落快照。
const PUMP_TICK: Duration = Duration::from_millis(500);

// ---------- 对外类型 ----------

/// 文件搜索的可用状态
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchStatus {
    /// 还没开始初始化
    Uninitialized,
    /// 正在建索引
    Indexing {
        /// 当前正在处理的盘
        disk: Option<char>,
        /// 已写入的记录数
        written: u64,
    },
    /// 可以搜索
    Ready {
        /// 增量监控是否已启动。非管理员运行时通常为 `false`
        /// （读 USN 日志也需要管理员），此时结果可能不包含刚刚新建的文件。
        monitoring: bool,
    },
    /// 不可用
    Unavailable {
        reason: String,
        /// 是否属于「需要管理员权限」这一类
        needs_admin: bool,
    },
}

impl SearchStatus {
    /// 是否属于「需要管理员权限」这一类（界面据此决定要不要给出提权按钮）
    pub fn needs_admin(&self) -> bool {
        matches!(self, Self::Unavailable { needs_admin: true, .. })
    }
}

/// 一条搜索结果
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileResult {
    /// 文件或目录名
    pub name: String,
    /// 所在目录
    pub dir: String,
    /// 完整路径
    pub path: PathBuf,
    /// 体积（目录为 `-`）
    pub size: String,
    /// 修改时间
    pub modified: String,
}

/// 搜索失败原因
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchError {
    /// 还没初始化完（或没有可用索引）
    NotReady,
    /// 状态不可用
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
    /// 由引导线程填入；填好之前 `search` 只会返回 `NotReady`
    engine: OnceLock<Arc<Engine>>,
    status: Mutex<SearchStatus>,
    /// 上一次搜索的取消句柄：新的搜索到来时先把旧的取消掉
    last_cancel: Mutex<Option<Arc<AtomicBool>>>,
    /// 正在运行的监控泵线程。它退出时会把各盘的监控线程一并收掉，
    /// 所以「句柄已结束」等于「监控已经没了」（见 [`ensure_monitor`]）
    pump: Mutex<Option<JoinHandle<()>>>,
    /// 防止重复触发建索引
    building: AtomicBool,
}

impl Inner {
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
/// 第一次调用会开一条引导线程做「打开索引库 / 建索引 / 启监控」，
/// 之后调用只做状态查询。初始化进度请轮询 [`status`]。
pub fn start() {
    let inner = STATE.get_or_init(|| {
        let inner = Arc::new(Inner {
            engine: OnceLock::new(),
            status: Mutex::new(SearchStatus::Uninitialized),
            last_cancel: Mutex::new(None),
            pump: Mutex::new(None),
            building: AtomicBool::new(false),
        });

        let bootstrap = Arc::clone(&inner);
        // 起不来也不影响后续调用：状态会一直是 Uninitialized，搜索会报 NotReady
        let _ = std::thread::Builder::new()
            .name("rastflow-index-bootstrap".to_string())
            .spawn(move || {
                if let Err(err) = bootstrap_engine(&bootstrap) {
                    eprintln!("[rastflow] 文件索引初始化失败：{err}");
                    bootstrap.set_status(SearchStatus::Unavailable {
                        reason: err.to_string(),
                        needs_admin: is_access_denied(&err),
                    });
                }
            });

        inner
    });

    let _ = inner; // 状态由 status() 读取
}

/// 当前状态
pub fn status() -> SearchStatus {
    match STATE.get() {
        Some(inner) => inner.status(),
        None => SearchStatus::Uninitialized,
    }
}

/// 当前进程是否以管理员身份运行
///
/// 直接查令牌的 `TokenElevation`，比「试着打开卷然后看报错」更准，
/// 也能在建索引之前就给出提示。
pub fn is_admin() -> bool {
    unsafe {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// 以管理员身份重新启动自己。成功返回 `true`，调用方随后应退出当前实例。
pub fn relaunch_as_admin() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    // 把原始参数带上，避免重启后丢掉命令行开关
    let args: Vec<String> = std::env::args().skip(1).collect();
    let params = if args.is_empty() {
        None
    } else {
        // ShellExecuteW 要的是整条命令行字符串，含空格的参数需要自己加引号
        Some(
            args.iter()
                .map(|arg| {
                    if arg.contains(' ') {
                        format!("\"{arg}\"")
                    } else {
                        arg.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
        )
    };

    let operation = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let parameters = params.as_ref().map(|value| wide(value));

    unsafe {
        let result = ShellExecuteW(
            None,
            PCWSTR(operation.as_ptr()),
            PCWSTR(file.as_ptr()),
            parameters
                .as_ref()
                .map(|value| PCWSTR(value.as_ptr()))
                .unwrap_or(PCWSTR::null()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        // 返回值 > 32 才算成功（ShellExecuteW 的历史约定）
        result.0 as usize > 32
    }
}

/// 请求重建索引（丢开旧记录重扫）。已有任务在跑时直接忽略。
///
/// 会把状态**同步**置为 [`SearchStatus::Indexing`]，这样调用方紧接着去轮询状态时
/// 一定能看到「正在建」，不会有「还没开始就以为已经结束」的竞态。
pub fn rebuild_index() {
    let Some(inner) = STATE.get() else {
        return;
    };
    if inner.building.swap(true, Ordering::SeqCst) {
        return; // 已经在建了
    }
    inner.set_status(SearchStatus::Indexing {
        disk: None,
        written: 0,
    });

    let task = Arc::clone(inner);
    let spawned = std::thread::Builder::new()
        .name("rastflow-index-rebuild".to_string())
        .spawn(move || {
            let result = build_index(&task);
            task.building.store(false, Ordering::SeqCst);
            if let Err(err) = result {
                eprintln!("[rastflow] 重建索引失败：{err}");
                task.set_status(SearchStatus::Unavailable {
                    reason: err.to_string(),
                    needs_admin: is_access_denied(&err),
                });
            }
        })
        .is_ok();
    if !spawned {
        inner.building.store(false, Ordering::SeqCst);
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
    let engine = inner
        .engine
        .get()
        .cloned()
        .ok_or(SearchError::NotReady)?;

    // 新搜索到来时取消上一次，避免上一轮的扫描继续白占 CPU
    let session = engine.search(SearchQuery::parse(query));
    let cancel = session.cancel_handle();
    if let Ok(mut previous) = inner.last_cancel.lock() {
        if let Some(old) = previous.replace(cancel) {
            old.store(true, Ordering::Relaxed);
        }
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

/// 启动时该做什么 —— 由「有没有索引 / 有没有可索引的盘 / 是不是管理员」三者决定。
///
/// 抽成纯函数是为了让这条权限策略有个明确的、可测的位置：它决定了非管理员
/// 用户到底能不能用文件搜索（能搜已有索引，但不能建索引、不能实时监控）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartupAction {
    /// 已有索引，直接可用（试图顺带启监控）
    Search,
    /// 没有索引但可以建
    Build,
    /// 没有索引且没有管理员权限：必须提权
    NeedAdmin,
    /// 没有可索引的盘
    NoDisks,
}

fn startup_action(indexed: bool, has_disks: bool, is_admin: bool) -> StartupAction {
    if !has_disks {
        return StartupAction::NoDisks;
    }
    if indexed {
        // 有索引就能搜，哪怕不是管理员（只是拿不到实时增量）
        return StartupAction::Search;
    }
    if is_admin {
        StartupAction::Build
    } else {
        StartupAction::NeedAdmin
    }
}

/// 打开引擎；有索引就先补课，补不了再重建
fn bootstrap_engine(inner: &Arc<Inner>) -> crate::search::Result<()> {
    let engine = Arc::new(Engine::open(CoreConfig::new(index_dir()))?);
    // 引擎先装好：即使最终状态不可用，搜索侧的报错也能更具体
    let _ = inner.engine.set(Arc::clone(&engine));

    if engine.disks().is_empty() {
        inner.set_status(SearchStatus::Unavailable {
            reason: "没有找到可用于索引的本地 NTFS 磁盘".to_string(),
            needs_admin: false,
        });
        return Ok(());
    }

    let admin = is_admin();
    let mut action = startup_action(engine.is_indexed(), true, admin);

    // 有索引时先「补课」：把程序没运行期间发生的变更从 USN 日志重放进来。
    // 没有这一步，关着程序改过的文件就只能等下次全量重扫才会出现。
    if let StartupAction::Search = action {
        match engine.catch_up(&mut |_| {}) {
            CatchUp::Replayed { events } if events > 0 => {
                eprintln!("[rastflow] 从 USN 日志补齐了 {events} 条变更");
            }
            CatchUp::NeedsRebuild => {
                eprintln!("[rastflow] USN 日志已覆盖上次位点，需要重扫全盘");
                action = if admin {
                    StartupAction::Build
                } else {
                    StartupAction::NeedAdmin
                };
            }
            CatchUp::Unavailable(reason) => {
                // 非管理员很常见。索引停在快照时的状态，但搜索仍可用，
                // 过期的条目会被搜索时的「懒删除」顺手清掉。
                eprintln!("[rastflow] 无法补齐离线变更（结果可能滞后）：{reason}");
            }
            _ => {}
        }
    }

    match action {
        StartupAction::NoDisks => {
            // 上面已经判过 disks 非空，这里只是把枚举穷尽掉
            inner.set_status(SearchStatus::Unavailable {
                reason: "没有找到可用于索引的本地 NTFS 磁盘".to_string(),
                needs_admin: false,
            });
        }
        StartupAction::Search => {
            let monitoring = ensure_monitor(inner, &engine);
            inner.set_status(SearchStatus::Ready { monitoring });
        }
        StartupAction::NeedAdmin => {
            inner.set_status(SearchStatus::Unavailable {
                reason: "首次建立索引需要管理员权限".to_string(),
                needs_admin: true,
            });
        }
        StartupAction::Build => {
            if inner.building.swap(true, Ordering::SeqCst) {
                return Ok(());
            }
            let result = build_index(inner);
            inner.building.store(false, Ordering::SeqCst);
            result?;
        }
    }
    Ok(())
}

/// 建索引并实时回报进度
fn build_index(inner: &Arc<Inner>) -> crate::search::Result<()> {
    let Some(engine) = inner.engine.get().cloned() else {
        return Ok(());
    };
    inner.set_status(SearchStatus::Indexing {
        disk: None,
        written: 0,
    });

    let report = engine.build_index(Some(&mut |progress| {
        inner.set_status(SearchStatus::Indexing {
            disk: Some(progress.disk),
            written: progress.written,
        });
    }))?;

    eprintln!(
        "[rastflow] 索引完成：{} 条记录，用时 {} ms",
        report.total_written(),
        report.elapsed_ms
    );

    let monitoring = ensure_monitor(inner, &engine);
    inner.set_status(SearchStatus::Ready { monitoring });
    Ok(())
}

/// 保证增量监控在跑（幂等），返回监控当前是否可用。
///
/// 已经在跑就原样留着：重建索引只是重扫 MFT，不动 USN 位点，而这条泵一直跟着日志走。
/// 换一条新的反而会把重建期间积在通道里的事件丢掉，还要重建一次各盘的卷句柄。
///
/// 非管理员直接返回 `false`：读 USN 日志要 `GENERIC_WRITE`（见 [`crate::search::win::volume`]），
/// 起了也只会白失败一次。权限判断只留在这里一处，两个调用点（引导与重建）写法一致。
fn ensure_monitor(inner: &Arc<Inner>, engine: &Arc<Engine>) -> bool {
    if !is_admin() {
        return false;
    }

    // 「查活 → 起新的 → 记下来」整段都在锁里：两条建索引路径可能同时走到这里，
    // 不锁就会各起一条。泵线程自己不碰这把锁（它只用索引与 `status`），
    // 所以下面 join 已结束的线程不会死锁。
    let mut slot = inner
        .pump
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if pump_is_live(&mut slot) {
        return true;
    }

    let monitor = match engine.start_monitor() {
        Ok(monitor) => monitor,
        Err(err) => {
            eprintln!("[rastflow] 文件监控未启动（搜索结果可能滞后）：{err}");
            return false;
        }
    };

    let pump_engine = Arc::clone(engine);
    let pump_inner = Arc::clone(inner);
    let spawned = std::thread::Builder::new()
        .name("rastflow-index-pump".to_string())
        .spawn(move || {
            // join() 需要取得所有权，binding 本身不需要 mut
            let monitor: MonitorHandle = monitor;
            // 「最后一次变更」的时刻；为 None 表示没有待落盘的改动
            let mut dirty_since: Option<Instant> = None;

            // 唯一的出口是「各盘监控线程都没了」：那时通道断开，不会再有事件到来
            loop {
                match monitor.recv_timeout(PUMP_TICK) {
                    Ok(event) => {
                        // 单条事件失败不值得中断整条泵：索引会在下次重建时自愈
                        match pump_engine.apply_event(&event) {
                            Ok(true) => dirty_since = Some(Instant::now()),
                            Ok(false) => {}
                            Err(err) => eprintln!("[rastflow] 应用文件变更失败：{err}"),
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        // 空闲下来一段时间了，把内存索引落一份快照
                        if snapshot_due(dirty_since, Instant::now()) {
                            save_snapshot(&pump_engine);
                            dirty_since = None;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        eprintln!("[rastflow] 文件监控已全部停止，索引不再跟随磁盘变化");
                        pump_inner.mark_monitoring_stopped();
                        break;
                    }
                }
            }

            // 退出前把关：把最后的变更落盘，下次启动就能少扫一遍 MFT
            if dirty_since.is_some() {
                save_snapshot(&pump_engine);
            }
            monitor.join();
        });

    match spawned {
        Ok(join) => {
            *slot = Some(join);
            true
        }
        Err(err) => {
            // 闭包连同捕获的 `monitor` 一起被丢弃 → `MonitorHandle::drop` 会停掉各盘监控线程
            eprintln!("[rastflow] 监控泵线程创建失败：{err}");
            false
        }
    }
}

/// 槽位里是否已经有一条活着的监控泵。
///
/// 活着就原样留着，调用方据此**复用**而不是再起一条；已经结束的（各盘监控线程全挂
/// 之后就是这个状态）顺手 `join` 回收并清空槽位，让调用方起一条新的。
fn pump_is_live(slot: &mut Option<JoinHandle<()>>) -> bool {
    if slot.as_ref().is_some_and(|pump| !pump.is_finished()) {
        return true;
    }
    if let Some(finished) = slot.take() {
        let _ = finished.join();
    }
    false
}

/// 是不是到落盘的时候了
fn snapshot_due(dirty_since: Option<Instant>, now: Instant) -> bool {
    match dirty_since {
        Some(at) => now.duration_since(at) >= SNAPSHOT_IDLE,
        None => false,
    }
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

/// 判断错误是不是「权限不足」（CreateFileW 打开卷被拒）
fn is_access_denied(err: &crate::search::CoreError) -> bool {
    const ERROR_ACCESS_DENIED: i32 = 5;
    match err {
        crate::search::CoreError::Win32 { err, .. } => err.code().0 == ERROR_ACCESS_DENIED,
        _ => false,
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
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
    fn startup_searches_without_admin_when_indexed() {
        // 这是整个权限设计里最关键的一条：非管理员不该被完全挡在门外
        assert_eq!(startup_action(true, true, false), StartupAction::Search);
        assert_eq!(startup_action(true, true, true), StartupAction::Search);
    }

    #[test]
    fn startup_build_requires_admin_without_index() {
        assert_eq!(startup_action(false, true, true), StartupAction::Build);
        assert_eq!(startup_action(false, true, false), StartupAction::NeedAdmin);
    }

    #[test]
    fn startup_reports_missing_disks_first() {
        // 即使有索引/是管理员，没有可索引的盘也没意义
        assert_eq!(startup_action(true, false, true), StartupAction::NoDisks);
        assert_eq!(startup_action(false, false, true), StartupAction::NoDisks);
        assert_eq!(startup_action(false, false, false), StartupAction::NoDisks);
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

    #[test]
    fn empty_pump_slot_reports_not_live() {
        let mut slot: Option<JoinHandle<()>> = None;
        assert!(!pump_is_live(&mut slot), "还没有泵时应当报告为不存在");
        assert!(slot.is_none());
    }

    #[test]
    fn live_pump_is_reused() {
        // 还在跑的泵代表「监控正在工作」：必须原样留着让调用方复用，
        // 换一条新的会把重建索引期间积在通道里的事件丢掉。
        let mut slot: Option<JoinHandle<()>> = Some(std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(300));
        }));
        assert!(pump_is_live(&mut slot), "还在跑的泵应当报告为存活");
        assert!(slot.is_some(), "存活时不能把句柄挪走");

        let handle = slot.take().expect("句柄应当还在");
        let _ = handle.join();
    }

    #[test]
    fn finished_pump_is_reaped() {
        // 泵是在「各盘监控线程全挂」时退出的，之后重建索引必须能起一条新的，
        // 所以已结束的句柄要被清掉（否则会一直被当成「在跑」）。
        let mut slot: Option<JoinHandle<()>> = Some(std::thread::spawn(|| {}));
        std::thread::sleep(Duration::from_millis(200));

        assert!(!pump_is_live(&mut slot), "已结束的泵应当让位");
        assert!(slot.is_none(), "已结束的句柄应当被清空");
    }

    #[test]
    fn is_admin_probe_does_not_panic() {
        // is_admin() 在测试环境通常是 false，这里只断言它不崩
        let _ = is_admin();
    }

    #[test]
    fn wide_appends_nul_terminator() {
        let value = wide("ab");
        assert_eq!(value, vec![97, 98, 0]);
    }
}
