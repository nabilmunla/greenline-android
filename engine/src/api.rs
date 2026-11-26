//! The engine's message API: what the UI side sends ([`Request`]) and what comes back ([`Event`]).
//!
//! Plain data only (integers, floats, byte vectors, one `String` path), no MuPDF types, no references,
//! no channels: so a later split into a separate engine process only has to serialise these two enums.
//! Error events carry a fixed [`ErrorCode`], never free text (MuPDF's messages can contain file paths).
//!
//! Ordering rules the worker guarantees (see `worker.rs`):
//! - `ShowPage` emits `Progress`* (the first within 1 s, then at least every 250 ms while a step runs),
//!   then `LowRes`, then `Fit`. `LowRes` always comes before `Fit`.
//! - `Cancel` is answered by exactly one `Cancelled`. Everything requested before the `Cancel` has
//!   finished or been stopped by then; no event for an older `ShowPage` follows the `Cancelled`.
//!   Tiles stopped by a cancel are answered with `TileSkipped`.
//! - Every `Tile` request is answered by exactly one of `Tile`, `TileSkipped` or `Error{tile: Some(id)}`.
//! - Memory guard (`guard.rs`): `DirectMode` comes before the first directly drawn image of its page;
//!   `MemoryReleased` can arrive at any time (also while idle, after an Android trim).

/// Requests to the engine thread.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    /// Open a document (a filesystem path or `/proc/self/fd/N`). Closes the current one first.
    /// Answered by `Opened` or `Error{code: OpenFailed}`.
    Open { path: String },
    /// Drop the document and its display list. No answer.
    Close,
    /// Read `page` (0-based) into the display list (dropping any list of another page), then answer
    /// with `LowRes` (about [`LOWRES_WIDTH`] px wide) and `Fit` (`fit_width` px wide).
    ShowPage { page: i32, fit_width: u32 },
    /// One `TILE_SIZE` x `TILE_SIZE` tile at `zoom` with its top-left pixel at (`x0`, `y0`).
    /// `id` is the caller's correlation id (echoed back). `gen` is the view generation: once any tile with
    /// a higher `gen` has been received, queued tiles with a lower `gen` are answered with `TileSkipped`
    /// without being rendered.
    Tile { id: u64, page: i32, zoom: f32, x0: i32, y0: i32, gen: u64 },
    /// Like `Tile`, but a `w` x `h` region (each side 1..=`MAX_REGION`): the viewer asks for the whole
    /// visible area in one render (task 2c.2). Answered by `Tile` (with `w`, `h`) or `TileSkipped`.
    Region { id: u64, page: i32, zoom: f32, x0: i32, y0: i32, w: u32, h: u32, gen: u64 },
    /// Stop everything requested so far (the in-flight read or render is aborted through the shared
    /// cookie, not through the queue). Answered by `Cancelled`.
    Cancel,
    /// Drop the display list and give the freed memory back (memory pressure). Answered by `Dropped`.
    DropList,
    /// Stop the engine thread. No answer; the event channel closes when the thread has exited.
    Shutdown,
}

/// Which step of a `ShowPage` a `Progress` event is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Reading the page into the display list.
    Read,
    /// Rendering the low-res fit image from the list.
    LowRes,
    /// Rendering the full fit image from the list.
    Fit,
}

