//! The app's handle on the engine worker (task 2b.6): the worker thread is the single owner of all
//! MuPDF state; this module holds only plain data, the client, and the fds the worker reads through.
//!
//! - [`start`] starts the engine (`Engine::start`) and a dispatcher thread (`gl-dispatch`) that owns
//!   the `Engine` and drains its unbounded event channel continuously. Tile answers go to the waiting
//!   `gl` protocol responders ([`TileRouter`]); everything else goes to the UI as Tauri events:
//!   `gl-progress`, `gl-image` (an id the page fetches over `gl://.../img`, never pixels over IPC, plus
//!   the page size in points), `gl-cancelled`, `gl-error`, `gl-dropped`, and the memory guard's
//!   `gl-direct` (a page went to direct mode) and `gl-memory` (list and store released). Every payload
//!   carries `doc`, the document serial, so the page can drop events of a document it has already left.
//! - Open: the worker opens `/proc/self/fd/N` (the engine's `Request::Open` takes a path), so the
//!   `OwnedFd` stays here until the worker is done with it: replaced on the next `Opened` / open
//!   failure (the worker drops the old document before opening), or, on Close, released by a fence: a
//!   tile request queued after `Close`, which the worker always answers, and only after the Close.
//!
//! Confidentiality: nothing here logs. The fd path is a number; events carry numbers and fixed codes.

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};

use greenline_engine::api::{ErrorCode, Event, MemReason, Request, Stage};
// Fixed names of the guard's reasons ("list_guard", "soft_limit", "trim"), the same as the results file.
use greenline_engine::results::reason_name;
use greenline_engine::worker::{Engine, EngineClient, Stats};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::protocol::{Backend, StoredImage, TileDone, TileParams, TileRouter};

/// Low-res / fit images kept for the page to fetch. Each is taken on fetch; this only bounds what an
/// unfetched backlog can hold (a 1600 px fit image of a tall page is ~15 MB).
const MAX_IMAGES: usize = 3;
/// Largest fit width the UI may ask for.
pub const MAX_FIT_WIDTH: u32 = 4096;

/// Result of an open, sent from the dispatcher to the waiting `open_picked`.
pub type OpenOutcome = Result<Opened, ErrorCode>;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Opened {
    pub pages: i32,
    pub open_ms: f64,
    /// Serial of this document; every later event for it carries the same `doc`.
    pub doc: u64,
}

#[derive(Default)]
struct DocSlot {
    /// The fd the worker's current document reads through.
    current: Option<OwnedFd>,
    /// The fd of an `Open` the worker has not answered yet.
    opening: Option<OwnedFd>,
    open_wait: Option<mpsc::Sender<OpenOutcome>>,
}

#[derive(Default)]
struct Images {
    next_id: u64,
    items: VecDeque<(u64, StoredImage)>,
}

pub struct Host {
    client: EngineClient,
    tiles: TileRouter,
    images: Mutex<Images>,
    doc: Mutex<DocSlot>,
    /// Document serial, bumped only by the dispatcher on `Opened` or an open failure, so events are
    /// tagged in exactly the order the worker produced them. (After `close_doc` the page drops all
    /// events until its next open, so Close needs no bump.)
    doc_seq: AtomicU64,
    /// The page the worker last put in direct mode (for the measurement screen).
    direct: Mutex<Option<DirectState>>,
}

#[derive(Clone, Copy)]
struct DirectState {
    doc: u64,
    page: i32,
    reason: MemReason,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Backend for Host {
    fn tile(&self, t: TileParams, done: TileDone) {
        self.tiles.queue(&self.client, t, done);
    }

    fn take_image(&self, id: u64) -> Option<StoredImage> {
        let mut g = lock(&self.images);
        let i = g.items.iter().position(|(k, _)| *k == id)?;
        g.items.remove(i).map(|(_, img)| img)
    }
}

impl Host {
    /// Queue opening `fd` (already probed seekable by the caller). Cancels whatever the worker is
    /// doing first, so a heavy page of the old document does not delay the open. Returns the receiver
    /// the dispatcher answers on; `Err` if an open is already in flight or the engine is gone.
    pub fn open(&self, fd: OwnedFd) -> Result<mpsc::Receiver<OpenOutcome>, &'static str> {
        let mut slot = lock(&self.doc);
        if slot.open_wait.is_some() {
            return Err("E_BUSY");
        }
        let path = format!("/proc/self/fd/{}", fd.as_raw_fd());
        let (tx, rx) = mpsc::channel();
        slot.opening = Some(fd);
        slot.open_wait = Some(tx);
        self.client.cancel();
        if !self.client.send(Request::Open { path }) {
            slot.opening = None;
            slot.open_wait = None;
            return Err("E_ENGINE");
        }
        Ok(rx)
    }

