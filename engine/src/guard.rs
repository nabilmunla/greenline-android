//! The memory guard (task 2b.4): limits, the trim flag, MuPDF's resource store, and direct draws.
//!
//! - **Store cap.** MuPDF's resource store (fonts, decoded images, ...) is capped at 256 MB. The `mupdf`
//!   0.8 crate creates its base context with `FZ_STORE_DEFAULT` (mupdf-sys `wrapper/context.c`) and has no
//!   way to change it afterwards, so the cap is asserted instead of set: at compile time against
//!   `mupdf_sys::FZ_STORE_DEFAULT`, and at run time by reading the live store's `max` ([`store_max_bytes`]).
//! - **List guard.** While a page is read into a display list, the worker's watcher thread samples the
//!   process RSS every [`GUARD_EVERY_MS`] ms. If RSS has grown more than [`GuardConfig::list_guard_mb`]
//!   since the read started (or passes the soft limit, or a trim arrives), the read is aborted through
//!   the shared cookie with a reason ([`MemReason`]); the worker drops the partial list, empties the
//!   store, trims the heap and draws that page directly (no list) until another page is shown.
//! - **Soft limit.** Between steps the worker checks RSS against [`GuardConfig::soft_limit_mb`]; above it,
//!   the list is dropped and the store emptied.
//! - **Trim.** Android's `onTrimMemory`, `onLowMemory` and `onStop` call [`request_trim`] (from JNI, on the
//!   UI thread). It records the kind in an `AtomicU8`, bumps a sequence number and wakes every running
//!   engine; the engine drops its list and empties the store, and the watcher aborts a read in flight.
//!   It never calls MuPDF itself.
//!
//! Nothing here logs. MuPDF calls in this module run on the calling thread's MuPDF context: the worker
//! only calls them from the engine thread.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use mupdf::{Device, Matrix, Pixmap};

use crate::{Bounds, Cookie, Doc, Rendered};

/// How often the watcher samples RSS while a page is read into a display list, in ms.
pub const GUARD_EVERY_MS: u64 = 50;

/// MuPDF's resource store cap: 256 MB.
pub const STORE_CAP_BYTES: usize = 256 << 20;

// The `mupdf` crate's base context is created with FZ_STORE_DEFAULT; that must be the 256 MB cap.
const _: () = assert!(mupdf_sys::FZ_STORE_DEFAULT as usize == STORE_CAP_BYTES);

/// Default list guard: a read may grow RSS by at most this much (MB).
pub const DEFAULT_LIST_GUARD_MB: f64 = 700.0;
/// Default main-process soft limit (MB): about 400 MB left for the WebView and Java under 1.5 GB.
pub const DEFAULT_SOFT_LIMIT_MB: f64 = 1100.0;

pub use crate::api::MemReason;

/// The guard's limits, fixed when an engine starts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GuardConfig {
    /// Abort a display-list read that has grown RSS by more than this (MB). 0 disables the list guard.
    pub list_guard_mb: f64,
    /// Main-process RSS above which the list is dropped and the store emptied (MB). 0 disables it.
    pub soft_limit_mb: f64,
}

impl Default for GuardConfig {
    fn default() -> Self {
        GuardConfig { list_guard_mb: DEFAULT_LIST_GUARD_MB, soft_limit_mb: DEFAULT_SOFT_LIMIT_MB }
    }
}

impl GuardConfig {
    /// The defaults, overridden by `GL_LIST_GUARD_MB` and `GL_SOFT_LIMIT_MB` (whole or decimal MB; `0`
    /// disables that check). Unparsable or negative values are ignored.
    pub fn from_env() -> GuardConfig {
        GuardConfig::default().with_env()
    }

    /// Limits that step up with the phone's RAM (task 2c.2b 26 Nov 2026). On the 16 GB test
    /// phone the 700 MB list guard abandoned a real 65 MB sheet's list (its list is ~530 MB on the host,
    /// plus up to 256 MB of store), so every sharpen was drawn directly: ~30 s.
    /// - under 10,000 MB or unknown: 700 / 1,100 (the 8 GB target, unchanged)
    /// - 10,000–13,999 MB: 1,100 / 1,700
    /// - 14,000 MB and over: 1,500 / 2,300
    pub fn for_ram_mb(ram_mb: Option<u32>) -> GuardConfig {
        let (list, soft) = match ram_mb {
            Some(r) if r >= 14_000 => (1_500.0, 2_300.0),
            Some(r) if r >= 10_000 => (1_100.0, 1_700.0),
            _ => (DEFAULT_LIST_GUARD_MB, DEFAULT_SOFT_LIMIT_MB),
        };
        GuardConfig { list_guard_mb: list, soft_limit_mb: soft }
    }

