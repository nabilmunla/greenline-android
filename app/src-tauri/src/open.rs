//! Open a PDF picked through Android's Storage Access Framework (task 2b.5), now opened by the engine
//! worker (task 2b.6).
//!
//! `open_picked`: the glfile plugin shows the system picker and hands back a detached fd plus the
//! provider authority; the fd is probed here (seekable? regular/pipe/socket? size). A non-seekable fd
//! is refused with a fixed code and no copy fallback. A seekable fd goes to the worker, which opens
//! `/proc/self/fd/N`; the `OwnedFd` stays in the [`Host`] until the worker is done with it.
//! `close_doc` closes the document (the fd closes once the worker has dropped it).
//!
//! Confidentiality: errors are fixed codes, never MuPDF text (which may contain paths); nothing here
//! logs. The only provider detail returned is the URI authority.

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Instant;

use greenline_engine::open::probe_fd;
use serde::Serialize;
use tauri::{AppHandle, State};
use tauri_plugin_glfile::GlFileExt;

use crate::host::Host;

/// Returned by `open_picked` on success.
#[derive(Serialize)]
pub struct OpenInfo {
    pub(crate) pages: i32,
    /// Document serial: the viewer's events (`gl-image`, `gl-progress`, ...) carry it as `doc`.
    pub(crate) doc: u64,
    pub(crate) seekable: bool,
    pub(crate) kind: &'static str,
    pub(crate) size_mb: f64,
    pub(crate) pick_to_fd_ms: f64,
    pub(crate) authority: String,
    /// MuPDF open + page count through the /proc/self/fd path, timed by the engine worker.
    pub(crate) open_ms: f64,
    /// Random id of the file's recent-list entry, if it has one (task 2bf.12).
    pub(crate) recent_id: Option<String>,
    /// The file was sent by another app (task 2bf.11): Back returns the user there.
    pub(crate) incoming: bool,
}

/// Where the document comes from (task 2bf.10). Every source goes through [`open_source`].
pub(crate) enum Source<'a> {
    Picker,
    Incoming,
    Recent(&'a str),
}

/// Returned by `open_picked` on failure: a fixed code plus whatever was measured before it failed,
/// so a refused (non-seekable) fd still reports its kind and provider.
#[derive(Serialize)]
pub struct OpenFailure {
    pub(crate) code: &'static str,
    pub(crate) seekable: Option<bool>,
    pub(crate) kind: Option<&'static str>,
    pub(crate) size_mb: Option<f64>,
    pub(crate) pick_to_fd_ms: Option<f64>,
    pub(crate) authority: Option<String>,
}

impl OpenFailure {
    fn code(code: &'static str) -> Self {
        OpenFailure { code, seekable: None, kind: None, size_mb: None, pick_to_fd_ms: None, authority: None }
    }
}

const E_NOT_SEEKABLE: &str = "E_NOT_SEEKABLE";
const E_OPEN_FAILED: &str = "E_OPEN_FAILED";
const E_BAD_FD: &str = "E_BAD_FD";
const E_ENGINE: &str = "E_ENGINE";

/// A finished pick and open: what `open_picked` returns, plus when the fd reached Rust (the start of
/// the measurement screen's first view).
pub(crate) struct Opened {
    pub(crate) info: OpenInfo,
    pub(crate) fd_at: Instant,
}

/// The SAF pick and the engine open, shared by the viewer (`open_picked`) and the measurement screen.
pub(crate) async fn pick_and_open(app: &AppHandle, host: &Host) -> Result<Opened, OpenFailure> {
    open_source(app, host, Source::Picker).await
}

/// One open path for every source (task 2bf.10): get a detached fd from the glfile plugin, then the
/// same probe, refusal and engine open for all of them.
pub(crate) async fn open_source(app: &AppHandle, host: &Host, source: Source<'_>) -> Result<Opened, OpenFailure> {
    let gl = app.glfile();
    let picked = match source {
        Source::Picker => gl.pick_pdf().await,
        Source::Incoming => gl.take_incoming().await,
        Source::Recent(id) => gl.open_recent(id).await,
    }
    .map_err(|e| OpenFailure::code(e.code()))?;
    // A file that just went into the recent list but then fails to open (not seekable, damaged) is
    // taken out again, so the list never offers a file that cannot open (2bf review finding 3).
    let new_entry = if picked.recent_new { picked.recent_id.clone() } else { None };
    let res = open_fd(host, picked).await;
    if res.is_err() {
        if let Some(id) = new_entry {
            let _ = gl.remove_recent(&id).await;
        }
    }
    res
}

