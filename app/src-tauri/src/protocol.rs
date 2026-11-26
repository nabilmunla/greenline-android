//! The `gl` custom protocol (task 2b.6): raw RGBA tiles and fit images from the engine to the WebView.
//!
//! URLs (`http://gl.localhost/...` on Android and Windows, `gl://localhost/...` elsewhere):
//! - `GET /tile?page=P&zoom=Z&x0=X&y0=Y&gen=G`: one `TILE_SIZE` x `TILE_SIZE` tile, raw RGBA (4 MiB).
//!   200 with the pixels; 204 when the engine skipped it (superseded `gen`, or a cancel); 422 with
//!   `X-Gl-Error: <code>` when the engine refused it; 400 on a malformed query; 503 without an engine.
//! - `GET /img?id=N`: a low-res or fit image the dispatcher stored for the UI (announced by a
//!   `gl-image` event carrying `id`). Each image can be fetched once; 404 after that or when evicted.
//!
//! Every response carries `Access-Control-Allow-Origin` for the app's own page only (the page is `http://tauri.localhost`, a
//! different origin) and exposes `X-Gl-Render-Ms`, `X-Gl-W`, `X-Gl-H`, plus `X-Gl-Gen`, `X-Gl-Wait-Ms`
//! (handler -> engine answer, so the JS side can split engine time from handoff time) and `X-Gl-Error`.
//!
//! Structure: [`handle`] is the whole request -> [`Reply`] logic over the [`Backend`] trait, with no
//! Tauri in it, so it is host-testable (`engine/tests/app_protocol.rs` compiles this file with
//! `cfg(test)` and runs it against the real engine worker). [`TileRouter`] matches engine tile events
//! to waiting responders by id. Only [`register`] (left out under `cfg(test)`) touches Tauri.
//!
//! The handler never blocks: it parses, queues a `Request::Tile` and returns; the dispatcher thread
//! answers the responder when the engine's event arrives.
//!
//! Confidentiality: nothing here logs, and no response carries a path, a name or page text.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Instant;

use greenline_engine::api::{Event, Request};
use greenline_engine::worker::EngineClient;

/// Largest zoom a tile request may ask for (MuPDF pixel coordinates are `int`; 64 x an A0 width stays
/// far below that).
pub const MAX_ZOOM: f32 = 64.0;

/// Headers the page may read on a cross-origin response.
pub const EXPOSE_HEADERS: &str = "X-Gl-Render-Ms, X-Gl-W, X-Gl-H, X-Gl-Gen, X-Gl-Wait-Ms, X-Gl-Error";

/// The app page's origin on Android (Tauri 2). Replies are readable by that page only (BUG-20).
const APP_ORIGIN: &str = "http://tauri.localhost";

/// Largest region origin accepted, in device pixels (BUG-08). A 200-inch page at the 1600 % maximum is
/// about 230,000 px wide; anything near i32::MAX is a bad request.
const MAX_ORIGIN: u32 = 1 << 24;

/// A response: status, headers, body. Converted to an `http::Response` only at the Tauri edge.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    fn new(status: u16) -> Reply {
        Reply {
            status,
            headers: vec![
                ("Access-Control-Allow-Origin", APP_ORIGIN.into()),
                ("Access-Control-Expose-Headers", EXPOSE_HEADERS.into()),
                ("Cache-Control", "no-store".into()),
            ],
            body: Vec::new(),
        }
    }

    fn header(mut self, name: &'static str, value: impl ToString) -> Reply {
        self.headers.push((name, value.to_string()));
        self
    }

    fn pixels(self, rgba: Vec<u8>, w: u32, h: u32, ms: f64) -> Reply {
        let mut r = self
            .header("Content-Type", "application/octet-stream")
            .header("X-Gl-W", w)
            .header("X-Gl-H", h)
            .header("X-Gl-Render-Ms", format!("{ms:.1}"));
        r.body = rgba;
        r
    }

    /// First value of header `name` (case-insensitive), for tests and callers.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// A parsed `/tile` query.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TileParams {
    pub page: i32,
    pub zoom: f32,
    pub x0: i32,
    pub y0: i32,
    pub gen: u64,
    /// Region size (task 2c.2: optional `w`/`h`, each 1..=4096; default the 1024 tile).
    pub w: u32,
    pub h: u32,
}

/// Default and largest region side.
const TILE_SIDE: u32 = 1024;
const MAX_SIDE: u32 = 4096;

/// What the engine answered for one tile.
#[derive(Debug)]
pub enum TileAnswer {
    Tile { gen: u64, rgba: Vec<u8>, w: u32, h: u32, render_ms: f64 },
    Skipped { gen: u64 },
    Error { code: u16 },
    /// The engine thread is gone (or never started).
    EngineGone,
}

