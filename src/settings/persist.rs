//! 版本化用户偏好；串行后台保存，原子替换并保留上一代。
use super::global::AppSettings;
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock, mpsc};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedSettings {
    pub version: u32,
    pub theme: String,
    pub language: String,
    pub hotkey: String,
}
impl Default for PersistedSettings {
    fn default() -> Self {
        Self {
            version: 1,
            theme: "system".into(),
            language: "zh".into(),
            hotkey: "alt+space".into(),
        }
    }
}
pub fn settings_path() -> std::path::PathBuf {
    crate::utils::app_data_dir().join("settings.json")
}

pub fn load_settings() -> PersistedSettings {
    let path = settings_path();
    for candidate in [path.clone(), crate::storage::sibling(&path, ".bak")] {
        if let Ok(bytes) = std::fs::read(candidate)
            && let Ok(settings) = serde_json::from_slice::<PersistedSettings>(&bytes)
                && settings.version == 1 {
                    return settings;
                }
    }
    PersistedSettings::default()
}

enum Command {
    Save(PersistedSettings),
    Flush(mpsc::Sender<()>),
}
static WRITER: OnceLock<mpsc::Sender<Command>> = OnceLock::new();
static PREVIOUS: Mutex<Option<PersistedSettings>> = Mutex::new(None);
fn writer() -> &'static mpsc::Sender<Command> {
    WRITER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("rastflow-settings".into())
            .spawn(move || {
                for command in receiver {
                    match command {
                        Command::Save(settings) => {
                            match serde_json::to_vec_pretty(&settings)
                                .map_err(std::io::Error::other)
                                .and_then(|bytes| {
                                    crate::storage::atomic_replace(&settings_path(), &bytes)
                                }) {
                                Ok(()) => {}
                                Err(error) => eprintln!("[rastflow] 保存设置失败：{error}"),
                            }
                        }
                        Command::Flush(reply) => {
                            let _ = reply.send(());
                        }
                    }
                }
            })
            .expect("cannot start settings writer");
        sender
    })
}
pub fn save_settings(settings: &AppSettings) {
    let next = PersistedSettings {
        version: 1,
        theme: settings.theme.to_string(),
        language: settings.language.to_string(),
        hotkey: settings.hotkey.to_string(),
    };
    let mut previous = PREVIOUS.lock().unwrap_or_else(|e| e.into_inner());
    if previous.as_ref() == Some(&next) {
        return;
    }
    if writer().send(Command::Save(next.clone())).is_ok() {
        *previous = Some(next);
    }
}
pub fn flush() {
    if let Some(writer) = WRITER.get() {
        let (reply, done) = mpsc::channel();
        if writer.send(Command::Flush(reply)).is_ok() {
            let _ = done.recv_timeout(std::time::Duration::from_secs(5));
        }
    }
}
