//! rastflow 自研文件搜索核心。
//!
//! 枚举 NTFS 的 MFT 建立常驻内存的索引，阻塞式读 USN 日志维持实时增量，
//! 搜索时顺序扫描索引并逐条匹配。不依赖第三方索引服务，也不依赖数据库。
//!
//! | 模块 | 职责 | 要管理员？ |
//! | --- | --- | --- |
//! | [`win`] | 全部 Win32 交互：卷句柄与 IOCTL、USN 记录与 FRN、盘类型、已知文件夹 | 部分 |
//! | [`pathutil`] | 路径纯函数与忽略规则 | 否 |
//! | [`priority`] | 后缀优先级表 | 否 |
//! | [`index`] | 内存索引、快照编解码，以及从 MFT 建它 | 建库阶段 |
//! | [`monitor`] | 阻塞式读 USN 日志，产出结构化的增量事件 | 是 |
//! | [`matcher`] | 查询解析与匹配规则 | 否 |
//! | [`engine`] | 搜索调度与索引生命周期 | 建库阶段 |
//!
//! `unsafe` 只出现在 [`win`] 里。
//!
//! # 使用
//!
//! ```no_run
//! use std::path::PathBuf;
//! use std::sync::Arc;
//! use std::time::Duration;
//!
//! use crate::search::engine::{CoreConfig, Engine};
//! use crate::search::matcher::SearchQuery;
//!
//! # fn main() -> Result<(), crate::search::CoreError> {
//! let engine = Arc::new(Engine::open(CoreConfig::new(PathBuf::from(r"D:\rastflow\index")))?);
//! engine.build_index(None)?;            // 需要管理员权限，放后台线程里跑
//! let session = engine.search(SearchQuery::parse("notepad"));
//! if session.wait(Duration::from_secs(2)) {
//!     for path in session.snapshot() {
//!         println!("{path}");
//!     }
//! }
//! # Ok(())
//! # }
//! ```

pub mod engine;
pub mod index;
pub mod matcher;
pub mod monitor;
pub mod pathutil;
pub mod priority;
pub mod win;

/// 核心模块统一结果类型
pub type Result<T> = std::result::Result<T, CoreError>;

/// 核心模块的错误类型。
///
/// Win32 调用失败时保留 `windows::core::Error`，并额外记录是哪个 API 失败 ——
/// 光有错误码很难定位问题。
#[derive(Debug)]
pub enum CoreError {
    /// 普通 IO（读写索引快照、目录遍历等）
    Io(std::io::Error),
    /// Win32 API 失败
    Win32 {
        /// 失败的系统调用，如 `"DeviceIoControl(FSCTL_ENUM_USN_DATA)"`
        api: &'static str,
        /// `windows` crate 捕获的 `GetLastError`
        err: windows::core::Error,
    },
    /// 目标盘不是「本地固定盘 + NTFS」，无法用 MFT/USN 方式索引
    UnsupportedDisk {
        /// 盘符，如 `'C'`
        letter: char,
        /// 原因说明
        reason: String,
    },
    /// 参数不合法（如空关键字、超过长度上限）
    InvalidArgument(String),
}

impl CoreError {
    /// 构造一个 `Win32` 错误
    pub(crate) fn win32(api: &'static str, err: windows::core::Error) -> Self {
        Self::Win32 { api, err }
    }

    /// 构造一个 `UnsupportedDisk` 错误
    pub(crate) fn unsupported(letter: char, reason: impl Into<String>) -> Self {
        Self::UnsupportedDisk {
            letter,
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IO 错误：{e}"),
            Self::Win32 { api, err } => write!(f, "{api} 失败：{err}"),
            Self::UnsupportedDisk { letter, reason } => {
                write!(f, "{letter}: 盘不可用：{reason}")
            }
            Self::InvalidArgument(msg) => write!(f, "参数不合法：{msg}"),
        }
    }
}

impl std::error::Error for CoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Win32 { err, .. } => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CoreError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

/// 单次搜索默认最大结果数
pub const DEFAULT_MAX_RESULTS: usize = 200;

/// 搜索串长度上限，超过就直接放弃本次搜索
pub const MAX_SEARCH_TEXT_LEN: usize = 300;

/// MFT 枚举缓冲区：`sizeof(USN) + 1MB`
pub const MFT_BUFFER_LEN: usize = 8 + 0x10_0000;

/// USN 读取缓冲区：512 KB
pub const USN_BUFFER_LEN: usize = 1024 * 1024 / 2;

/// 路径回溯的最大层数。异常的 FRN 链会让回溯不收敛，有上限就不会死循环
pub const MAX_PATH_DEPTH: usize = 256;