    /// [`for_ram_mb`](Self::for_ram_mb) for this device (MemTotal from /proc/meminfo), then the
    /// `GL_LIST_GUARD_MB` / `GL_SOFT_LIMIT_MB` overrides. What the app starts its engine with.
    pub fn for_device() -> GuardConfig {
        GuardConfig::for_ram_mb(crate::results::device().ram_mb).with_env()
    }

    fn with_env(self) -> GuardConfig {
        let mut c = self;
        if let Some(v) = env_mb("GL_LIST_GUARD_MB") {
            c.list_guard_mb = v;
        }
        if let Some(v) = env_mb("GL_SOFT_LIMIT_MB") {
            c.soft_limit_mb = v;
        }
        c
    }
}

fn env_mb(name: &str) -> Option<f64> {
    parse_mb(&std::env::var(name).ok()?)
}

fn parse_mb(s: &str) -> Option<f64> {
    let v: f64 = s.trim().parse().ok()?;
    (v.is_finite() && v >= 0.0).then_some(v)
}

// ---------------------------------------------------------------------------------------------
// trim flag (set from JNI)

/// What Android asked for. Stored as a `u8` in [`trim_kind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TrimKind {
    /// `onTrimMemory` (any level).
    TrimMemory = 1,
    /// `onLowMemory`.
    LowMemory = 2,
    /// `onStop` (the app went to the background, or the SAF picker covered it). Light.
    Background = 3,
    /// `onTrimMemory` below `TRIM_MEMORY_BACKGROUND` (e.g. `UI_HIDDEN`): no real pressure. Light.
    Light = 4,
}

impl TrimKind {
    /// From the integer Kotlin passes; anything unknown counts as `TrimMemory`.
    pub fn from_i32(v: i32) -> TrimKind {
        match v {
            2 => TrimKind::LowMemory,
            3 => TrimKind::Background,
            4 => TrimKind::Light,
            _ => TrimKind::TrimMemory,
        }
    }

    /// Hard trims (real memory pressure) drop the display list and abort a read in flight. Light ones
    /// (task 2bf.2: the picker fires them on every open) only empty MuPDF's store.
    pub fn is_hard(self) -> bool {
        matches!(self, TrimKind::TrimMemory | TrimKind::LowMemory)
    }
}

/// The last trim kind requested (0 = none yet). The JNI-visible atomic of the plan.
static TRIM_KIND: AtomicU8 = AtomicU8::new(0);
/// Trim requests so far. Each engine remembers the last one it handled.
static TRIM_SEQ: AtomicU64 = AtomicU64::new(0);

type Waker = Arc<dyn Fn() + Send + Sync>;
static WAKERS: Mutex<Vec<(u64, Waker)>> = Mutex::new(Vec::new());
static NEXT_WAKER: AtomicU64 = AtomicU64::new(1);

/// Hard trim requests so far ([`TrimKind::is_hard`]).
static HARD_SEQ: AtomicU64 = AtomicU64::new(0);

/// Tell every running engine about a trim. Safe from any thread (the JNI hook calls it on Android's UI
/// thread); never blocks on the engine and never calls MuPDF. A light trim empties MuPDF's store; a hard
/// one also drops the display list and aborts a read in flight (the page is then read again).
pub fn request_trim(kind: TrimKind) {
    TRIM_KIND.store(kind as u8, SeqCst);
    if kind.is_hard() {
        HARD_SEQ.fetch_add(1, SeqCst);
    }
    TRIM_SEQ.fetch_add(1, SeqCst);
    let wakers: Vec<Waker> = lock_wakers().iter().map(|(_, w)| w.clone()).collect();
    for w in wakers {
        w();
    }
}

/// The last trim kind requested, as its `u8` code (0 if none yet).
pub fn trim_kind() -> u8 {
    TRIM_KIND.load(SeqCst)
}

/// Number of trim requests so far.
pub fn trim_seq() -> u64 {
    TRIM_SEQ.load(SeqCst)
}

/// Number of hard trim requests so far.
pub fn hard_trim_seq() -> u64 {
    HARD_SEQ.load(SeqCst)
}