    /// Cancel, close the document, and release its fd once the worker has processed the Close.
    /// False if no document was open.
    pub fn close(&self) -> bool {
        let fd = {
            let mut slot = lock(&self.doc);
            match slot.current.take() {
                Some(fd) => fd,
                None => return false,
            }
        };
        self.client.cancel();
        self.client.send(Request::Close);
        // Fence: any tile is answered exactly once (here: NoDocument or skipped), and only after the
        // Close before it has run, so the fd is closed only once MuPDF no longer reads it.
        let fence = TileParams { page: 0, zoom: 1.0, x0: 0, y0: 0, gen: 0, w: 1024, h: 1024 };
        self.tiles.queue(&self.client, fence, Box::new(move |_| drop(fd)));
        true
    }

    pub fn show_page(&self, page: i32, fit_width: u32) -> bool {
        // The worker ends direct mode when another page is shown (it sends no event for that).
        {
            let mut d = lock(&self.direct);
            let other_page = matches!(*d, Some(s) if s.page != page);
            if other_page {
                *d = None;
            }
        }
        self.client.send(Request::ShowPage { page, fit_width: fit_width.clamp(1, MAX_FIT_WIDTH) })
    }

    pub fn cancel(&self) -> bool {
        self.client.cancel()
    }

    fn doc(&self) -> u64 {
        self.doc_seq.load(Relaxed)
    }

    fn store_image(&self, img: StoredImage) -> u64 {
        let mut g = lock(&self.images);
        g.next_id += 1;
        let id = g.next_id;
        if g.items.len() >= MAX_IMAGES {
            g.items.pop_front();
        }
        g.items.push_back((id, img));
        id
    }

    /// Answer the waiting open (if any) and move the fds along. The worker has dropped the previous
    /// document by now (it does so before every open), so the previous fd can close.
    fn finish_open(&self, outcome: Result<(i32, f64), ErrorCode>) -> bool {
        let (wait, released) = {
            let mut slot = lock(&self.doc);
            let Some(wait) = slot.open_wait.take() else { return false };
            let opened = slot.opening.take();
            let released = match outcome {
                // The new fd becomes current; the previous document's fd is released.
                Ok(_) => std::mem::replace(&mut slot.current, opened),
                // Failed: the worker has no document now; release both fds.
                Err(_) => {
                    drop(slot.current.take());
                    opened
                }
            };
            (wait, released)
        };
        drop(released);
        lock(&self.images).items.clear();
        *lock(&self.direct) = None;
        let doc = self.doc_seq.fetch_add(1, Relaxed) + 1;
        let _ = wait.send(outcome.map(|(pages, open_ms)| Opened { pages, open_ms, doc }));
        true
    }

    /// Route one engine event (dispatcher thread).
    fn dispatch(&self, app: &AppHandle, ev: Event) {
        let Some(ev) = self.tiles.route(ev) else { return };
        let doc = self.doc();
        match ev {
            Event::Opened { pages, open_ms } => {
                self.finish_open(Ok((pages, open_ms)));
            }
            Event::Error { code: ErrorCode::OpenFailed, tile: None } if self.finish_open(Err(ErrorCode::OpenFailed)) => {}
            Event::Error { code, .. } => emit(app, "gl-error", ErrorMsg { doc, code: code.code() }),
            Event::Progress { page, stage, ops, elapsed_ms } => {
                emit(app, "gl-progress", ProgressMsg { doc, page, stage: stage_name(stage), ops, elapsed_ms })
            }
            Event::LowRes { page, rgba, w, h, ms, list_ms, total_ms, pt_w, pt_h } => {
                let id = self.store_image(StoredImage { rgba, w, h, ms });
                emit(app, "gl-image", ImageMsg { doc, page, kind: "lowres", id, w, h, ms, list_ms, total_ms, pt_w, pt_h })
            }
            Event::Fit { page, rgba, w, h, ms, list_ms, total_ms, pt_w, pt_h } => {
                let id = self.store_image(StoredImage { rgba, w, h, ms });
                emit(app, "gl-image", ImageMsg { doc, page, kind: "fit", id, w, h, ms, list_ms, total_ms, pt_w, pt_h })
            }
            Event::Cancelled => emit(app, "gl-cancelled", DocMsg { doc }),
            Event::Dropped { had_list } => emit(app, "gl-dropped", DroppedMsg { doc, had_list }),
            Event::DirectMode { page, reason, rss_mb } => {
                *lock(&self.direct) = Some(DirectState { doc, page, reason });
                emit(app, "gl-direct", DirectMsg { doc, page, reason: reason_name(reason), rss_mb })
            }
            Event::MemoryReleased { reason, had_list, rss_mb } => {
                emit(app, "gl-memory", MemoryMsg { doc, reason: reason_name(reason), had_list, rss_mb })
            }
            // Tile answers were consumed by the router; anything added later is ignored here.
            _ => {}
        }
    }