/// The probe, refusal and engine open of a detached fd, the same for every source.
async fn open_fd(host: &Host, picked: tauri_plugin_glfile::Picked) -> Result<Opened, OpenFailure> {
    let fd_at = Instant::now();
    if picked.fd < 0 {
        return Err(OpenFailure::code(E_BAD_FD));
    }
    // Own the fd from here on: every early return below closes it.
    let fd = unsafe { OwnedFd::from_raw_fd(picked.fd) };
    let probe = probe_fd(picked.fd);
    let fail = |code: &'static str| OpenFailure {
        code,
        seekable: Some(probe.seekable),
        kind: Some(probe.kind.as_str()),
        size_mb: Some(probe.size_mb),
        pick_to_fd_ms: Some(picked.pick_to_fd_ms),
        authority: Some(picked.authority.clone()),
    };
    if !probe.seekable {
        return Err(fail(E_NOT_SEEKABLE));
    }
    let rx = host.open(fd).map_err(&fail)?;
    // Wait for the dispatcher's answer on a blocking-pool thread, off the async runtime.
    let opened = tauri::async_runtime::spawn_blocking(move || rx.recv())
        .await
        .map_err(|_| fail(E_ENGINE))?
        .map_err(|_| fail(E_ENGINE))?
        .map_err(|_| fail(E_OPEN_FAILED))?;
    let info = OpenInfo {
        pages: opened.pages,
        doc: opened.doc,
        seekable: probe.seekable,
        kind: probe.kind.as_str(),
        size_mb: probe.size_mb,
        pick_to_fd_ms: picked.pick_to_fd_ms,
        authority: picked.authority,
        open_ms: opened.open_ms,
        recent_id: picked.recent_id,
        incoming: picked.incoming,
    };
    Ok(Opened { info, fd_at })
}

#[tauri::command]
pub async fn open_picked(app: AppHandle, host: State<'_, Arc<Host>>) -> Result<OpenInfo, OpenFailure> {
    pick_and_open(&app, &host).await.map(|o| o.info)
}

/// Open the file another app sent, if one is waiting (`E_NONE` if not). The page calls this on load
/// and whenever it becomes visible (task 2bf.11).
#[tauri::command]
pub async fn open_incoming(app: AppHandle, host: State<'_, Arc<Host>>) -> Result<OpenInfo, OpenFailure> {
    open_source(&app, &host, Source::Incoming).await.map(|o| o.info)
}

/// Reopen a recent file by its random id (`E_NO_PERMISSION` if the provider took the permission back).
#[tauri::command]
pub async fn open_recent(app: AppHandle, host: State<'_, Arc<Host>>, id: String) -> Result<OpenInfo, OpenFailure> {
    open_source(&app, &host, Source::Recent(&id)).await.map(|o| o.info)
}

// Recent files for the start screen (task 2bf.12). The list holds display names: it is passed to the
// page untouched and nowhere else (never logged, never in exports or measurement records).

fn code_only<T>(r: Result<T, tauri_plugin_glfile::PickError>) -> Result<T, &'static str> {
    r.map_err(|e| e.code())
}

#[tauri::command]
pub async fn list_recents(app: AppHandle) -> Result<serde_json::Value, &'static str> {
    let v = code_only(app.glfile().list_recents().await)?;
    Ok(v.get("items").cloned().unwrap_or_else(|| serde_json::json!([])))
}

#[tauri::command]
pub async fn remove_recent(app: AppHandle, id: String) -> Result<bool, &'static str> {
    code_only(app.glfile().remove_recent(&id).await).map(|_| true)
}

#[tauri::command]
pub async fn set_last_page(app: AppHandle, id: String, page: i32, pages: i32) -> Result<bool, &'static str> {
    code_only(app.glfile().set_last_page(&id, page, pages).await).map(|_| true)
}

#[tauri::command]
pub async fn save_thumb(app: AppHandle, id: String, data: String) -> Result<bool, &'static str> {
    let v = code_only(app.glfile().save_thumb(&id, &data).await)?;
    Ok(v.get("saved").and_then(|s| s.as_bool()).unwrap_or(false))
}

#[tauri::command]
pub async fn default_pdf_app(app: AppHandle) -> Result<serde_json::Value, &'static str> {
    code_only(app.glfile().is_default_pdf_app().await)
}

#[tauri::command]
pub async fn move_to_back(app: AppHandle) -> Result<bool, &'static str> {
    code_only(app.glfile().move_to_back().await).map(|_| true)
}

/// Close the open document (and, once the worker has dropped it, its fd). Returns whether one was open.
#[tauri::command]
pub fn close_doc(host: State<'_, Arc<Host>>) -> bool {
    host.close()
}

/// Read `page` (0-based) and answer with `gl-progress` events, then `gl-image` low-res and fit.
/// False if the engine has stopped.
#[tauri::command]
pub fn show_page(host: State<'_, Arc<Host>>, page: i32, fit_width: u32) -> bool {
    host.show_page(page, fit_width)
}

/// Stop the current read or render (answered by a `gl-cancelled` event).
#[tauri::command]
pub fn cancel(host: State<'_, Arc<Host>>) -> bool {
    host.cancel()
}