fn lock_wakers() -> std::sync::MutexGuard<'static, Vec<(u64, Waker)>> {
    WAKERS.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn add_waker(w: Waker) -> u64 {
    let id = NEXT_WAKER.fetch_add(1, SeqCst);
    lock_wakers().push((id, w));
    id
}

pub(crate) fn remove_waker(id: u64) {
    lock_wakers().retain(|(i, _)| *i != id);
}

// ---------------------------------------------------------------------------------------------
// MuPDF's resource store

// `mupdf::Context` is `struct Context { inner: *mut fz_context }` with no Drop (mupdf 0.8.0,
// src/context.rs): the pointer is its only field. Same check as the worker's cookie view.
const _: () = assert!(std::mem::size_of::<mupdf::Context>() == std::mem::size_of::<*mut mupdf_sys::fz_context>());

/// This thread's MuPDF context pointer (created on first use, like every `mupdf` call does).
pub(crate) fn ctx() -> *mut mupdf_sys::fz_context {
    let c = mupdf::Context::get();
    // SAFETY: see the size assert above; `Context` has no Drop, so nothing is freed twice.
    unsafe { std::mem::transmute::<mupdf::Context, *mut mupdf_sys::fz_context>(c) }
}

/// The head of MuPDF 1.27's `struct fz_store` (source/fitz/store.c), which mupdf-sys only binds as an
/// opaque type. Only `max` and `size` are read. The app pins MuPDF 1.27.2 (`MUPDF_VERSION` in the app,
/// checked at run time against the linked library).
#[repr(C)]
struct StoreHead {
    refs: std::ffi::c_int,
    head: *mut c_void,
    tail: *mut c_void,
    hash: *mut c_void,
    max: usize,
    size: usize,
}

/// (max, size) of the shared store, in bytes. `size` is read without MuPDF's allocation lock, so it is
/// a gauge, not an exact figure, if another thread is using MuPDF at the same moment.
fn store_numbers() -> (usize, usize) {
    let c = ctx();
    // SAFETY: `fz_keep_store_context` returns the context's store with one more reference (NULL if the
    // context has none), so the memory stays alive until the matching drop below.
    unsafe {
        let s = mupdf_sys::fz_keep_store_context(c) as *const StoreHead;
        if s.is_null() {
            return (0, 0);
        }
        let max = std::ptr::read_volatile(std::ptr::addr_of!((*s).max));
        let size = std::ptr::read_volatile(std::ptr::addr_of!((*s).size));
        mupdf_sys::fz_drop_store_context(c);
        (max, size)
    }
}

/// The live store cap in bytes (expected: [`STORE_CAP_BYTES`]).
pub fn store_max_bytes() -> usize {
    store_numbers().0
}

/// Bytes currently held in the store (approximate, see above).
pub fn store_size_bytes() -> usize {
    store_numbers().1
}

/// Evict everything MuPDF's store can evict (items still in use stay). Call [`crate::mem::trim`] after
/// it to hand the memory back to the kernel.
pub fn empty_store() {
    // SAFETY: a valid context for this thread; fz_empty_store takes MuPDF's own lock.
    unsafe { mupdf_sys::fz_empty_store(ctx()) }
}

// ---------------------------------------------------------------------------------------------
// direct draws (no display list), with a cookie

