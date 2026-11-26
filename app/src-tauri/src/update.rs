//! Task 2b.9: the in-app updater's commands. The glfile plugin (Kotlin GlUpdater) does the work: it
//! asks this app's own GitHub repository for the latest release, downloads the APK into the app's
//! private cache, checks it is this app with a higher version and the same signing certificates, and
//! opens Android's installer. These commands pass through fixed codes only.

use tauri::AppHandle;
use tauri_plugin_glfile::GlFileExt;

/// `{available, version?, size_mb?}`: is a newer release published?
#[tauri::command]
pub async fn update_check(app: AppHandle) -> Result<serde_json::Value, &'static str> {
    app.glfile().update("checkUpdate").await
}

/// Download and verify the offered update (resolves when done).
#[tauri::command]
pub async fn update_download(app: AppHandle) -> Result<serde_json::Value, &'static str> {
    app.glfile().update("downloadUpdate").await
}

/// `{done, total}` bytes of the download in progress.
#[tauri::command]
pub async fn update_progress(app: AppHandle) -> Result<serde_json::Value, &'static str> {
    app.glfile().update("updateProgress").await
}

/// Open Android's installer (`{opened: true}`), or the "install unknown apps" setting (`{opened: false}`).
#[tauri::command]
pub async fn update_install(app: AppHandle) -> Result<serde_json::Value, &'static str> {
    app.glfile().update("installUpdate").await
}
