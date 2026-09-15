//! zvec-sys 的 build.rs 用 `cargo:rustc-link-arg` 加 rpath，但该指令只作用于它自己的包，
//! 不会传给下游二进制，所以链接 libzvec_c_api.dylib 的二进制必须自己声明 rpath。
//! aero-app 最终也要这样做：开发时指向 vendor 目录，打包后指向 .app 的 Frameworks。
use std::env;
use std::path::PathBuf;

fn main() {
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "macos" && os != "linux" {
        return;
    }
    // 1. 开发：ZVEC_LIB_DIR（.cargo/config.toml 里相对 workspace 根解析成绝对路径）
    if let Ok(dir) = env::var("ZVEC_LIB_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_dir() {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", dir.display());
        }
    }
    // 2. 打包：macOS .app 的 Contents/Frameworks；Linux AppImage 同目录 lib/
    if os == "macos" {
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../Frameworks");
    } else {
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib");
    }
    println!("cargo:rerun-if-env-changed=ZVEC_LIB_DIR");
}
