use std::env;
use std::path::{Path, PathBuf};

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is not set"));

    embed_windows_resources(&manifest_dir);
}

/// 把应用图标与版本信息写进 exe 的资源段。
///
/// 不做这一步的话，打包安装后的 `rastflow.exe` 在资源管理器、任务栏和
/// Alt-Tab 里都是默认的空白图标（cargo-packager 不会改写已编译的 exe）。
///
/// 注意：这里**不能**再写自定义清单。rustc 链接 Windows 可执行文件时已经会内嵌
/// 一份默认清单（含 asInvoker 与 DPI 感知），再通过 winresource 加一份会得到
/// `CVTRES error CVT1100: 资源重复。类型: MANIFEST，名称: 1` → 链接失败。
fn embed_windows_resources(manifest_dir: &Path) {
    let icon = manifest_dir.join("assets").join("app-icon.ico");
    // 图标随仓库提交；缺失时不应让整个构建失败，只是 exe 没有图标而已。
    if !icon.exists() {
        println!(
            "cargo:warning=缺少 {}，exe 将不带应用图标（托盘也会因此不可用）",
            icon.display()
        );
        return;
    }

    println!("cargo:rerun-if-changed={}", icon.display());

    let version = env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(icon.to_str().expect("图标路径不是合法 UTF-8"));
    resource.set("ProductName", "rastflow");
    resource.set("FileDescription", "rastflow — Windows 快速启动器");
    resource.set("ProductVersion", &version);
    resource.set("FileVersion", &version);

    if let Err(err) = resource.compile() {
        // 仅作为提示：编译失败多半是缺 rc.exe（Windows SDK），
        // 此时仍希望普通 cargo build 能继续。
        println!("cargo:warning=嵌入 exe 图标/版本信息失败: {err}");
    }
}
