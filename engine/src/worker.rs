//! The engine worker: one thread that owns every piece of MuPDF state, fed by a request queue.
//!
//! - [`Engine::start`] spawns the engine thread (all MuPDF calls) and a small progress thread (no MuPDF
//!   calls: it only reads the shared cookie's counter and sends `Progress` events).
//! - [`EngineClient`] is the cheap, cloneable, `Send + Sync` sending side. Its [`cancel`](EngineClient::cancel)
//!   writes the abort flag of the shared cookie directly, so it works while the engine thread is busy
//!   inside MuPDF; the `Cancel` message it also queues is only the acknowledgement point.
//! - One display list at a time, dropped on page change, on `DropList`, `Open`, `Close` and when a read
//!   is cancelled (a cancelled read leaves a partial list: it is always dropped, then `mem::trim()`).
//!
//! # The memory guard (`guard.rs`)
//!
//! - While a page is read into the list, the progress thread also samples RSS every
//!   [`GUARD_EVERY_MS`](crate::guard::GUARD_EVERY_MS) ms. Growth past the list guard, RSS past the soft
//!   limit, or an Android trim aborts the read through the shared cookie, with the reason stored next to
//!   it (`Shared::reason`), so the engine can tell a guard abort from a user cancel. The engine then
//!   drops the partial list, empties MuPDF's store, trims the heap, sends `DirectMode` and draws that page
//!   directly (no list) until another page is shown.
//! - Between steps (and when woken while idle by a trim) the engine handles a pending trim and checks
//!   the soft limit: list dropped, store emptied, `MemoryReleased` sent.
//! - Trims reach the engine thread as an internal `Msg::Wake` on the request channel, so an idle engine
//!   releases its list at once instead of at its next request.
//!
//! # The shared cookie
//!
//! MuPDF's `fz_cookie` is a plain C struct of ints that one thread (the renderer) writes `progress` into
//! and reads `abort` from, while another thread sets `abort`; MuPDF documents it as unsynchronised by
//! design. The `mupdf` crate's `Cookie` is `!Send`, frees its own allocation on drop and needs `&mut` to
//! abort, so it cannot be shared. [`SharedCookie`] instead owns the memory: an `Arc` of a `#[repr(C)]`
//! struct with exactly `fz_cookie`'s layout (checked at compile time against `mupdf_sys::fz_cookie`) whose
//! fields are atomics. Rust code only touches it through atomic operations; MuPDF gets a raw pointer to
//! it. Because every byte lives inside an `UnsafeCell` (the atomics), MuPDF writing through that pointer
//! while `&` references exist is allowed, and there is no `&mut` anywhere, so no aliasing UB. See
//! [`SharedCookie::mupdf`] for how the pointer is handed to the `mupdf` crate.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicU8, AtomicUsize, Ordering::SeqCst};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::api::{ErrorCode, Event, MemReason, Request, Stage, PROGRESS_EVERY_MS};
use crate::guard::{self, GuardConfig, GUARD_EVERY_MS};
use crate::view::{lowres_width, Image};
use crate::{mem, Bounds, Cookie, DisplayList, Doc, TILE_SIZE};

/// What travels on the engine's request channel: a client request, or a wake-up from
/// [`guard::request_trim`] (the trim itself is read from the guard's sequence number).
enum Msg {
    Req(Request),
    Wake,
}

// ---------------------------------------------------------------------------------------------
// shared cookie

/// `fz_cookie` with atomic fields. Same layout as `mupdf_sys::fz_cookie` (asserted below).
#[repr(C)]
struct CookieMem {
    abort: AtomicI32,
    progress: AtomicI32,
    progress_max: AtomicUsize,
    errors: AtomicI32,
    incomplete: AtomicI32,
}

const _: () = {
    use mupdf_sys::fz_cookie as C;
    use std::mem::{align_of, offset_of, size_of};
    assert!(size_of::<CookieMem>() == size_of::<C>());
    assert!(align_of::<CookieMem>() == align_of::<C>());
    assert!(offset_of!(CookieMem, abort) == offset_of!(C, abort));
    assert!(offset_of!(CookieMem, progress) == offset_of!(C, progress));
    assert!(offset_of!(CookieMem, progress_max) == offset_of!(C, progress_max));
    assert!(offset_of!(CookieMem, errors) == offset_of!(C, errors));
    assert!(offset_of!(CookieMem, incomplete) == offset_of!(C, incomplete));
    // `mupdf::Cookie` is `struct Cookie { inner: *mut fz_cookie }` (mupdf 0.8.0, src/cookie.rs). Same
    // size as the pointer means the pointer is its only non-zero-sized field and sits at offset 0.
    assert!(size_of::<Cookie>() == size_of::<*mut C>());
};

/// A MuPDF cookie that any thread can abort and read progress from. Clone it to share it.
#[derive(Clone)]
pub struct SharedCookie(Arc<CookieMem>);

impl SharedCookie {
    pub fn new() -> SharedCookie {
        let c = SharedCookie(Arc::new(CookieMem {
            abort: AtomicI32::new(0),
            progress: AtomicI32::new(0),
            progress_max: AtomicUsize::new(0),
            errors: AtomicI32::new(0),
            incomplete: AtomicI32::new(0),
        }));
        // Runtime proof that the `mupdf::Cookie` we build points at our memory (no MuPDF call involved:
        // `Cookie::progress` only reads the field).
        c.0.progress.store(0x5A5A, SeqCst);
        assert_eq!(c.mupdf().progress(), 0x5A5A, "mupdf::Cookie layout changed");
        c.0.progress.store(0, SeqCst);
        c
    }

