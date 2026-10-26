//! rastflow 自研文件搜索核心：枚举 NTFS 主文件表建立常驻内存的索引，阻塞读 USN
//! 日志维持增量。不依赖第三方索引服务，也不依赖数据库。
//!
//! **打开卷句柄要 `GENERIC_WRITE`（管理员）**，所以真正碰 MFT/USN 的代码跑在一个
//! 提权的子进程（[`helper_main`]）里，本进程只通过命名管道收记录与事件
//! （[`helper`]）。索引、快照与搜索始终留在主进程，子进程不持有索引。
//!
//! `unsafe` 只允许出现在 [`win`] 里。
//!
//! 模块划分、索引布局、生命周期与并发约束见 `docs/search-engine.md`。

pub mod engine;
pub mod helper;
pub mod helper_main;
pub mod index;
pub mod matcher;
pub mod monitor;
pub mod pathutil;
pub mod priority;
mod scheduler;
pub mod win;

pub type Result<T> = std::result::Result<T, CoreError>;

/// Win32 失败时保留 `windows::core::Error`，并额外记下是哪个 API 失败 ——
/// 光有错误码很难定位问题。
#[derive(Debug)]
pub enum CoreError {
    /// 读写索引快照、目录遍历等
    Io(std::io::Error),
    Win32 {
        /// 失败的系统调用，如 `"DeviceIoControl(FSCTL_ENUM_USN_DATA)"`
        api: &'static str,
        err: windows::core::Error,
    },
    /// 不是「本地固定盘 + NTFS」，无法用 MFT/USN 方式索引
    UnsupportedDisk {
        letter: char,
        reason: String,
    },
    /// 参数不合法（如空关键字、超过长度上限）
    InvalidArgument(String),
    /// 用户在 UAC 提示上拒绝了提权（或提权被策略禁止）
    ElevationDenied,
    /// 与索引子进程的通信出问题（管道断开、握手失败、超时）
    Helper(String),
}

impl CoreError {
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
            Self::ElevationDenied => write!(f, "用户取消了管理员权限请求"),
            Self::Helper(msg) => write!(f, "索引子进程：{msg}"),
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
