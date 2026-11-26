//! Host test of the app's `gl` protocol (task 2b.6): the pure request -> reply function in
//! `app/src-tauri/src/protocol.rs`, run against the real engine worker on a synthetic sheet.
//!
//! The app crate itself cannot build on the host (Tauri needs webkit2gtk), so this test compiles
//! protocol.rs directly; under `cfg(test)` it leaves out its Tauri glue and needs only std and this
//! crate. Run: `cargo test --release -p greenline-engine -j 8 --test app_protocol`.

mod common;

#[allow(dead_code)]
#[path = "../../app/src-tauri/src/protocol.rs"]
mod protocol;

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{non_white, serial, tiny_pdf};
use greenline_engine::api::{Event, Request};
use greenline_engine::worker::{Engine, EngineClient};
use greenline_engine::{Rgba, TILE_SIZE};
use protocol::{handle, Backend, Reply, StoredImage, TileDone, TileParams, TileRouter};

const WAIT: Duration = Duration::from_secs(60);

/// What the app's `Host` is, minus Tauri: the client, the tile router, an image store.
struct TestHost {
    client: EngineClient,
    tiles: TileRouter,
    images: Mutex<Vec<(u64, StoredImage)>>,
}

impl Backend for TestHost {
    fn tile(&self, t: TileParams, done: TileDone) {
        self.tiles.queue(&self.client, t, done);
    }
    fn take_image(&self, id: u64) -> Option<StoredImage> {
        let mut g = self.images.lock().unwrap();
        let i = g.iter().position(|(k, _)| *k == id)?;
        Some(g.remove(i).1)
    }
}

/// Start the engine with a dispatcher thread like the app's: tile events go to the router, the rest
/// to the returned channel.
fn start() -> (Arc<TestHost>, mpsc::Receiver<Event>) {
    let engine = Engine::start().expect("engine");
    let host = Arc::new(TestHost { client: engine.client(), tiles: TileRouter::default(), images: Mutex::new(Vec::new()) });
    let (tx, rx) = mpsc::channel();
    let h = host.clone();
    std::thread::spawn(move || {
        for ev in engine.events().iter() {
            if let Some(other) = h.tiles.route(ev) {
                if tx.send(other).is_err() {
                    break;
                }
            }
        }
        h.tiles.fail_all();
        // `engine` drops here: Shutdown + join.
    });
    (host, rx)
}

fn open_tiny(host: &TestHost, events: &mpsc::Receiver<Event>) {
    assert!(host.client.send(Request::Open { path: tiny_pdf().to_str().unwrap().into() }));
    match events.recv_timeout(WAIT).expect("no Opened") {
        Event::Opened { pages, .. } => assert_eq!(pages, 3),
        other => panic!("expected Opened, got {other:?}"),
    }
}

/// Run `handle` and wait for its reply (sync replies and engine-answered ones alike).
fn request(host: &TestHost, method: &str, path: &str, query: Option<&str>) -> Reply {
    let (tx, rx) = mpsc::channel();
    handle(method, path, query, host, move |r| {
        let _ = tx.send(r);
    });
    rx.recv_timeout(WAIT).expect("no reply")
}

fn get(host: &TestHost, path: &str, query: &str) -> Reply {
    request(host, "GET", path, Some(query))
}

fn assert_cors(r: &Reply) {
    assert_eq!(r.get("Access-Control-Allow-Origin"), Some("http://tauri.localhost"));
    let exposed: Vec<&str> = r.get("Access-Control-Expose-Headers").expect("expose header").split(", ").collect();
    for h in ["X-Gl-Render-Ms", "X-Gl-W", "X-Gl-H"] {
        assert!(exposed.contains(&h), "{h} not exposed: {exposed:?}");
    }
}

