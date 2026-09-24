//! 盘类型判断：只有「本地固定盘 + NTFS」能用 MFT/USN 索引。

use windows::Win32::Storage::FileSystem::{
    GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
};
use windows::core::PCWSTR;

use super::super::Result;
use super::to_wide;

// winbase.h 的 DRIVE_* 常量（windows crate 把它们放在需要开大 feature 的模块里）
const DRIVE_UNKNOWN: u32 = 0;
const DRIVE_NO_ROOT_DIR: u32 = 1;
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_CDROM: u32 = 5;
const DRIVE_RAMDISK: u32 = 6;

/// `GetDriveTypeW`
pub fn drive_type(root: &str) -> u32 {
    let wide = to_wide(root);
    unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) }
}

/// 盘类型的可读名字（写日志用）
pub fn drive_type_name(value: u32) -> &'static str {
    match value {
        DRIVE_FIXED => "本地固定盘",
        DRIVE_REMOVABLE => "可移动盘",
        DRIVE_REMOTE => "网络盘",
        DRIVE_CDROM => "光驱",
        DRIVE_RAMDISK => "内存盘",
        DRIVE_NO_ROOT_DIR => "无根目录",
        DRIVE_UNKNOWN => "未知",
        _ => "其它",
    }
}

/// 卷的文件系统名（如 `NTFS`）；取不到返回 `None`
pub fn filesystem_name(root: &str) -> Option<String> {
    let wide = to_wide(root);
    let mut fs_name = [0u16; 64];
    let mut serial = 0u32;
    let mut max_component = 0u32;
    let mut flags = 0u32;
    unsafe {
        GetVolumeInformationW(
            PCWSTR(wide.as_ptr()),
            None,
            Some(&mut serial),
            Some(&mut max_component),
            Some(&mut flags),
            Some(&mut fs_name[..]),
        )
    }
    .ok()?;
    let name = super::from_wide_lossy(&fs_name);
    if name.is_empty() { None } else { Some(name) }
}

/// 是否是本地固定盘
pub fn is_fixed_drive(root: &str) -> bool {
    drive_type(root) == DRIVE_FIXED
}

/// 是否是 NTFS
pub fn is_ntfs(root: &str) -> bool {
    filesystem_name(root)
        .map(|name| name.eq_ignore_ascii_case("NTFS"))
        .unwrap_or(false)
}

/// 该盘能否用 MFT/USN 方式索引
pub fn is_supported(letter: char) -> bool {
    let root = root_of(letter);
    is_fixed_drive(&root) && is_ntfs(&root)
}

/// 不适用的原因（`is_supported` 为真时返回 `None`），用于生成 [`Result`] 错误
pub fn unsupported_reason(letter: char) -> Option<String> {
    let root = root_of(letter);
    let kind = drive_type(&root);
    if kind != DRIVE_FIXED {
        return Some(format!("不是本地固定盘（{}）", drive_type_name(kind)));
    }
    match filesystem_name(&root) {
        Some(name) if name.eq_ignore_ascii_case("NTFS") => None,
        Some(name) => Some(format!("文件系统不是 NTFS（{name}）")),
        None => Some("读不到卷信息".to_string()),
    }
}

/// 校验并返回错误，供建索引前调用
pub fn ensure_supported(letter: char) -> Result<()> {
    match unsupported_reason(letter) {
        None => Ok(()),
        Some(reason) => Err(super::CoreError::unsupported(letter, reason)),
    }
}

/// `'C'` → `"C:\\"`
pub fn root_of(letter: char) -> String {
    format!("{}:\\", letter.to_ascii_uppercase())
}

/// 系统上所有存在根目录的盘符（`GetLogicalDrives` 的位图）
pub fn logical_drives() -> Vec<char> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| (b'A' + bit as u8) as char)
        .collect()
}

/// 可以用于索引的盘符（本地固定盘 + NTFS，且跳过软驱位 A/B）
pub fn available_disks() -> Vec<char> {
    logical_drives()
        .into_iter()
        .filter(|letter| !matches!(letter, 'A' | 'B'))
        .filter(|letter| is_supported(*letter))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_of_completes_to_volume_root() {
        assert_eq!(root_of('c'), "C:\\");
        assert_eq!(root_of('D'), "D:\\");
    }

    #[test]
    fn drive_type_name_covers_all_constants() {
        for value in [
            DRIVE_UNKNOWN,
            DRIVE_NO_ROOT_DIR,
            DRIVE_REMOVABLE,
            DRIVE_FIXED,
            DRIVE_REMOTE,
            DRIVE_CDROM,
            DRIVE_RAMDISK,
        ] {
            assert_ne!(drive_type_name(value), "其它");
        }
        assert_eq!(drive_type_name(999), "其它");
    }

    #[test]
    fn missing_disk_is_not_usable() {
        // Z: 一般不存在（即使存在也未必是本地固定盘），这里只断言不会 panic
        let _ = is_supported('Z');
        let letter = 'Q';
        if let Some(reason) = unsupported_reason(letter) {
            assert!(!reason.is_empty());
        }
    }

    #[test]
    fn available_disks_returns_letters_only() {
        for letter in available_disks() {
            assert!(letter.is_ascii_uppercase());
            assert_ne!(letter, 'A');
            assert_ne!(letter, 'B');
        }
    }
}
