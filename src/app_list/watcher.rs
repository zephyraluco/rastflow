//! 原生目录通知驱动应用目录刷新；接收方拥有 watcher，关闭时自动释放。
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::sync::OnceLock;

static CHANGES: OnceLock<(async_channel::Sender<()>, async_channel::Receiver<()>)> = OnceLock::new();
fn changes() -> &'static (async_channel::Sender<()>, async_channel::Receiver<()>) {
    CHANGES.get_or_init(|| async_channel::bounded(1))
}
pub fn invalidate() { let _ = changes().0.try_send(()); }

pub struct CatalogWatcher {
    _watcher: Option<RecommendedWatcher>,
    changes: async_channel::Receiver<()>,
}
impl CatalogWatcher {
    pub async fn changed(&self) -> bool { self.changes.recv().await.is_ok() }
    pub fn drain(&self) { while self.changes.try_recv().is_ok() {} }
}
pub fn start() -> CatalogWatcher {
    let watcher = RecommendedWatcher::new(|result: Result<notify::Event, notify::Error>| {
        match result {
            Ok(event) if matches!(event.kind, EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_)) => invalidate(),
            Err(error) => { eprintln!("[rastflow] 应用目录通知失败：{error}"); invalidate(); }
            _ => {}
        }
    }, Config::default());
    let watcher = watcher.ok().map(|mut watcher| {
        for root in [crate::search::win::known_folder::start_menu(), crate::search::win::known_folder::common_start_menu()].into_iter().flatten() {
            if root.exists() && watcher.watch(&root, RecursiveMode::Recursive).is_err() {
                eprintln!("[rastflow] 无法监听目录：{}", root.display());
            }
        }
        watcher
    });
    CatalogWatcher { _watcher: watcher, changes: changes().1.clone() }
}
