use std::env;
use std::path::PathBuf;

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is not set"));
    let sdk_dir = manifest_dir.join("Everything-SDK");
    let lib_dir = sdk_dir.join("lib");

    println!("cargo:rerun-if-changed={}", lib_dir.display());
    println!("cargo:rustc-link-search=native={}", lib_dir.display());

    let sdk_name = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86") => "Everything32",
        Ok("x86_64") => "Everything64",
        Ok("arm") => "EverythingARM",
        Ok("aarch64") => "EverythingARM64",
        Ok(arch) => panic!("unsupported Windows target architecture for Everything SDK: {arch}"),
        Err(_) => panic!("CARGO_CFG_TARGET_ARCH is not set"),
    };
    println!("cargo:rustc-link-lib=static={sdk_name}");
}