    /// Ask MuPDF to stop the run using this cookie. Any thread; takes effect at MuPDF's next check
    /// (per content-stream operator while reading, per list node while replaying).
    pub fn abort(&self) {
        self.0.abort.store(1, SeqCst);
    }

    pub fn is_aborted(&self) -> bool {
        self.0.abort.load(SeqCst) != 0
    }

    /// MuPDF's progress counter for the current run (operators or list nodes done).
    pub fn progress(&self) -> u64 {
        self.0.progress.load(SeqCst).max(0) as u64
    }

    /// Clear every field before a new run. Only the engine thread calls this, between runs.
    fn reset(&self) {
        self.0.progress.store(0, SeqCst);
        self.0.progress_max.store(0, SeqCst);
        self.0.errors.store(0, SeqCst);
        self.0.incomplete.store(0, SeqCst);
        self.0.abort.store(0, SeqCst);
    }

    /// A `mupdf::Cookie` view of this memory, to pass to `run_with_cookie`.
    ///
    /// The `mupdf` crate has no constructor from a raw pointer, so the view is made by transmuting our
    /// pointer into a `Cookie` (sound because `Cookie`'s only field is that pointer, see the size assert,
    /// and checked at runtime in `new`). It is wrapped in `ManuallyDrop` so `Cookie::drop` never calls
    /// `fz_free` on memory the `Arc` owns, and it only hands out `&Cookie` (never `&mut`), so its
    /// `abort(&mut self)` cannot be reached. The view borrows `self`, so the memory outlives it, and it is
    /// `!Send` like `Cookie`.
    fn mupdf(&self) -> CookieView<'_> {
        let p = Arc::as_ptr(&self.0) as *mut mupdf_sys::fz_cookie;
        // SAFETY: see above; `p` is non-null, aligned and points to a live fz_cookie-shaped allocation.
        let c: Cookie = unsafe { std::mem::transmute::<*mut mupdf_sys::fz_cookie, Cookie>(p) };
        CookieView { c: ManuallyDrop::new(c), _owner: self }
    }
}

impl Default for SharedCookie {
    fn default() -> Self {
        Self::new()
    }
}

struct CookieView<'a> {
    c: ManuallyDrop<Cookie>,
    _owner: &'a SharedCookie,
}

impl Deref for CookieView<'_> {
    type Target = Cookie;
    fn deref(&self) -> &Cookie {
        &self.c
    }
}

// ---------------------------------------------------------------------------------------------
// state shared by the client, the engine thread and the progress thread

/// Counters, for tests and the measurement screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Display lists completely read.
    pub lists_built: u64,
    /// Reads (or list renders) stopped by a cancel.
    pub cancelled_steps: u64,
    /// Tiles rendered.
    pub tiles_rendered: u64,
    /// Tiles answered with `TileSkipped` (superseded or cancelled) without rendering.
    pub tiles_skipped: u64,
    /// Display-list reads abandoned by the memory guard (any [`MemReason`]); each one sent `DirectMode`.
    pub reads_abandoned: u64,
    /// Lists or store contents released by a trim or the soft limit (each one sent `MemoryReleased`).
    pub memory_releases: u64,
    /// Images and tiles drawn directly (direct mode), included in `tiles_rendered` for tiles.
    pub direct_renders: u64,
    /// Bytes in MuPDF's store after the engine's last request (approximate gauge).
    pub store_bytes: u64,
}

#[derive(Default)]
struct Counters {
    lists_built: AtomicU64,
    cancelled_steps: AtomicU64,
    tiles_rendered: AtomicU64,
    tiles_skipped: AtomicU64,
    reads_abandoned: AtomicU64,
    memory_releases: AtomicU64,
    direct_renders: AtomicU64,
    store_bytes: AtomicU64,
}

struct Step {
    page: i32,
    stage: Stage,
    /// When the engine started the ShowPage (Progress `elapsed_ms` counts from here).
    t0: Instant,
    /// Next Progress event; None if this step sends none.
    next: Option<Instant>,
    /// Memory guard for a display-list read; None for renders.
    guard: Option<ReadGuard>,
}

struct ReadGuard {
    /// RSS when the read started (MB).
    rss0: f64,
    /// Next RSS sample.
    next: Instant,
    /// Hard trim requests already handled by the engine when the read started (only a hard trim
    /// aborts a read; task 2bf.2).
    hard_seen: u64,
}

#[derive(Default)]
struct Watch {
    step: Option<Step>,
    stop: bool,
}

struct Shared {
    cookie: SharedCookie,
    /// Cancels issued by clients. Incremented before the abort flag is set and before the `Cancel`
    /// message is queued; the engine counts the `Cancel` messages it has handled. More issued than
    /// handled means a cancel is on its way: everything older must stop.
    cancels_sent: AtomicU64,
    /// Why the progress thread aborted the current read (a [`MemReason`] code; 0 = it did not). Set
    /// before the cookie's abort flag, cleared by `begin_run`, so an abort with no reason is a cancel.
    reason: AtomicU8,
    /// Sharp-view regions with a lower `gen` are stale (DS.1): queued ones are skipped, and the one being
    /// drawn is stopped. Only grows. Raised by every `Region` sent and by `supersede_regions`.
    region_floor: AtomicU64,
    /// `gen` of the region whose draw is running now (0 = none). Read and written under this lock, so
    /// a supersede can only abort the cookie while that draw (never a read) is running.
    region_slot: Mutex<u64>,
    /// Page of the last `ShowPage` sent since the last `Open`/`Close` (DS.2). Another page cancels first.
    shown: Mutex<Option<i32>>,
    limits: GuardConfig,
    counters: Counters,
    watch: Mutex<Watch>,
    wake: Condvar,
}

