//! 链接 libzvec_c_api.dylib 的二进制必须自己声明 rpath：zvec-sys 的 `rustc-link-arg` 只作用于它自己的包（ADR 0003）。
//! 开发期指向 `ZVEC_LIB_DIR`（.cargo/config.toml → vendor/zvec/lib），打包后指向 .app 的 `Contents/Frameworks`。
use std::env;
use std::path::PathBuf;

fn main() {
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "macos" && os != "linux" {
        return;
    }
    if let Ok(dir) = env::var("ZVEC_LIB_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_dir() {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", dir.display());
        }
    }
    if os == "macos" {
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../Frameworks");
    } else {
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib");
    }
    println!("cargo:rerun-if-env-changed=ZVEC_LIB_DIR");
}