/// Events from the engine thread.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The document is open.
    Opened { pages: i32, open_ms: f64 },
    /// A `ShowPage` step is still running. `ops` is MuPDF's cookie progress counter for that step
    /// (operators or list nodes done; there is no known total). `elapsed_ms` counts from the moment the
    /// engine started the `ShowPage`.
    Progress { page: i32, stage: Stage, ops: u64, elapsed_ms: f64 },
    /// Low-res fit image. `list_ms` is the read into the display list, `ms` the render (plus RGBA
    /// conversion), `total_ms` from the engine starting the `ShowPage` to this image being ready.
    /// `pt_w` x `pt_h` is the page size in points (`Doc::page_bounds`), so the UI can place tiles at any
    /// zoom (a tile at zoom z covers 1024 / z points); the same in direct mode.
    LowRes { page: i32, rgba: Vec<u8>, w: u32, h: u32, ms: f64, list_ms: f64, total_ms: f64, pt_w: f32, pt_h: f32 },
    /// Full-width fit image, same fields as `LowRes`.
    Fit { page: i32, rgba: Vec<u8>, w: u32, h: u32, ms: f64, list_ms: f64, total_ms: f64, pt_w: f32, pt_h: f32 },
    /// A rendered tile (always `TILE_SIZE` x `TILE_SIZE` RGBA).
    Tile { id: u64, gen: u64, page: i32, rgba: Vec<u8>, w: u32, h: u32, render_ms: f64 },
    /// A tile request answered without rendering: superseded by a newer `gen`, or stopped by a cancel.
    TileSkipped { id: u64, gen: u64 },
    /// Answer to `Cancel`.
    Cancelled,
    /// Answer to `DropList`. `had_list` is false if there was no list to drop.
    Dropped { had_list: bool },
    /// A request failed. `tile` is the tile id when the failed request was a `Tile`.
    Error { code: ErrorCode, tile: Option<u64> },
    /// The memory guard abandoned the display-list read of `page` (partial list dropped, MuPDF's store
    /// emptied, heap trimmed; `rss_mb` is the main-process RSS after that). From now on `page` is in
    /// direct mode: its `LowRes`, `Fit` and tiles are drawn straight from the page content (slower per
    /// image, far less memory) until another page is shown. Sent once, before the first direct image;
    /// a `ShowPage` whose read was abandoned still delivers `LowRes` and `Fit` after it.
    DirectMode { page: i32, reason: MemReason, rss_mb: f64 },
    /// The engine dropped its display list (`had_list`) and emptied MuPDF's store on its own, not in
    /// answer to `DropList`: Android's trim hooks (`Trim`) or the main-process soft limit (`SoftLimit`).
    /// Only sent when something was held. `rss_mb` is the RSS afterwards.
    MemoryReleased { reason: MemReason, had_list: bool, rss_mb: f64 },
}

/// Why the memory guard stepped in. Fixed codes (numbers only, for the measurement screen).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MemReason {
    /// A display-list read grew RSS by more than the list guard (`GL_LIST_GUARD_MB`, default 700).
    ListGuard = 1,
    /// The main process passed the soft limit (`GL_SOFT_LIMIT_MB`, default 1100).
    SoftLimit = 2,
    /// Android asked the app to release memory (onTrimMemory, onLowMemory, onStop).
    Trim = 3,
}

impl MemReason {
    pub fn code(self) -> u8 {
        self as u8
    }

    pub(crate) fn from_code(c: u8) -> Option<MemReason> {
        match c {
            1 => Some(MemReason::ListGuard),
            2 => Some(MemReason::SoftLimit),
            3 => Some(MemReason::Trim),
            _ => None,
        }
    }
}

/// Fixed error codes. No text, no paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum ErrorCode {
    /// The request needs an open document and there is none.
    NoDocument = 1,
    /// MuPDF could not open the file (missing, unreadable, not a PDF, ...).
    OpenFailed = 2,
    /// Page index outside 0..pages.
    PageOutOfRange = 3,
    /// Invalid parameters (zero width, bad zoom, degenerate page, ...).
    InvalidRequest = 4,
    /// MuPDF failed while reading the page into the display list.
    ReadFailed = 5,
    /// MuPDF failed while rendering.
    RenderFailed = 6,
}

impl ErrorCode {
    pub fn code(self) -> u16 {
        self as u16
    }
}

/// Width of the low-res fit image, in pixels (never wider than the requested fit width).
pub const LOWRES_WIDTH: u32 = 600;

/// While a step runs, a `Progress` event is sent every this many milliseconds (the first one this long
/// after the step starts); every read also ends with one final `Progress`.
pub const PROGRESS_EVERY_MS: u64 = 200;
