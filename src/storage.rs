//! 单写入者使用的原子文件替换，保留上一代用于损坏恢复。
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}
pub fn atomic_replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = sibling(path, ".tmp");
    let mut file = std::fs::File::create(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    if path.exists() {
        let backup_temp = sibling(path, ".bak.tmp");
        std::fs::copy(path, &backup_temp)?;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&backup_temp)?
            .sync_all()?;
        std::fs::rename(&backup_temp, sibling(path, ".bak"))?;
    }
    std::fs::rename(temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacement_keeps_previous_generation_and_ignores_unpublished_temp() {
        let root = std::env::temp_dir().join(format!(
            "rastflow-atomic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("state");
        atomic_replace(&path, b"first").unwrap();
        atomic_replace(&path, b"second").unwrap();
        std::fs::write(sibling(&path, ".tmp"), b"partial").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert_eq!(std::fs::read(sibling(&path, ".bak")).unwrap(), b"first");
        std::fs::remove_dir_all(root).unwrap();
    }
}