/// A stored low-res / fit image.
#[derive(Debug)]
pub struct StoredImage {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
    /// Engine render time for this image.
    pub ms: f64,
}

/// Called exactly once with the engine's answer, from whichever thread has it.
pub type TileDone = Box<dyn FnOnce(TileAnswer) + Send + 'static>;

/// The engine-like side the protocol talks to.
pub trait Backend {
    /// Queue a tile; `done` is called exactly once (possibly before this returns).
    fn tile(&self, t: TileParams, done: TileDone);
    /// Take a stored image out of the store (each id can be taken once).
    fn take_image(&self, id: u64) -> Option<StoredImage>;
}

/// A parsed request.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GlRequest {
    Tile(TileParams),
    Img { id: u64 },
}

/// Parse the path and query of a `gl` URL. `Err` is the reply to send (400 or 404).
pub fn parse(path: &str, query: Option<&str>) -> Result<GlRequest, Reply> {
    let bad = || Reply::new(400);
    let q = Query::parse(query.unwrap_or("")).ok_or_else(bad)?;
    match path.trim_end_matches('/') {
        "/tile" => {
            let t = TileParams {
                page: q.num("page").ok_or_else(bad)?,
                zoom: q.num("zoom").ok_or_else(bad)?,
                x0: q.num("x0").filter(|v: &i32| v.unsigned_abs() <= MAX_ORIGIN).ok_or_else(bad)?,
                y0: q.num("y0").filter(|v: &i32| v.unsigned_abs() <= MAX_ORIGIN).ok_or_else(bad)?,
                gen: q.num("gen").ok_or_else(bad)?,
                w: q.num_or("w", TILE_SIDE).map_err(|_| bad())?,
                h: q.num_or("h", TILE_SIDE).map_err(|_| bad())?,
            };
            if t.page < 0 || !(t.zoom > 0.0 && t.zoom <= MAX_ZOOM) || !(1..=MAX_SIDE).contains(&t.w) || !(1..=MAX_SIDE).contains(&t.h) {
                return Err(bad());
            }
            Ok(GlRequest::Tile(t))
        }
        "/img" => Ok(GlRequest::Img { id: q.num("id").ok_or_else(bad)? }),
        _ => Err(Reply::new(404)),
    }
}

/// The reply for an engine tile answer. `wait_ms` is the time from the request reaching the handler
/// to the answer reaching the dispatcher.
pub fn tile_reply(answer: TileAnswer, wait_ms: f64) -> Reply {
    let r = Reply::new(200).header("X-Gl-Wait-Ms", format!("{wait_ms:.1}"));
    match answer {
        TileAnswer::Tile { gen, rgba, w, h, render_ms } => {
            if rgba.len() as u64 != w as u64 * h as u64 * 4 {
                return Reply { status: 500, ..r };
            }
            r.header("X-Gl-Gen", gen).pixels(rgba, w, h, render_ms)
        }
        TileAnswer::Skipped { gen } => Reply { status: 204, ..r }.header("X-Gl-Gen", gen),
        TileAnswer::Error { code } => Reply { status: 422, ..r }.header("X-Gl-Error", code),
        TileAnswer::EngineGone => Reply { status: 503, ..r },
    }
}

/// The whole request -> reply step. `reply` is called exactly once: before this returns for
/// everything except a tile, and from the engine's answer (dispatcher thread) for a tile.
pub fn handle<B, F>(method: &str, path: &str, query: Option<&str>, backend: &B, reply: F)
where
    B: Backend + ?Sized,
    F: FnOnce(Reply) + Send + 'static,
{
    match method {
        "GET" => {}
        "OPTIONS" => {
            return reply(Reply::new(204).header("Access-Control-Allow-Methods", "GET").header("Access-Control-Max-Age", 600))
        }
        _ => return reply(Reply::new(405).header("Allow", "GET, OPTIONS")),
    }
    match parse(path, query) {
        Err(r) => reply(r),
        Ok(GlRequest::Img { id }) => match backend.take_image(id) {
            Some(i) => reply(Reply::new(200).pixels(i.rgba, i.w, i.h, i.ms)),
            None => reply(Reply::new(404)),
        },
        Ok(GlRequest::Tile(t)) => {
            let t0 = Instant::now();
            backend.tile(t, Box::new(move |a| reply(tile_reply(a, t0.elapsed().as_secs_f64() * 1e3))));
        }
    }
}

/// `k=v&k=v` with plain ASCII keys and values (numbers only here, so no percent-decoding). `None` if a
/// pair has no `=`, a key repeats, or anything is not printable ASCII.
struct Query<'a>(Vec<(&'a str, &'a str)>);