    /// Whether `page` of the current document is in direct mode (the last `DirectMode` event was for it
    /// and no other page has been shown since), and why.
    pub fn direct_mode(&self, page: i32) -> Option<MemReason> {
        let doc = self.doc();
        let state: Option<DirectState> = *lock(&self.direct);
        state.filter(|d| d.doc == doc && d.page == page).map(|d| d.reason)
    }

    /// The engine's counters (direct renders, memory releases, ...).
    pub fn stats(&self) -> Stats {
        self.client.stats()
    }

    /// The engine thread has gone: fail every waiter.
    fn engine_gone(&self) {
        self.tiles.fail_all();
        let mut slot = lock(&self.doc);
        if let Some(w) = slot.open_wait.take() {
            let _ = w.send(Err(ErrorCode::OpenFailed));
        }
        slot.opening = None;
    }
}

fn emit<S: Serialize + Clone>(app: &AppHandle, name: &str, payload: S) {
    let _ = app.emit(name, payload);
}

fn stage_name(s: Stage) -> &'static str {
    match s {
        Stage::Read => "read",
        Stage::LowRes => "lowres",
        Stage::Fit => "fit",
    }
}

#[derive(Clone, Serialize)]
struct DocMsg {
    doc: u64,
}

#[derive(Clone, Serialize)]
struct ErrorMsg {
    doc: u64,
    code: u16,
}

#[derive(Clone, Serialize)]
struct DroppedMsg {
    doc: u64,
    had_list: bool,
}

#[derive(Clone, Serialize)]
struct ProgressMsg {
    doc: u64,
    page: i32,
    stage: &'static str,
    ops: u64,
    elapsed_ms: f64,
}

#[derive(Clone, Serialize)]
struct ImageMsg {
    doc: u64,
    page: i32,
    kind: &'static str,
    /// Fetch as `gl://.../img?id=<id>` (once).
    id: u64,
    w: u32,
    h: u32,
    ms: f64,
    list_ms: f64,
    total_ms: f64,
    /// Page size in points (the engine's page bounds): the viewer's 400 % tile grid is built on it.
    pt_w: f32,
    pt_h: f32,
}

/// `gl-direct`: the memory guard put `page` in direct mode (drawn without a display list until another
/// page is shown). `reason` is "list_guard", "soft_limit" or "trim"; `rss_mb` the main-process RSS after.
#[derive(Clone, Serialize)]
struct DirectMsg {
    doc: u64,
    page: i32,
    reason: &'static str,
    rss_mb: f64,
}

/// `gl-memory`: the engine released its display list and MuPDF's store on its own (trim or soft limit).
#[derive(Clone, Serialize)]
struct MemoryMsg {
    doc: u64,
    reason: &'static str,
    had_list: bool,
    rss_mb: f64,
}

/// Start the engine and the dispatcher. The returned host goes into Tauri state.
pub fn start(app: AppHandle) -> std::io::Result<Arc<Host>> {
    // Memory limits by the phone's RAM (task 2c.2b): 1,500 / 2,300 MB on the 16 GB test phone.
    let engine = Engine::start_with(greenline_engine::guard::GuardConfig::for_device())?;
    let host = Arc::new(Host {
        client: engine.client(),
        tiles: TileRouter::default(),
        images: Mutex::new(Images::default()),
        doc: Mutex::new(DocSlot::default()),
        doc_seq: AtomicU64::new(0),
        direct: Mutex::new(None),
    });
    let h = host.clone();
    std::thread::Builder::new().name("gl-dispatch".into()).spawn(move || {
        // Owns the engine: the receiver is drained here for the life of the app.
        for ev in engine.events().iter() {
            h.dispatch(&app, ev);
        }
        h.engine_gone();
    })?;
    Ok(host)
}