#[test]
fn tile_is_raw_rgba_4mib_with_headers_and_gen() {
    let _g = serial();
    let (host, events) = start();
    open_tiny(&host, &events);

    // Zoom 4 of the 612 x 792 pt page 1: this tile covers the blue rectangle.
    let r = get(&host, "/tile", "page=0&zoom=4&x0=1024&y0=2048&gen=7");
    assert_eq!(r.status, 200);
    assert_eq!(r.body.len(), 1024 * 1024 * 4, "4 MiB body");
    assert_cors(&r);
    assert_eq!(r.get("X-Gl-W"), Some("1024"));
    assert_eq!(r.get("X-Gl-H"), Some("1024"));
    assert_eq!(r.get("X-Gl-Gen"), Some("7"), "gen passed through the engine and back");
    assert_eq!(r.get("Content-Type"), Some("application/octet-stream"));
    let render_ms: f64 = r.get("X-Gl-Render-Ms").expect("render ms").parse().expect("render ms is a number");
    let wait_ms: f64 = r.get("X-Gl-Wait-Ms").expect("wait ms").parse().expect("wait ms is a number");
    assert!(render_ms >= 0.0 && wait_ms >= render_ms, "render {render_ms} ms, wait {wait_ms} ms");
    let img = Rgba { width: TILE_SIZE, height: TILE_SIZE, data: r.body };
    assert!(img.data.chunks_exact(4).all(|p| p[3] == 255), "opaque RGBA");
    assert!(non_white(&img) > 10_000, "the tile shows the page content");
    eprintln!("  tile: {} bytes, render {render_ms:.1} ms, handler->answer {wait_ms:.1} ms", img.data.len());

    // A second gen is passed through as well.
    let r = get(&host, "/tile", "page=2&zoom=1.5&x0=-10&y0=0&gen=123456789012");
    assert_eq!((r.status, r.body.len()), (200, 1024 * 1024 * 4));
    assert_eq!(r.get("X-Gl-Gen"), Some("123456789012"));

    // A tile of an older view generation is skipped unrendered: 204, its gen echoed.
    let r = get(&host, "/tile", "page=0&zoom=4&x0=0&y0=0&gen=8");
    assert_eq!((r.status, r.get("X-Gl-Gen")), (204, Some("8")));
    assert!(r.body.is_empty());
    assert_cors(&r);

    // Engine refusals come back as 422 with the fixed code (3 = PageOutOfRange), never text.
    let r = get(&host, "/tile", "page=99&zoom=4&x0=0&y0=0&gen=123456789013");
    assert_eq!(r.status, 422);
    assert_eq!(r.get("X-Gl-Error"), Some("3"));
    assert!(r.body.is_empty());
    assert_cors(&r);
    assert_eq!(host.tiles.waiting(), 0, "every tile answered exactly once");
}

/// 2c.2: a region of any size up to 4096 per side comes back at that size, and its pixels match the
/// 1024 tile at the same origin (it is the same render, just larger).
#[test]
fn region_is_served_at_its_size_and_matches_the_tile() {
    let _g = serial();
    let (host, events) = start();
    open_tiny(&host, &events);
    let tile = get(&host, "/tile", "page=0&zoom=4&x0=1024&y0=2048&gen=30");
    assert_eq!(tile.status, 200);
    let r = get(&host, "/tile", "page=0&zoom=4&x0=1024&y0=2048&w=1080&h=2400&gen=31");
    assert_eq!(r.status, 200);
    assert_eq!(r.body.len(), 1080 * 2400 * 4);
    assert_eq!((r.get("X-Gl-W"), r.get("X-Gl-H")), (Some("1080"), Some("2400")));
    for y in 0..1024usize {
        let a = &tile.body[y * 1024 * 4..(y + 1) * 1024 * 4];
        let b = &r.body[y * 1080 * 4..y * 1080 * 4 + 1024 * 4];
        assert!(a == b, "row {y} differs between the region and the tile");
    }
    assert_eq!(host.tiles.waiting(), 0);
}

