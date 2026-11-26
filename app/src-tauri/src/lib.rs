//! Greenline Android: a Tauri 2 shell around greenline-engine (MuPDF).
//!
//! The engine worker (`greenline_engine::worker`) is the single owner of all MuPDF state (`Doc` is
//! !Send and MuPDF's context is thread-local). `host.rs` starts it with a dispatcher thread that
//! routes its events; `protocol.rs` serves tiles and fit images to the viewer over the async `gl`
//! scheme; `open.rs` holds the SAF open and the viewer's commands (task 2b.6); `measure.rs` the
//! measurement screen's commands and its numbers-only results file (task 2b.7).
//! Confidentiality: nothing here logs paths, file names or page text.

mod host;
mod jni;
mod measure;
mod open;
mod protocol;

use std::ffi::CString;

use serde::Serialize;
use tauri::Manager;

/// MuPDF release bundled by mupdf-sys 0.8.0 (include/mupdf/fitz/version.h). mupdf-sys blocklists
/// FZ_VERSION in its bindings, so it cannot be read at runtime; `mupdf_version_matches` checks it
/// against the linked library instead.
const MUPDF_VERSION: &str = "1.27.2";

#[derive(Serialize)]
struct EngineInfo {
    app_version: &'static str,
    /// Kernel page size in bytes (4096 or 16384 on Android 15+ devices).
    page_size: usize,
    /// Resident set size of the app's main process, MB. The WebView renders in another process.
    rss_mb: f64,
    mupdf_version: &'static str,
    /// True when the linked MuPDF library accepted MUPDF_VERSION as its header version.
    mupdf_version_ok: bool,
}

/// fz_new_context_imp refuses (returns NULL) when the caller's header version differs from the
/// linked library's FZ_VERSION, so creating and dropping a context both proves MuPDF runs on the
/// device and confirms the version string.
fn mupdf_version_matches(version: &str) -> bool {
    let Ok(v) = CString::new(version) else {
        return false;
    };
    unsafe {
        let ctx = mupdf_sys::fz_new_context_imp(std::ptr::null(), std::ptr::null(), 0, v.as_ptr());
        if ctx.is_null() {
            return false;
        }
        mupdf_sys::fz_drop_context(ctx);
    }
    true
}

#[tauri::command]
fn engine_info() -> EngineInfo {
    EngineInfo {
        app_version: env!("CARGO_PKG_VERSION"),
        page_size: greenline_engine::mem::page_size(),
        rss_mb: greenline_engine::mem::rss_mb(),
        mupdf_version: MUPDF_VERSION,
        mupdf_version_ok: mupdf_version_matches(MUPDF_VERSION),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default().plugin(tauri_plugin_glfile::init()).setup(|app| {
        let host = host::start(app.handle().clone())?;
        app.manage(host);
        // Records a run the last session left unfinished as a probable kill. Not fatal: without it only
        // the measurement screen's commands fail (with Tauri's "state not managed").
        if let Ok(m) = measure::init(app.handle()) {
            app.manage(m);
        }
        Ok(())
    });
    protocol::register(builder)
        .invoke_handler(tauri::generate_handler![
            engine_info,
            open::open_picked,
            open::open_incoming,
            open::open_recent,
            open::list_recents,
            open::remove_recent,
            open::set_last_page,
            open::save_thumb,
            open::default_pdf_app,
            open::move_to_back,
            open::close_doc,
            open::show_page,
            open::cancel,
            measure::measure_start,
            measure::measure_plan,
            measure::measure_step,
            measure::measure_end,
            measure::measure_state,
            measure::measure_export
        ])
        .run(tauri::generate_context!())
        .expect("error while running Greenline");
}
