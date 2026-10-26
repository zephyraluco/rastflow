//! 有界滚动诊断日志。不记录查询输入，不转储环境变量。
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
const MAX_BYTES: u64 = 2 * 1024 * 1024;
static PATH: OnceLock<PathBuf> = OnceLock::new();
static WRITER: Mutex<()> = Mutex::new(());
pub fn initialize(path: PathBuf) {
    let _ = PATH.set(path);
}
pub fn write(arguments: std::fmt::Arguments<'_>) {
    let Some(path) = PATH.get() else {
        std::eprintln!("{arguments}");
        return;
    };
    let _guard = WRITER.lock().unwrap_or_else(|e| e.into_inner());
    let _ = append(path, &arguments.to_string());
}
fn append(path: &Path, message: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() >= MAX_BYTES) {
        let previous = crate::storage::sibling(path, ".1");
        match std::fs::remove_file(&previous) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        std::fs::rename(path, previous)?;
    }
    let mut end = message.len().min(16 * 1024);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    writeln!(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?,
        "[{time}] {}",
        &message[..end]
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn log_rotation_and_large_unicode_messages_are_bounded() {
        let root = std::env::temp_dir().join(format!(
            "rastflow-log-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("desktop.log");
        std::fs::write(&path, vec![b'x'; MAX_BYTES as usize]).unwrap();
        append(&path, &"测试".repeat(20000)).unwrap();
        assert_eq!(
            std::fs::metadata(crate::storage::sibling(&path, ".1"))
                .unwrap()
                .len(),
            MAX_BYTES
        );
        assert!(std::fs::read_to_string(&path).unwrap().len() < 17000);
        std::fs::remove_dir_all(root).unwrap();
    }
}