#[test]
fn malformed_requests_get_400_without_touching_the_engine() {
    // No document is open and no engine call is made: these are answered by the parser.
    let (host, _events) = start();
    for q in [
        "",
        "page=0&zoom=4&x0=0&y0=0",              // gen missing
        "page=0&zoom=4&x0=0&y0=0&gen=abc",      // not a number
        "page=0&zoom=4&x0=0&y0=0&gen=-1",       // gen is unsigned
        "page=-1&zoom=4&x0=0&y0=0&gen=1",       // negative page
        "page=0&zoom=0&x0=0&y0=0&gen=1",        // zoom must be > 0
        "page=0&zoom=NaN&x0=0&y0=0&gen=1",      // not finite
        "page=0&zoom=1000&x0=0&y0=0&gen=1",     // above MAX_ZOOM
        "page=0&zoom=4&x0=0.5&y0=0&gen=1",      // pixel origin is an integer
        "page=0&zoom=4&x0=0&y0=0&gen=1&gen=2",  // repeated key
        "page=0&zoom=4&x0&y0=0&gen=1",          // pair without '='
        "page=0&zoom=4&x0=%30&y0=0&gen=1",      // percent-encoding is not accepted
        "page=0&zoom=4&x0=0&y0=0&gen=1&w=0",    // region side 0 (2c.2)
        "page=0&zoom=4&x0=0&y0=0&gen=1&h=4097", // region side above 4096
        "page=0&zoom=4&x0=0&y0=0&gen=1&w=-5",   // region side is unsigned
    ] {
        let r = get(&host, "/tile", q);
        assert_eq!(r.status, 400, "query {q:?}");
        assert!(r.body.is_empty());
        assert_cors(&r);
    }
    assert_eq!(request(&host, "GET", "/tile", None).status, 400);
    // DS.5 / BUG-08: an origin far outside any page is refused at the boundary.
    assert_eq!(get(&host, "/tile", "page=0&zoom=1&x0=2147482624&y0=0&w=4096&h=16&gen=1").status, 400);
    assert_eq!(get(&host, "/tile", "page=0&zoom=1&x0=0&y0=-2147482624&w=16&h=16&gen=1").status, 400);
    // DS.5 / BUG-20: replies are readable by the app's own page only, not by any origin.
    assert_eq!(get(&host, "/img", "id=x").get("Access-Control-Allow-Origin"), Some("http://tauri.localhost"));
    assert_eq!(get(&host, "/img", "id=x").status, 400);
    assert_eq!(get(&host, "/img", "").status, 400);
    assert_eq!(get(&host, "/nope", "id=1").status, 404);
    assert_eq!(request(&host, "POST", "/tile", Some("page=0&zoom=4&x0=0&y0=0&gen=1")).status, 405);
    assert_eq!(request(&host, "OPTIONS", "/tile", None).status, 204);
    assert_eq!(host.tiles.waiting(), 0);
}

#[test]
fn img_is_served_once() {
    let (host, _events) = start();
    let (w, h) = (600u32, 776u32);
    host.images.lock().unwrap().push((5, StoredImage { rgba: vec![255; (w * h * 4) as usize], w, h, ms: 12.0 }));
    let r = get(&host, "/img", "id=5");
    assert_eq!(r.status, 200);
    assert_eq!(r.body.len(), (w * h * 4) as usize);
    assert_eq!((r.get("X-Gl-W"), r.get("X-Gl-H"), r.get("X-Gl-Render-Ms")), (Some("600"), Some("776"), Some("12.0")));
    assert_cors(&r);
    assert_eq!(get(&host, "/img", "id=5").status, 404, "taken on the first fetch");
}

#[test]
fn tiles_without_a_document_are_answered() {
    let _g = serial();
    let (host, _events) = start();
    // 1 = NoDocument. The app's close fence relies on exactly this: an answer after Close.
    let r = get(&host, "/tile", "page=0&zoom=1&x0=0&y0=0&gen=0");
    assert_eq!((r.status, r.get("X-Gl-Error")), (422, Some("1")));
}
