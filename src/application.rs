//! 桌面事件入口。平台回调只投递事件，GPUI 主线程统一处理。
use std::sync::OnceLock;

pub enum Event {
    Tray(crate::tray::TrayEvent),
    Hotkey(global_hotkey::GlobalHotKeyEvent),
    SettingsChanged,
    UpdaterChanged,
}

static EVENTS: OnceLock<async_channel::Sender<Event>> = OnceLock::new();
pub fn initialize() -> async_channel::Receiver<Event> {
    let (sender, receiver) = async_channel::unbounded();
    EVENTS
        .set(sender)
        .ok()
        .expect("desktop event loop initialized twice");
    receiver
}
pub fn send(event: Event) {
    if let Some(sender) = EVENTS.get() {
        let _ = sender.try_send(event);
    }
}
