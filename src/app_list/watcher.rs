//! 开始菜单目录监听：新装/卸载程序时使发现缓存失效

use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 共享标记：watcher 发现变化时设为 true，UI 线程消费后设为 false
pub type DirtyFlag = Arc<AtomicBool>;

/// 启动后台 watcher，监听开始菜单目录变化。
///
/// 返回 `DirtyFlag`：每次检测到有效变化时会被设为 `true`。
/// watcher 本身在后台线程中运行，是常驻的。
#[cfg(windows)]
pub fn start() -> Option<DirtyFlag> {
    let roots = start_menu_roots();
    if roots.is_empty() {
        return None;
    }

    let dirty = Arc::new(AtomicBool::new(false));
    let dirty_clone = dirty.clone();

    let mut watcher = RecommendedWatcher::new(
        move |res: Result<notify::Event, notify::Error>| {
            let Ok(event) = res else { return };
            // 只关心会改变程序列表的事件
            let dominated = matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(
                    notify::event::ModifyKind::Name(_)
                )
            );
            if !dominated {
                return;
            }
            // 只关心 .lnk / .url 文件
            let dominated = event.paths.iter().any(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| {
                        let e = e.to_ascii_lowercase();
                        e == "lnk" || e == "url"
                    })
                    .unwrap_or(false)
            });
            if !dominated {
                return;
            }
            dirty_clone.store(true, Ordering::Relaxed);
        },
        Config::default().with_poll_interval(Duration::from_millis(200)),
    )
    .ok()?;

    for root in &roots {
        if watcher.watch(root, RecursiveMode::Recursive).is_err() {
            eprintln!("[rastflow] 无法监听目录: {}", root.display());
        }
    }

    // 把 watcher 泄漏到静态生命周期，让它常驻后台
    // 这是合理的：watcher 一旦启动就应该活到进程结束
    Box::leak(Box::new(watcher));

    Some(dirty)
}

#[cfg(not(windows))]
pub fn start() -> Option<DirtyFlag> {
    None
}

/// 开始菜单的两个根目录（公共 + 用户）
#[cfg(windows)]
fn start_menu_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(program_data) = std::env::var("ProgramData") {
        roots.push(
            PathBuf::from(program_data)
                .join("Microsoft\\Windows\\Start Menu\\Programs"),
        );
    }
    if let Ok(app_data) = std::env::var("APPDATA") {
        roots.push(
            PathBuf::from(app_data)
                .join("Microsoft\\Windows\\Start Menu\\Programs"),
        );
    }
    roots
}