fn ms_since(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// The whole page at `width` px wide, drawn straight from the page content with a cookie (progress and
/// abort). Same size and placement as [`Doc::render_fit`] and `DisplayList::render_fit`. An aborted
/// draw may still return `Ok` with a partial image: the caller checks its own abort flag.
pub fn direct_fit(doc: &Doc, page: i32, width: u32, cookie: &Cookie) -> crate::Result<Rendered> {
    direct_fit_bounds(doc, page, width, cookie).map(|(r, _)| r)
}

/// [`direct_fit`] plus the page bounds it drew (points), for the `pt_w` / `pt_h` of the worker's
/// `LowRes` and `Fit` events in direct mode.
pub fn direct_fit_bounds(doc: &Doc, page: i32, width: u32, cookie: &Cookie) -> crate::Result<(Rendered, Bounds)> {
    let (p, b, load_ms) = doc.load(page)?;
    let s = crate::fit_scale(&b, width)?;
    let r = crate::fit_irect(&b, s)?;
    let t = Instant::now();
    let mut pm = Pixmap::new_with_rect(&doc.rgb, r, false)?;
    pm.clear_with(255)?;
    {
        let d = Device::from_pixmap(&pm)?;
        p.run_with_cookie(&d, &Matrix::new_scale(s, s), cookie)?;
    }
    Ok((crate::finish(&pm, load_ms, ms_since(t))?, b))
}

/// A `TILE_SIZE` tile at `zoom` with its top-left pixel at (`x0`, `y0`), drawn straight from the page
/// content with a cookie. Same placement as [`Doc::render_tile_at`]; same abort rule as [`direct_fit`].
pub fn direct_tile(doc: &Doc, page: i32, zoom: f32, x0: i32, y0: i32, cookie: &Cookie) -> crate::Result<Rendered> {
    direct_region(doc, page, zoom, x0, y0, crate::TILE_SIZE, crate::TILE_SIZE, cookie)
}

/// [`direct_tile`] for a `w` x `h` region (task 2c.2).
#[allow(clippy::too_many_arguments)]
pub fn direct_region(doc: &Doc, page: i32, zoom: f32, x0: i32, y0: i32, w: u32, h: u32, cookie: &Cookie) -> crate::Result<Rendered> {
    crate::check_zoom(zoom)?;
    let (p, _b, load_ms) = doc.load(page)?;
    let t = Instant::now();
    let tile = crate::new_region(&doc.rgb, x0, y0, w, h)?;
    {
        let d = Device::from_pixmap(&tile)?;
        p.run_with_cookie(&d, &Matrix::new_scale(zoom, zoom), cookie)?;
    }
    crate::finish(&tile, load_ms, ms_since(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mb_values_parse() {
        assert_eq!(parse_mb("50"), Some(50.0));
        assert_eq!(parse_mb(" 1100.5 "), Some(1100.5));
        assert_eq!(parse_mb("0"), Some(0.0));
        assert_eq!(parse_mb("-1"), None);
        assert_eq!(parse_mb("lots"), None);
        assert_eq!(parse_mb("inf"), None);
    }

    #[test]
    fn reasons_round_trip() {
        for r in [MemReason::ListGuard, MemReason::SoftLimit, MemReason::Trim] {
            assert_eq!(MemReason::from_code(r.code()), Some(r));
        }
        assert_eq!(MemReason::from_code(0), None);
        assert_eq!(TrimKind::from_i32(2), TrimKind::LowMemory);
        assert_eq!(TrimKind::from_i32(99), TrimKind::TrimMemory);
        assert_eq!(TrimKind::from_i32(4), TrimKind::Light);
        // 2c.2b: limits by RAM
        let at = |r: Option<u32>| {
            let c = GuardConfig::for_ram_mb(r);
            (c.list_guard_mb, c.soft_limit_mb)
        };
        assert_eq!(at(None), (700.0, 1_100.0));
        assert_eq!(at(Some(0)), (700.0, 1_100.0));
        assert_eq!(at(Some(7_680)), (700.0, 1_100.0));
        assert_eq!(at(Some(9_999)), (700.0, 1_100.0));
        assert_eq!(at(Some(10_000)), (1_100.0, 1_700.0));
        assert_eq!(at(Some(13_999)), (1_100.0, 1_700.0));
        assert_eq!(at(Some(14_000)), (1_500.0, 2_300.0));
        assert_eq!(at(Some(15_200)), (1_500.0, 2_300.0));
        assert_eq!(GuardConfig::default(), GuardConfig::for_ram_mb(None));
        assert!(TrimKind::TrimMemory.is_hard() && TrimKind::LowMemory.is_hard());
        assert!(!TrimKind::Background.is_hard() && !TrimKind::Light.is_hard());
    }

    /// The transmuted context pointer is this thread's MuPDF context: a setting made through the
    /// `mupdf` crate reads back through the raw pointer.
    #[test]
    fn context_pointer_is_the_threads_context() {
        crate::set_aa_level(4);
        let raw = unsafe { mupdf_sys::fz_aa_level(ctx()) };
        crate::set_aa_level(8);
        let raw8 = unsafe { mupdf_sys::fz_aa_level(ctx()) };
        assert_eq!((raw, raw8), (4, 8));
    }

    #[test]
    fn store_cap_is_256_mb() {
        assert_eq!(store_max_bytes(), STORE_CAP_BYTES);
        empty_store();
        assert!(store_size_bytes() <= STORE_CAP_BYTES);
    }
}