impl<'a> Query<'a> {
    fn parse(q: &'a str) -> Option<Query<'a>> {
        let mut pairs: Vec<(&str, &str)> = Vec::new();
        for part in q.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = part.split_once('=')?;
            if k.is_empty() || !part.bytes().all(|b| b.is_ascii_graphic()) || pairs.iter().any(|(pk, _)| *pk == k) {
                return None;
            }
            pairs.push((k, v));
        }
        Some(Query(pairs))
    }

    fn num<T: std::str::FromStr>(&self, key: &str) -> Option<T> {
        let v = self.0.iter().find(|(k, _)| *k == key)?.1;
        v.parse().ok()
    }

    /// An optional number: `Ok(default)` when the key is absent, `Err` when present but not a number.
    fn num_or<T: std::str::FromStr>(&self, key: &str, default: T) -> Result<T, ()> {
        match self.0.iter().find(|(k, _)| *k == key) {
            None => Ok(default),
            Some((_, v)) => v.parse().map_err(|_| ()),
        }
    }
}

/// Waiting tile responders, keyed by the request id sent to the engine.
pub struct TileRouter {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, TileDone>>,
}

impl Default for TileRouter {
    fn default() -> Self {
        TileRouter { next_id: AtomicU64::new(1), pending: Mutex::new(HashMap::new()) }
    }
}

impl TileRouter {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, TileDone>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register `done` and send the tile request. If the engine is gone, `done` gets `EngineGone` now.
    pub fn queue(&self, client: &EngineClient, t: TileParams, done: TileDone) {
        let id = self.next_id.fetch_add(1, Relaxed);
        // Registered before sending, so the answer can never arrive ahead of its responder.
        self.lock().insert(id, done);
        let sent = if t.w == TILE_SIDE && t.h == TILE_SIDE {
            client.send(Request::Tile { id, page: t.page, zoom: t.zoom, x0: t.x0, y0: t.y0, gen: t.gen })
        } else {
            client.send(Request::Region { id, page: t.page, zoom: t.zoom, x0: t.x0, y0: t.y0, w: t.w, h: t.h, gen: t.gen })
        };
        if !sent {
            if let Some(d) = self.lock().remove(&id) {
                d(TileAnswer::EngineGone);
            }
        }
    }

    /// Answer the responder an engine tile event belongs to. Returns the event when it is not a tile
    /// answer (or answers an id nobody waits for), for the caller to route elsewhere.
    pub fn route(&self, ev: Event) -> Option<Event> {
        let (id, answer) = match ev {
            Event::Tile { id, gen, rgba, w, h, render_ms, .. } => (id, TileAnswer::Tile { gen, rgba, w, h, render_ms }),
            Event::TileSkipped { id, gen } => (id, TileAnswer::Skipped { gen }),
            Event::Error { code, tile: Some(id) } => (id, TileAnswer::Error { code: code.code() }),
            other => return Some(other),
        };
        // Take it out under the lock, call it outside: the responder may do real work.
        let done = self.lock().remove(&id);
        if let Some(d) = done {
            d(answer);
        }
        None
    }

    /// The engine has stopped: answer everyone still waiting.
    pub fn fail_all(&self) {
        let all: Vec<TileDone> = self.lock().drain().map(|(_, d)| d).collect();
        for d in all {
            d(TileAnswer::EngineGone);
        }
    }

    /// Number of tiles still waiting for an answer.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn waiting(&self) -> usize {
        self.lock().len()
    }
}

/// Register the `gl` scheme on the Tauri builder. The handler runs on whatever thread the WebView
/// calls it on; it only parses and queues (see [`handle`]), and the responder is answered later from
/// the dispatcher thread.
#[cfg(not(test))]
pub fn register<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    use std::sync::Arc;
    use tauri::Manager;

    builder.register_asynchronous_uri_scheme_protocol("gl", |ctx, request, responder| {
        let respond = move |r: Reply| responder.respond(to_http(r));
        let uri = request.uri();
        match ctx.app_handle().try_state::<Arc<crate::host::Host>>() {
            Some(host) => handle(request.method().as_str(), uri.path(), uri.query(), host.inner().as_ref(), respond),
            None => respond(Reply::new(503)),
        }
    })
}

#[cfg(not(test))]
fn to_http(r: Reply) -> tauri::http::Response<Vec<u8>> {
    let mut b = tauri::http::Response::builder().status(r.status);
    for (k, v) in &r.headers {
        b = b.header(*k, v.as_str());
    }
    b.body(r.body).unwrap_or_else(|_| {
        let mut resp = tauri::http::Response::new(Vec::new());
        *resp.status_mut() = tauri::http::StatusCode::INTERNAL_SERVER_ERROR;
        resp
    })
}