impl Shared {
    fn watch(&self) -> MutexGuard<'_, Watch> {
        self.watch.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// ---------------------------------------------------------------------------------------------
// client side

/// The sending side of the engine. Cheap to clone, `Send + Sync`.
#[derive(Clone)]
pub struct EngineClient {
    tx: Sender<Msg>,
    shared: Arc<Shared>,
}

impl EngineClient {
    /// Queue a request. `Request::Cancel` is routed to [`cancel`](Self::cancel). False if the engine
    /// thread has stopped.
    pub fn send(&self, req: Request) -> bool {
        match req {
            Request::Cancel => self.cancel(),
            r => {
                match &r {
                    Request::Region { gen, .. } => self.supersede_regions(*gen),
                    Request::Open { .. } | Request::Close => *self.shown() = None,
                    Request::ShowPage { page, .. } => {
                        // Another page: stop the old page's read or render first, or the new page waits
                        // for all of it (a heavy sheet's read is 10-30 s). The same page keeps its work.
                        let mut shown = self.shown();
                        if shown.is_some_and(|p| p != *page) {
                            self.cancel();
                        }
                        *shown = Some(*page);
                    }
                    _ => {}
                }
                self.tx.send(Msg::Req(r)).is_ok()
            }
        }
    }

    /// Stop everything requested so far: aborts the in-flight read or render at once (shared cookie),
    /// skips older queued work, and is answered by one `Event::Cancelled`.
    pub fn cancel(&self) -> bool {
        self.shared.cancels_sent.fetch_add(1, SeqCst);
        self.shared.cookie.abort();
        self.tx.send(Msg::Req(Request::Cancel)).is_ok()
    }

    fn shown(&self) -> MutexGuard<'_, Option<i32>> {
        self.shared.shown.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Make every sharp-view region with `gen < below` stale: queued ones are skipped (TileSkipped) and
    /// the one being drawn stops at MuPDF's next check. A display-list read is never stopped by this, so
    /// the newer region does not have to read the page again. Sending a `Region` does this for its gen.
    pub fn supersede_regions(&self, below: u64) {
        self.shared.region_floor.fetch_max(below, SeqCst);
        let slot = self.shared.region_slot.lock().unwrap_or_else(|e| e.into_inner());
        if *slot != 0 && *slot < below {
            self.shared.cookie.abort();
        }
    }

    pub fn stats(&self) -> Stats {
        let c = &self.shared.counters;
        Stats {
            lists_built: c.lists_built.load(SeqCst),
            cancelled_steps: c.cancelled_steps.load(SeqCst),
            tiles_rendered: c.tiles_rendered.load(SeqCst),
            tiles_skipped: c.tiles_skipped.load(SeqCst),
            reads_abandoned: c.reads_abandoned.load(SeqCst),
            memory_releases: c.memory_releases.load(SeqCst),
            direct_renders: c.direct_renders.load(SeqCst),
            store_bytes: c.store_bytes.load(SeqCst),
        }
    }

    /// The guard limits this engine runs with.
    pub fn guard_config(&self) -> GuardConfig {
        self.shared.limits
    }
}

/// The running engine: a client, the event stream, and the threads. Dropping it (or [`shutdown`]
/// (Self::shutdown)) aborts any in-flight work, stops the engine thread and waits for it.
pub struct Engine {
    client: EngineClient,
    events: Receiver<Event>,
    thread: Option<JoinHandle<()>>,
    /// Registration with [`guard::request_trim`] (removed on stop).
    waker: u64,
}

/// MuPDF anti-aliasing level for every engine render (bug sweep BUG-11: approved, never applied).
pub const ENGINE_AA_LEVEL: i32 = 4;

/// Display-list reads of one page that hard trims may stop before it is drawn directly (BUG-06).
const MAX_TRIM_READS: u32 = 3;

/// Engine thread stack: MuPDF recurses on nested forms and groups; 2 MiB (Rust's default) is tight.
const ENGINE_STACK: usize = 8 << 20;

impl Engine {
    /// Start with the guard limits from the environment ([`GuardConfig::from_env`]: defaults 700 MB list
    /// guard and 1.1 GB soft limit, overridable with `GL_LIST_GUARD_MB` and `GL_SOFT_LIMIT_MB`).
    pub fn start() -> std::io::Result<Engine> {
        Engine::start_with(GuardConfig::from_env())
    }

