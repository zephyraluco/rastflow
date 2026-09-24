//! 单实例守卫
//!
//! 常驻托盘的程序被重复启动时，正确行为是**把已有实例的窗口唤出来**，
//! 而不是再开一份。多开一份的代价都是实打实的：
//!
//! - 两枚托盘图标抢同一个位置；
//! - 两个进程同时写索引快照文件；
//! - 全局快捷键只有先注册的那个生效（后注册的直接失败）；
//! - 自动升级期间尤其要紧 —— 安装器会按镜像名结束进程再重启，这中间用户
//!   顺手点一次图标就可能多出一个实例（见 [`crate::tray::activate_existing`]）。
//!
//! 做法是持有一个**具名互斥体**：「是否已有实例」这个事实由内核判定并保证，
//! 不存在「先检查、再创建」的竞态窗口。
//!
//! # 为什么必须一直持有句柄
//!
//! 互斥体对象在所有句柄关闭时被销毁。如果本进程用完就把句柄关掉，
//! 第三个实例启动时就会看到「互斥体不存在」并自认为第一个 —— 守卫随之失效。
//! 所以句柄要持有到进程结束，这也正是这里的语义：不做清理，交给内核在进程退出时回收。

use std::sync::atomic::{AtomicIsize, Ordering};

use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::{PCWSTR, w};

/// 互斥体名。
///
/// `Local\` 前缀把作用域限定在**当前登录会话**，这正是需要的粒度：多用户同时登录
/// （或 RDP 会话）时各人应当能各开一份，互不干扰。若用 `Global\`，
/// 第二个登录的用户会被拒绝启动。
///
/// 名字里的 `-single-instance` 是为了与将来可能引入的其他内核对象区分开。
const MUTEX_NAME: PCWSTR = w!("Local\\rastflow-single-instance");

/// 已持有的互斥体句柄，进程生命周期内不释放。
///
/// 存成 `isize` 而不是 `HANDLE`：静态量要求 `Sync`，而 `HANDLE` 不满足（它是个裸指针）。
/// 反正互斥体本来就要持有到进程结束，转成整数正好表达了「不再需要按句柄语义操作它」。
static GUARD: AtomicIsize = AtomicIsize::new(0);

/// 单实例检查的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// 本进程是第一个实例，互斥体归自己所有
    Primary,
    /// 已有实例在运行（其中包含「上个实例正在退出」这种边界情况）
    AlreadyRunning,
}

/// 取得单实例守卫。
///
/// 必须在 [`crate::main`] 的**最开头**调用：一旦判定为已有实例，
/// 就应该在接触 GPUI 之前退回，否则托盘图标与快捷键注册已经发生过了。
///
/// 失败时**故意按「第一个实例」放行**：创建互斥体失败（权限异常等）远不如
/// 「因为一个守卫而启动不了程序」严重。宁可偶发多开，也不要锁死。
pub fn acquire() -> Guard {
    acquire_named(MUTEX_NAME, &GUARD)
}

/// [`acquire`] 的实现。
///
/// `name` 与 `slot` 之所以做成参数，只是为了测试：生产用的名字是进程级的，
/// 本机只要开着 `rastflow.exe`（哪怕只是刚刚冒烟测试留下的那个），
/// 首次调用就会拿到 `AlreadyRunning`，测试于是变成假失败。
fn acquire_named(name: PCWSTR, slot: &AtomicIsize) -> Guard {
    let Ok(handle) = (unsafe { CreateMutexW(None, false, name) }) else {
        eprintln!("[rastflow] 创建单实例互斥体失败，按首个实例继续启动");
        return Guard::Primary;
    };

    // 名字已存在时 CreateMutexW **仍然成功返回**，只是给的是已有对象的另一个句柄，
    // 靠 GetLastError 区分这两种情况。
    //
    // 顺序要紧：GetLastError 必须在 CreateMutexW 之后、且中间不能插入其他
    // 可能改写它的 API 调用（windows-rs 的成功路径不会碰它，只读参数转换也不碰）。
    let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;

    if already_exists {
        // 这个句柄是我们自己开的，要关掉；对方的句柄还在，所以互斥体继续存在，
        // 守卫依然有效（第三个实例照样会被拦下）。
        unsafe {
            let _ = CloseHandle(handle);
        }
        Guard::AlreadyRunning
    } else {
        // 故意泄漏：见文件头「为什么必须一直持有句柄」
        slot.store(handle.0 as isize, Ordering::SeqCst);
        Guard::Primary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 守卫的核心语义：首个调用者拿到 `Primary` 并留住句柄，之后一律报告已有实例。
    ///
    /// 互斥体名带上进程号，测试因此**完全不依赖外部状态**：若沿用生产那个名字，
    /// 本机只要开着 `rastflow.exe`（本地开发时很常见），首次调用就会是
    /// `AlreadyRunning`，测试变成假失败 —— 这个坑实际踩到过一次。
    ///
    /// 顺带验证 `GetLastError` 的读取时机没错 —— 一旦在 `CreateMutexW` 与
    /// `GetLastError` 之间插入别的 API，第二次调用就会退化成 `Primary`，立即失败。
    #[test]
    fn guard_is_exclusive_within_process() {
        static SLOT: AtomicIsize = AtomicIsize::new(0);

        let name: Vec<u16> = format!(
            "Local\\rastflow-single-instance-test-{}",
            std::process::id()
        )
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
        let name = PCWSTR(name.as_ptr());

        assert_eq!(
            acquire_named(name, &SLOT),
            Guard::Primary,
            "首个调用者应当是主实例"
        );
        assert_ne!(
            SLOT.load(Ordering::SeqCst),
            0,
            "主实例必须一直持有互斥体句柄，否则后续实例会误判"
        );

        assert_eq!(
            acquire_named(name, &SLOT),
            Guard::AlreadyRunning,
            "重复获取必须报告已有实例（否则 GetLastError 读晚了）"
        );
        assert_eq!(
            acquire_named(name, &SLOT),
            Guard::AlreadyRunning,
            "而且应当持续报告，不只是第二次"
        );
    }

    // 跨进程语义（真的再启一个进程会被拦下）已手工验证：本机开着 rastflow.exe 时，
    // 测试进程的 acquire() 直接返回 AlreadyRunning，连它的互斥体都创建不出来。
    // 至于「第二个实例会把已有窗口唤出来」，也手工验证过：
    // 首次启动 MainWindowHandle 为 0（静默隐藏），第二次启动后变为非 0，进程数始终为 1。
}
