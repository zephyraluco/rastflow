//! 可独立验证的数据结构与纯逻辑；桌面与服务运行入口仅支持 Windows。
#[cfg(windows)]
macro_rules! eprintln {
    ($($argument:tt)*) => { crate::diagnostics::write(format_args!($($argument)*)) };
}

pub mod diagnostics;
pub mod search;

pub mod catalog;

pub mod storage;

pub mod cache;