    /// Start with explicit guard limits.
    pub fn start_with(limits: GuardConfig) -> std::io::Result<Engine> {
        let (req_tx, req_rx) = channel::<Msg>();
        let (ev_tx, ev_rx) = channel::<Event>();
        let shared = Arc::new(Shared {
            cookie: SharedCookie::new(),
            cancels_sent: AtomicU64::new(0),
            reason: AtomicU8::new(0),
            region_floor: AtomicU64::new(0),
            region_slot: Mutex::new(0),
            shown: Mutex::new(None),
            limits,
            counters: Counters::default(),
            watch: Mutex::new(Watch::default()),
            wake: Condvar::new(),
        });
        // Trims seen before this engine started are not replayed.
        let trim_seen = guard::trim_seq();
        let hard_seen = guard::hard_trim_seq();
        let ticker = {
            let (shared, tx) = (shared.clone(), ev_tx.clone());
            thread::Builder::new().name("gl-progress".into()).spawn(move || progress_loop(&shared, &tx))?
        };
        let worker_shared = shared.clone();
        let spawned = thread::Builder::new().name("gl-engine".into()).stack_size(ENGINE_STACK).spawn(move || {
            // The approved anti-aliasing level (BASELINE: faster than MuPDF's default 8; owner's visual
            // check on the phone). Per thread: this is the only thread that draws.
            crate::set_aa_level(ENGINE_AA_LEVEL);
            Worker {
                rx: req_rx,
                tx: ev_tx,
                shared: worker_shared,
                ticker: Some(ticker),
                pending: VecDeque::new(),
                cancels_seen: 0,
                latest_gen: 0,
                woke: false,
                trim_seen,
                hard_seen,
                direct: None,
                doc: None,
                list: None,
            }
            .run()
        });
        let thread = match spawned {
            Ok(t) => t,
            Err(e) => {
                // The closure (and the ticker handle in it) is gone; stop the ticker so it exits.
                shared.watch().stop = true;
                shared.wake.notify_all();
                return Err(e);
            }
        };
        // A trim wakes the progress thread (to abort a read in flight) and the engine thread (to release
        // its list while idle). Weak: the registration must not keep the engine's state alive.
        let waker = {
            let (weak, tx): (Weak<Shared>, Sender<Msg>) = (Arc::downgrade(&shared), req_tx.clone());
            guard::add_waker(Arc::new(move || {
                if let Some(s) = weak.upgrade() {
                    // Take the watch lock so the notify cannot fall between the watcher's check and wait.
                    drop(s.watch());
                    s.wake.notify_all();
                }
                let _ = tx.send(Msg::Wake);
            }))
        };
        Ok(Engine { client: EngineClient { tx: req_tx, shared }, events: ev_rx, thread: Some(thread), waker })
    }

    /// A cloneable sending side (for other threads).
    pub fn client(&self) -> EngineClient {
        self.client.clone()
    }

    pub fn send(&self, req: Request) -> bool {
        self.client.send(req)
    }

    pub fn cancel(&self) -> bool {
        self.client.cancel()
    }

    pub fn stats(&self) -> Stats {
        self.client.stats()
    }

    /// The event stream. It disconnects once the engine thread has exited.
    pub fn events(&self) -> &Receiver<Event> {
        &self.events
    }

