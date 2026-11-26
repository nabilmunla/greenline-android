//! Hands the Kotlin library in `android/` to tauri-build (see `links` in Cargo.toml).
//!
//! This is the one thing `tauri_plugin::Builder::android_path("android").build()` does that this
//! plugin needs. The plugin exposes no JS commands (the app's own commands call it from Rust), so
//! there are no permission files to generate and the `tauri-plugin` build crate is not needed.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        let dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("android");
        println!("cargo:android_library_path={}", dir.display());
    }
}
