//! `SHGetKnownFolderPath` 封装：按 GUID 取系统的已知目录。
//!
//! 预扫四个：当前用户开始菜单、所有用户开始菜单、当前用户桌面、公共桌面。

use std::path::PathBuf;

use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FOLDERID_CommonStartMenu, FOLDERID_Desktop, FOLDERID_PublicDesktop, FOLDERID_StartMenu,
    KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};
use windows::core::GUID;

/// 取某个已知文件夹的路径；不存在或调用失败返回 `None`
pub fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let pwstr = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let path = pwstr.to_string().ok().map(PathBuf::from);
        // SHGetKnownFolderPath 分配的内存必须由调用方释放
        CoTaskMemFree(Some(pwstr.0 as *const std::ffi::c_void));
        path
    }
}

/// 当前用户开始菜单
pub fn start_menu() -> Option<PathBuf> {
    known_folder(&FOLDERID_StartMenu)
}

/// 所有用户（公共）开始菜单
pub fn common_start_menu() -> Option<PathBuf> {
    known_folder(&FOLDERID_CommonStartMenu)
}

/// 当前用户桌面
pub fn desktop() -> Option<PathBuf> {
    known_folder(&FOLDERID_Desktop)
}

/// 公共桌面
pub fn public_desktop() -> Option<PathBuf> {
    known_folder(&FOLDERID_PublicDesktop)
}

/// 预备搜索要预扫的全部目录
pub fn shortcut_roots() -> Vec<PathBuf> {
    [
        start_menu(),
        common_start_menu(),
        desktop(),
        public_desktop(),
    ]
    .into_iter()
    .flatten()
    .collect()
}