    /// Stop the engine thread and wait for it (also done on drop).
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if let Some(t) = self.thread.take() {
            guard::remove_waker(self.waker);
            // Like a cancel that is never acknowledged: the in-flight run aborts, and everything queued
            // before Shutdown is skipped (tiles get TileSkipped), then the loop ends at Shutdown.
            self.client.shared.cancels_sent.fetch_add(1, SeqCst);
            self.client.shared.cookie.abort();
            let _ = self.client.tx.send(Msg::Req(Request::Shutdown));
            let _ = t.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------------------------
// progress thread (never calls MuPDF)

/// The progress thread: Progress ticks for the current step, and the memory guard for a display-list
/// read (RSS every `GUARD_EVERY_MS` ms, trims at once). It never calls MuPDF; it stops a read only through
/// the shared cookie, after storing the reason.
fn progress_loop(shared: &Shared, tx: &Sender<Event>) {
    let every = Duration::from_millis(PROGRESS_EVERY_MS);
    let sample = Duration::from_millis(GUARD_EVERY_MS);
    let mut g = shared.watch();
    loop {
        if g.stop {
            return;
        }
        let Some(s) = g.step.as_mut() else {
            g = shared.wake.wait(g).unwrap_or_else(|e| e.into_inner());
            continue;
        };
        let now = Instant::now();
        if let Some(next) = s.next.filter(|n| now >= *n) {
            // Sent while holding the lock: once the engine has cleared the step (under the same lock),
            // no Progress for it can follow the engine's own events.
            let _ = tx.send(Event::Progress {
                page: s.page,
                stage: s.stage,
                ops: shared.cookie.progress(),
                elapsed_ms: ms_since(s.t0),
            });
            let mut n = next + every;
            if n <= now {
                n = now + every;
            }
            s.next = Some(n);
        }
        if let Some(rg) = s.guard.as_mut() {
            let reason = if guard::hard_trim_seq() > rg.hard_seen {
                Some(MemReason::Trim)
            } else if now >= rg.next {
                rg.next = now + sample;
                over_limit(&shared.limits, rg.rss0, mem::rss_mb())
            } else {
                None
            };
            if let Some(r) = reason {
                // First reason wins; a user cancel racing with it still counts as a cancel (the engine
                // checks the cancel count first).
                let _ = shared.reason.compare_exchange(0, r.code(), SeqCst, SeqCst);
                shared.cookie.abort();
                s.guard = None; // one abort per read; the read ends at MuPDF's next check
            }
        }
        let deadline = s.next.into_iter().chain(s.guard.as_ref().map(|r| r.next)).min();
        g = match deadline {
            None => shared.wake.wait(g).unwrap_or_else(|e| e.into_inner()),
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now());
                shared.wake.wait_timeout(g, left).unwrap_or_else(|e| e.into_inner()).0
            }
        };
    }
}

/// Which limit a read has passed, if any: growth since the read started over the list guard, or RSS
/// over the soft limit. A limit of 0 is off; an unreadable RSS (0.0) never trips.
fn over_limit(l: &GuardConfig, rss0: f64, rss: f64) -> Option<MemReason> {
    if rss <= 0.0 {
        None
    } else if l.list_guard_mb > 0.0 && rss - rss0 > l.list_guard_mb {
        Some(MemReason::ListGuard)
    } else if l.soft_limit_mb > 0.0 && rss > l.soft_limit_mb {
        Some(MemReason::SoftLimit)
    } else {
        None
    }
}

fn ms_since(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

// ---------------------------------------------------------------------------------------------
// engine thread

struct PageList {
    page: i32,
    list: DisplayList,
}

/// Why a step did not produce its result.
enum Stop {
    /// A cancel was issued (the `Cancelled` acknowledgement comes from the `Cancel` message).
    Cancelled,
    /// The memory guard stopped a display-list read (the page then goes to direct mode).
    Memory(MemReason),
    Failed(ErrorCode),
}

struct Worker {
    rx: Receiver<Msg>,
    tx: Sender<Event>,
    shared: Arc<Shared>,
    ticker: Option<JoinHandle<()>>,
    /// Requests already taken off the channel (by `drain`) but not handled yet, in order.
    pending: VecDeque<Request>,
    cancels_seen: u64,
    /// Highest tile `gen` received for the open document.
    latest_gen: u64,
    /// `drain` took a trim wake-up off the queue; `next` acts on it before it waits again (BUG-04).
    woke: bool,
    /// Last trim request (`guard::trim_seq`) this engine has handled.
    trim_seen: u64,
    /// Last hard trim request (`guard::hard_trim_seq`) this engine has handled.
    hard_seen: u64,
    /// The page in direct mode (its list read was abandoned by the guard): drawn without a list until
    /// another page is shown.
    direct: Option<i32>,
    // Field order matters for drop order: the list before the document.
    list: Option<PageList>,
    doc: Option<Doc>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Runs on the engine thread, also on unwind: MuPDF state goes first, then the progress thread
        // stops (it holds an event sender; when both are gone the event channel disconnects).
        self.list = None;
        self.doc = None;
        self.shared.watch().stop = true;
        self.shared.wake.notify_all();
        if let Some(t) = self.ticker.take() {
            let _ = t.join();
        }
    }
}

fn code_of(e: &crate::Error, mupdf: ErrorCode) -> ErrorCode {
    match e {
        crate::Error::PageOutOfRange { .. } => ErrorCode::PageOutOfRange,
        crate::Error::Invalid(_) => ErrorCode::InvalidRequest,
        crate::Error::Mupdf(_) => mupdf,
    }
}

impl Worker {
    fn run(mut self) {
        while let Some(req) = self.next() {
            match req {
                Request::Shutdown => break,
                Request::Open { path } => self.open(&path),
                Request::Close => {
                    self.drop_list();
                    self.doc = None;
                    self.direct = None;
                    self.latest_gen = 0;
                    mem::trim();
                }
                Request::ShowPage { page, fit_width } => self.show_page(page, fit_width),
                Request::Tile { id, page, zoom, x0, y0, gen } => self.tile(id, page, zoom, x0, y0, TILE_SIZE, TILE_SIZE, gen, false),
                Request::Region { id, page, zoom, x0, y0, w, h, gen } => self.tile(id, page, zoom, x0, y0, w, h, gen, true),
                Request::Cancel => {
                    self.cancels_seen += 1;
                    self.emit(Event::Cancelled);
                }
                Request::DropList => {
                    let had_list = self.drop_list();
                    self.emit(Event::Dropped { had_list });
                }
            }
            self.note_store();
        }
    }

    /// The next request to handle. Wake-ups from a trim are handled here while idle (list dropped, store
    /// emptied) and never returned.
    fn next(&mut self) -> Option<Request> {
        loop {
            if let Some(r) = self.pending.pop_front() {
                return Some(r);
            }
            if std::mem::take(&mut self.woke) {
                // A wake-up drained during the last request (which may have ended early, before its own
                // memory check): handle the trim now, not at the next request.
                self.memory_check(None);
                self.note_store();
            }
            match self.rx.recv().ok()? {
                Msg::Req(r) => return Some(r),
                Msg::Wake => {
                    self.memory_check(None);
                    self.note_store();
                }
            }
        }
    }

    /// Move everything waiting in the channel into `pending` (so stale work can be seen and skipped).
    /// A wake-up is remembered in `woke`, so `next` handles its trim even if this request ends early.
    fn drain(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            let Msg::Req(r) = m else {
                self.woke = true;
                continue;
            };
            if let Request::Tile { gen, .. } | Request::Region { gen, .. } = r {
                self.latest_gen = self.latest_gen.max(gen);
            }
            self.pending.push_back(r);
        }
    }

    fn note_store(&self) {
        self.shared.counters.store_bytes.store(guard::store_size_bytes() as u64, SeqCst);
    }

    fn emit(&self, e: Event) {
        let _ = self.tx.send(e);
    }

    fn error(&self, code: ErrorCode, tile: Option<u64>) {
        self.emit(Event::Error { code, tile });
    }

    /// A newer sharp-view region has been sent (or the viewer dropped this one).
    fn region_stale(&self, gen: u64) -> bool {
        gen < self.shared.region_floor.load(SeqCst)
    }

    fn region_slot(&self) -> MutexGuard<'_, u64> {
        self.shared.region_slot.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A cancel has been issued that this thread has not acknowledged yet.
    fn cancel_pending(&self) -> bool {
        self.shared.cancels_sent.load(SeqCst) > self.cancels_seen
    }

