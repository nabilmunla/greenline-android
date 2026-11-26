//! Compiles engine/csrc/fdstream.c (task 2bf.4: open a PDF from an fd through a pread-backed
//! fz_stream) against the MuPDF headers that ship inside mupdf-sys 0.8.0, exactly as mupdf-sys
//! compiles its own wrapper (`-I mupdf/include`, no defines). Pinned to 0.8.0 / MuPDF 1.27.2: a
//! different mupdf-sys must be checked by hand before this is changed.

use std::path::PathBuf;

fn mupdf_include() -> PathBuf {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
        .expect("CARGO_HOME or HOME");
    let reg = home.join("registry").join("src");
    for idx in std::fs::read_dir(&reg).expect("cargo registry src") {
        let inc = idx.expect("registry entry").path().join("mupdf-sys-0.8.0").join("mupdf").join("include");
        if inc.join("mupdf").join("fitz.h").is_file() {
            return inc;
        }
    }
    panic!("mupdf-sys-0.8.0 headers not found under {}", reg.display());
}

fn main() {
    println!("cargo:rerun-if-changed=csrc/fdstream.c");
    println!("cargo:rerun-if-changed=build.rs");
    cc::Build::new()
        .file("csrc/fdstream.c")
        .include(mupdf_include())
        .warnings(false)
        .compile("gl-fdstream");
}