    /// Reset the cookie for a new MuPDF run. False if a cancel is pending (the run must not start).
    ///
    /// Order matters: reset (abort = 0) first, then look at the cancel count. A client increments the
    /// count before setting abort = 1, so (all SeqCst) either this load sees the increment, or the
    /// client's abort store comes after our reset and the run aborts at once.
    fn begin_run(&self) -> bool {
        self.shared.reason.store(0, SeqCst);
        self.shared.cookie.reset();
        !self.cancel_pending()
    }

    /// After a MuPDF run: was it stopped, and why? A cancel issued meanwhile always counts as a cancel;
    /// an abort with a guard reason is that reason; any other abort is a cancel.
    fn run_stopped(&self) -> Option<Stop> {
        if self.cancel_pending() {
            return Some(Stop::Cancelled);
        }
        if !self.shared.cookie.is_aborted() {
            return None;
        }
        Some(match MemReason::from_code(self.shared.reason.load(SeqCst)) {
            Some(r) => Stop::Memory(r),
            None => Stop::Cancelled,
        })
    }

    /// Start Progress ticks for a render step.
    fn watch_start(&self, page: i32, stage: Stage, t0: Instant) {
        self.set_step(Step { page, stage, t0, next: Some(Self::first_tick()), guard: None });
    }

    /// Start the memory guard (and, with `ticks`, Progress ticks) for a display-list read.
    fn watch_read(&self, page: i32, t0: Instant, ticks: bool) {
        let rg = ReadGuard {
            rss0: mem::rss_mb(),
            next: Instant::now() + Duration::from_millis(GUARD_EVERY_MS),
            hard_seen: self.hard_seen,
        };
        self.set_step(Step { page, stage: Stage::Read, t0, next: ticks.then(Self::first_tick), guard: Some(rg) });
    }

    fn first_tick() -> Instant {
        Instant::now() + Duration::from_millis(PROGRESS_EVERY_MS)
    }

    fn set_step(&self, step: Step) {
        let mut g = self.shared.watch();
        g.step = Some(step);
        drop(g);
        self.shared.wake.notify_all();
    }

    /// Stop the progress ticks for the current step. Once this returns, the progress thread sends
    /// nothing more for it.
    fn watch_stop(&self) {
        self.shared.watch().step = None;
    }

    fn drop_list(&mut self) -> bool {
        let had = self.list.take().is_some();
        if had {
            mem::trim();
        }
        had
    }

    /// Drop the list, empty MuPDF's store and give the memory back. True if there was a list.
    fn release(&mut self) -> bool {
        let had = self.list.take().is_some();
        guard::empty_store();
        mem::trim();
        had
    }

    /// Between steps, and when woken by a trim while idle: handle a pending trim, then the soft limit.
    /// Both drop the list and empty the store. Above the soft limit the dropped list's page also goes to
    /// direct mode (reading it again would pass the limit again), but only if it is the page about to be
    /// drawn (`target`; None = idle, the list's page is the one on screen). A page being left only loses
    /// its list (BUG-05).
    fn memory_check(&mut self, target: Option<i32>) {
        let seq = guard::trim_seq();
        if seq > self.trim_seen {
            self.trim_seen = seq;
            let hard = guard::hard_trim_seq();
            if hard > self.hard_seen {
                self.hard_seen = hard;
                self.release_for(MemReason::Trim);
            } else {
                // Light trim (onStop, UI_HIDDEN: the SAF picker fires these): the store only; the
                // current page's list stays.
                self.empty_store_for(MemReason::Trim);
            }
        }
        let soft = self.shared.limits.soft_limit_mb;
        if soft > 0.0 && mem::rss_mb() > soft {
            let page = self.list.as_ref().map(|l| l.page);
            if self.release_for(MemReason::SoftLimit) {
                if let Some(p) = page.filter(|p| target.map_or(true, |t| t == *p)) {
                    self.go_direct(p, MemReason::SoftLimit);
                }
            }
        }
    }

    /// Release for `reason`; sends `MemoryReleased` if anything was held. True if a list was dropped.
    fn release_for(&mut self, reason: MemReason) -> bool {
        let store_before = guard::store_size_bytes();
        let had_list = self.release();
        if had_list || store_before > 0 {
            self.shared.counters.memory_releases.fetch_add(1, SeqCst);
            self.emit(Event::MemoryReleased { reason, had_list, rss_mb: mem::rss_mb() });
        }
        had_list
    }

    /// Empty MuPDF's store (the list stays); sends `MemoryReleased` if the store held anything.
    fn empty_store_for(&mut self, reason: MemReason) {
        if guard::store_size_bytes() > 0 {
            guard::empty_store();
            mem::trim();
            self.shared.counters.memory_releases.fetch_add(1, SeqCst);
            self.emit(Event::MemoryReleased { reason, had_list: false, rss_mb: mem::rss_mb() });
        }
    }

    /// Mark every trim requested so far as handled.
    fn trims_handled(&mut self) {
        self.trim_seen = self.trim_seen.max(guard::trim_seq());
        self.hard_seen = self.hard_seen.max(guard::hard_trim_seq());
    }

    /// Put `page` in direct mode after the guard abandoned (or dropped) its list: store emptied, heap
    /// trimmed, `DirectMode` sent.
    fn go_direct(&mut self, page: i32, reason: MemReason) {
        self.release();
        if reason == MemReason::Trim {
            // The trim that stopped the read has been acted on (everything is released).
            self.trims_handled();
        }
        self.direct = Some(page);
        self.emit(Event::DirectMode { page, reason, rss_mb: mem::rss_mb() });
    }

    fn open(&mut self, path: &str) {
        self.drop_list();
        self.doc = None;
        self.direct = None;
        self.latest_gen = 0;
        mem::trim();
        match Doc::open(path) {
            Ok(d) => {
                let (pages, open_ms) = (d.page_count(), d.open_ms());
                self.doc = Some(d);
                self.emit(Event::Opened { pages, open_ms });
            }
            Err(_) => self.error(ErrorCode::OpenFailed, None),
        }
    }

    fn check_page(&self, page: i32) -> Result<(), ErrorCode> {
        let d = self.doc.as_ref().ok_or(ErrorCode::NoDocument)?;
        if page < 0 || page >= d.page_count() {
            return Err(ErrorCode::PageOutOfRange);
        }
        Ok(())
    }

    /// Make sure the display list holds `page`, reading it (with progress ticks) if not. Returns the read
    /// time in ms (0 if the list was already there).
    fn ensure_list(&mut self, page: i32, t0: Instant, ticks: bool) -> Result<f64, Stop> {
        if self.list.as_ref().is_some_and(|l| l.page == page) {
            return Ok(0.0);
        }
        self.drop_list(); // one list at a time: the old page goes before the new one is read
        if !self.begin_run() {
            return Err(Stop::Cancelled);
        }
        let doc = self.doc.as_ref().ok_or(Stop::Failed(ErrorCode::NoDocument))?;
        // Always watched: the memory guard runs for every read, Progress ticks only with `ticks`.
        self.watch_read(page, t0, ticks);
        let res = {
            let cookie = self.shared.cookie.mupdf();
            doc.build_display_list_with_cookie(page, &cookie)
        };
        self.watch_stop();
        let stopped = self.run_stopped();
        if let Some(Stop::Memory(r)) = stopped {
            // Abandoned by the guard: the partial list (if any) goes now; the caller draws directly.
            drop(res);
            self.shared.counters.reads_abandoned.fetch_add(1, SeqCst);
            return Err(Stop::Memory(r));
        }
        let stopped = stopped.is_some();
        match res {
            Ok(built) if !stopped => {
                self.shared.counters.lists_built.fetch_add(1, SeqCst);
                if ticks {
                    self.emit(Event::Progress {
                        page,
                        stage: Stage::Read,
                        ops: self.shared.cookie.progress(),
                        elapsed_ms: ms_since(t0),
                    });
                }
                let ms = built.timings.load_ms + built.timings.list_ms;
                self.list = Some(PageList { page, list: built.list });
                Ok(ms)
            }
            Ok(partial) => {
                drop(partial);
                mem::trim();
                self.shared.counters.cancelled_steps.fetch_add(1, SeqCst);
                Err(Stop::Cancelled)
            }
            Err(_) if stopped => {
                mem::trim();
                self.shared.counters.cancelled_steps.fetch_add(1, SeqCst);
                Err(Stop::Cancelled)
            }
            Err(e) => Err(Stop::Failed(code_of(&e, ErrorCode::ReadFailed))),
        }
    }

    /// Make `page` renderable: the list holds it, or it is in direct mode. A read abandoned by the guard
    /// puts the page in direct mode (and returns the time the abandoned read took).
    fn prepare(&mut self, page: i32, t0: Instant, ticks: bool) -> Result<f64, Stop> {
        if self.direct == Some(page) {
            return Ok(0.0);
        }
        self.direct = None; // another page: direct mode ends, the new page gets a list again
        let t = Instant::now();
        let mut reads = 1;
        loop {
            match self.ensure_list(page, t0, ticks) {
                // A hard trim stopped the read (task 2bf.2): release everything and read again with a
                // list (BUG-06: a second trim gets another read too). The list guard and the soft limit
                // still watch every read. Only after MAX_TRIM_READS trimmed reads is the page drawn
                // directly, so it still shows on a phone that keeps trimming.
                Err(Stop::Memory(MemReason::Trim)) if reads < MAX_TRIM_READS => {
                    self.trims_handled();
                    self.release_for(MemReason::Trim);
                    reads += 1;
                }
                Err(Stop::Memory(reason)) => {
                    self.go_direct(page, reason);
                    return Ok(ms_since(t));
                }
                other => return other,
            }
        }
    }

    /// Render the fit image at `width` from the current list (or directly, in direct mode), with progress
    /// ticks for `stage`. Also returns the page bounds (points) the image was drawn from.
    fn render_fit(&self, page: i32, stage: Stage, width: u32, t0: Instant) -> Result<(Image, Bounds), Stop> {
        let direct = self.direct == Some(page);
        let doc = self.doc.as_ref().ok_or(Stop::Failed(ErrorCode::NoDocument))?;
        let list = match (direct, self.list.as_ref()) {
            (true, _) => None,
            (false, Some(pl)) if pl.page == page => Some(&pl.list),
            (false, _) => return Err(Stop::Failed(ErrorCode::RenderFailed)),
        };
        if !self.begin_run() {
            return Err(Stop::Cancelled);
        }
        self.watch_start(page, stage, t0);
        let res = {
            let cookie = self.shared.cookie.mupdf();
            match list {
                Some(l) => l.render_fit_cookie(width, Some(&cookie)).map(|r| (r, l.bounds())),
                None => guard::direct_fit_bounds(doc, page, width, &cookie),
            }
        };
        self.watch_stop();
        if self.run_stopped().is_some() {
            self.shared.counters.cancelled_steps.fetch_add(1, SeqCst);
            return Err(Stop::Cancelled);
        }
        if direct && res.is_ok() {
            self.shared.counters.direct_renders.fetch_add(1, SeqCst);
        }
        res.map(|(r, b)| (Image::from(r), b)).map_err(|e| Stop::Failed(code_of(&e, ErrorCode::RenderFailed)))
    }

    /// True if work queued after the current ShowPage makes it pointless: a cancel, or a later ShowPage.
    fn show_superseded(&mut self) -> bool {
        self.drain();
        self.cancel_pending() || self.pending.iter().any(|r| matches!(r, Request::ShowPage { .. }))
    }

    fn show_page(&mut self, page: i32, fit_width: u32) {
        let t0 = Instant::now();
        if self.show_superseded() {
            return;
        }
        if let Err(code) = self.check_page(page) {
            return self.error(code, None);
        }
        if fit_width == 0 {
            return self.error(ErrorCode::InvalidRequest, None);
        }
        self.memory_check(Some(page));
        let list_ms = match self.prepare(page, t0, true) {
            Ok(ms) => ms,
            Err(Stop::Cancelled | Stop::Memory(_)) => return,
            Err(Stop::Failed(code)) => return self.error(code, None),
        };
        match self.render_fit(page, Stage::LowRes, lowres_width(fit_width), t0) {
            Ok((i, b)) => {
                let total_ms = ms_since(t0);
                let (pt_w, pt_h) = (b.width(), b.height());
                self.emit(Event::LowRes { page, rgba: i.rgba, w: i.w, h: i.h, ms: i.ms, list_ms, total_ms, pt_w, pt_h });
            }
            Err(Stop::Cancelled | Stop::Memory(_)) => return,
            Err(Stop::Failed(code)) => return self.error(code, None),
        }
        if self.show_superseded() {
            return;
        }
        self.memory_check(Some(page));
        if self.direct != Some(page) && self.list.is_none() {
            // A trim between the two images dropped the list: read it again for the Fit image.
            match self.prepare(page, t0, true) {
                Ok(_) => {}
                Err(Stop::Cancelled | Stop::Memory(_)) => return,
                Err(Stop::Failed(code)) => return self.error(code, None),
            }
        }
        match self.render_fit(page, Stage::Fit, fit_width, t0) {
            Ok((i, b)) => {
                let total_ms = ms_since(t0);
                let (pt_w, pt_h) = (b.width(), b.height());
                self.emit(Event::Fit { page, rgba: i.rgba, w: i.w, h: i.h, ms: i.ms, list_ms, total_ms, pt_w, pt_h });
            }
            Err(Stop::Cancelled | Stop::Memory(_)) => {}
            Err(Stop::Failed(code)) => self.error(code, None),
        }
    }

    fn skip_tile(&self, id: u64, gen: u64) {
        self.shared.counters.tiles_skipped.fetch_add(1, SeqCst);
        self.emit(Event::TileSkipped { id, gen });
    }

    #[allow(clippy::too_many_arguments)]
    fn tile(&mut self, id: u64, page: i32, zoom: f32, x0: i32, y0: i32, w: u32, h: u32, gen: u64, region: bool) {
        // See everything queued first: a newer generation anywhere in the queue makes this one stale.
        self.latest_gen = self.latest_gen.max(gen);
        self.drain();
        if gen < self.latest_gen || (region && self.region_stale(gen)) || self.cancel_pending() {
            return self.skip_tile(id, gen);
        }
        if let Err(code) = self.check_page(page) {
            return self.error(code, Some(id));
        }
        self.memory_check(Some(page));
        match self.prepare(page, Instant::now(), true) {
            Ok(_) => {}
            Err(Stop::Cancelled | Stop::Memory(_)) => return self.skip_tile(id, gen),
            Err(Stop::Failed(code)) => return self.error(code, Some(id)),
        }
        let direct = self.direct == Some(page);
        let Some(doc) = self.doc.as_ref() else { return self.error(ErrorCode::NoDocument, Some(id)) };
        let list = match (direct, self.list.as_ref()) {
            (true, _) => None,
            (false, Some(pl)) if pl.page == page => Some(&pl.list),
            (false, _) => return self.error(ErrorCode::RenderFailed, Some(id)),
        };
        if !self.begin_run() {
            return self.skip_tile(id, gen);
        }
        if region {
            // From here a newer region may stop this draw. Superseded while the page was being read?
            *self.region_slot() = gen;
            if self.region_stale(gen) {
                *self.region_slot() = 0;
                return self.skip_tile(id, gen);
            }
        }
        let res = {
            let cookie = self.shared.cookie.mupdf();
            match list {
                Some(l) => l.render_region_cookie(zoom, x0, y0, w, h, Some(&cookie)),
                None => guard::direct_region(doc, page, zoom, x0, y0, w, h, &cookie),
            }
        };
        if region {
            *self.region_slot() = 0;
        }
        if self.run_stopped().is_some() {
            return self.skip_tile(id, gen);
        }
        match res {
            Ok(r) => {
                self.shared.counters.tiles_rendered.fetch_add(1, SeqCst);
                if direct {
                    self.shared.counters.direct_renders.fetch_add(1, SeqCst);
                }
                let i = Image::from(r);
                self.emit(Event::Tile { id, gen, page, rgba: i.rgba, w: i.w, h: i.h, render_ms: i.ms });
            }
            Err(e) => self.error(code_of(&e, ErrorCode::RenderFailed), Some(id)),
        }
    }
}
