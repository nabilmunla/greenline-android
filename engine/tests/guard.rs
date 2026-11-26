//! Memory guard tests (2b.4). The tiny-PDF and store tests always run; the corpus tests skip with a
//! message if `corpus/heavy.pdf`, `corpus/lines.pdf` or `corpus/set.pdf` is absent
//! (`python3 tools/mkcorpus.py corpus lines set heavy`).
//!
//! The list-guard test uses `GL_LIST_GUARD_MB` if set, else 50 MB. Run with
//! `cargo test --release -p greenline-engine -j 8 --test guard -- --nocapture` to see the numbers.

mod common;

use std::time::{Duration, Instant};

use common::{assert_rgba_shape, corpus, non_white, serial, tiny_pdf};
use greenline_engine::api::{Event, MemReason, Request, Stage};
use greenline_engine::guard::{self, GuardConfig, TrimKind, STORE_CAP_BYTES};
use greenline_engine::worker::Engine;
use greenline_engine::{mem, Rgba, FIT_WIDTH, TILE_SIZE};

const LONG: Duration = Duration::from_secs(180);

fn next(e: &Engine, timeout: Duration) -> Event {
    e.events().recv_timeout(timeout).unwrap_or_else(|err| panic!("no event within {timeout:?}: {err}"))
}

/// Events up to and including the first one matching `stop`, each with its arrival time. While
/// waiting, RSS is sampled every 20 ms; the highest reading is returned too.
fn until_peak(e: &Engine, timeout: Duration, stop: impl Fn(&Event) -> bool) -> (Vec<(Instant, Event)>, f64) {
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    let mut peak = mem::rss_mb();
    loop {
        assert!(Instant::now() < deadline, "timed out; events so far: {:?}", out.iter().map(|(_, e)| short(e)).collect::<Vec<_>>());
        match e.events().recv_timeout(Duration::from_millis(20)) {
            Ok(ev) => {
                let done = stop(&ev);
                out.push((Instant::now(), ev));
                if done {
                    return (out, peak.max(mem::rss_mb()));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(err) => panic!("event stream closed: {err}"),
        }
        peak = peak.max(mem::rss_mb());
    }
}

fn until(e: &Engine, timeout: Duration, stop: impl Fn(&Event) -> bool) -> Vec<(Instant, Event)> {
    until_peak(e, timeout, stop).0
}

fn open(e: &Engine, path: &std::path::Path) -> i32 {
    assert!(e.send(Request::Open { path: path.to_str().unwrap().into() }));
    match next(e, LONG) {
        Event::Opened { pages, .. } => pages,
        other => panic!("expected Opened, got {}", short(&other)),
    }
}

fn short(e: &Event) -> String {
    match e {
        Event::LowRes { page, w, h, ms, .. } => format!("LowRes{{page {page}, {w}x{h}, {ms:.0} ms}}"),
        Event::Fit { page, w, h, ms, .. } => format!("Fit{{page {page}, {w}x{h}, {ms:.0} ms}}"),
        Event::Tile { id, w, h, render_ms, .. } => format!("Tile{{id {id}, {w}x{h}, {render_ms:.0} ms}}"),
        Event::Progress { stage, ops, elapsed_ms, .. } => format!("Progress{{{stage:?}, ops {ops}, {elapsed_ms:.0} ms}}"),
        other => format!("{other:?}"),
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn img(px: &[u8], w: u32, h: u32) -> Rgba {
    Rgba { width: w, height: h, data: px.to_vec() }
}

fn pos(evs: &[(Instant, Event)], f: impl Fn(&Event) -> bool) -> Option<usize> {
    evs.iter().position(|(_, ev)| f(ev))
}

#[test]
fn store_cap_is_256_mb() {
    let _g = serial();
    // The live store of the mupdf crate's base context (shared by every thread's context).
    assert_eq!(guard::store_max_bytes(), STORE_CAP_BYTES);
    assert_eq!(STORE_CAP_BYTES, 256 << 20);
    // Through the engine: its gauge never exceeds the cap.
    let e = Engine::start().expect("start");
    open(&e, &tiny_pdf());
    assert!(e.send(Request::ShowPage { page: 0, fit_width: 800 }));
    until(&e, LONG, |ev| matches!(ev, Event::Fit { .. }));
    let s = e.stats();
    eprintln!("  store cap {} MB; store after a tiny page {} bytes", guard::store_max_bytes() >> 20, s.store_bytes);
    assert!(s.store_bytes <= STORE_CAP_BYTES as u64);
}

#[test]
fn env_overrides_and_defaults() {
    let d = GuardConfig::default();
    assert_eq!((d.list_guard_mb, d.soft_limit_mb), (700.0, 1100.0));
    let c = GuardConfig::from_env();
    eprintln!("  GuardConfig::from_env() = {c:?}");
    if let Ok(v) = std::env::var("GL_LIST_GUARD_MB") {
        assert_eq!(c.list_guard_mb, v.trim().parse::<f64>().unwrap());
    } else {
        assert_eq!(c.list_guard_mb, 700.0);
    }
}

/// Acceptance: with a small list guard on the heaviest synthetic sheet, the list is abandoned, the
/// page and a tile still come back by direct draw, and RSS drops back.
#[test]
fn heavy_list_guard_abandons_list_and_draws_directly() {
    let Some(path) = corpus("heavy.pdf") else {
        eprintln!("SKIP: heavy.pdf missing (python3 tools/mkcorpus.py corpus heavy)");
        return;
    };
    let _g = serial();
    let mut cfg = GuardConfig::from_env();
    if std::env::var_os("GL_LIST_GUARD_MB").is_none() {
        cfg.list_guard_mb = 50.0;
    }
    eprintln!("  guard config {cfg:?}");
    let e = Engine::start_with(cfg).expect("start");
    assert_eq!(e.client().guard_config(), cfg);
    open(&e, &path);
    mem::trim();
    let rss0 = mem::rss_mb();
    let t0 = Instant::now();
    assert!(e.send(Request::ShowPage { page: 0, fit_width: FIT_WIDTH }));
    let (evs, peak) = until_peak(&e, LONG, |ev| matches!(ev, Event::Fit { .. } | Event::Error { .. }));
    for (t, ev) in &evs {
        eprintln!("  +{:7.0} ms  {}", ms(*t - t0), short(ev));
    }
    let dm = pos(&evs, |ev| matches!(ev, Event::DirectMode { .. })).expect("DirectMode event");
    let low = pos(&evs, |ev| matches!(ev, Event::LowRes { .. })).expect("LowRes");
    let fit = pos(&evs, |ev| matches!(ev, Event::Fit { .. })).expect("Fit");
    assert!(dm < low && low < fit, "order DirectMode < LowRes < Fit");
    let Event::DirectMode { page, reason, rss_mb: rss_after } = evs[dm].1 else { unreachable!() };
    assert_eq!((page, reason), (0, MemReason::ListGuard));
    let abandon_ms = ms(evs[dm].0 - t0);
    match (&evs[low].1, &evs[fit].1) {
        (Event::LowRes { rgba: lp, w: lw, h: lh, ms: lms, .. }, Event::Fit { rgba: fp, w, h, ms: fms, .. }) => {
            let (li, fi) = (img(lp, *lw, *lh), img(fp, *w, *h));
            assert_rgba_shape(&fi, FIT_WIDTH, None);
            assert!(non_white(&li) > 0 && non_white(&fi) > 0);
            eprintln!("  direct LowRes {lms:.0} ms, direct Fit {fms:.0} ms");
        }
        _ => unreachable!(),
    }
    // The read reported progress before it was abandoned.
    assert!(evs[..dm].iter().any(|(_, ev)| matches!(ev, Event::Progress { stage: Stage::Read, .. })));

    // A tile of the same page comes back by direct draw, without a second read.
    let b = greenline_engine::Doc::open(&path).unwrap().page_bounds(0).unwrap();
    let (x0, y0) = b.tile_origin_at(4.0, 0.5, 0.5);
    let tt = Instant::now();
    assert!(e.send(Request::Tile { id: 1, page: 0, zoom: 4.0, x0, y0, gen: 1 }));
    let (tevs, tpeak) = until_peak(&e, LONG, |ev| matches!(ev, Event::Tile { .. } | Event::TileSkipped { .. } | Event::Error { .. }));
    let tile_ms = ms(tt.elapsed());
    match &tevs.last().unwrap().1 {
        Event::Tile { id: 1, rgba: px, w, h, .. } => {
            let t = img(px, *w, *h);
            assert_rgba_shape(&t, TILE_SIZE, Some(TILE_SIZE));
            assert!(non_white(&t) > 0);
        }
        other => panic!("expected the direct Tile, got {}", short(other)),
    }
    assert!(!tevs.iter().any(|(_, ev)| matches!(ev, Event::Progress { stage: Stage::Read, .. })), "no second read");
    mem::trim();
    let rss_end = mem::rss_mb();
    let s = e.stats();
    eprintln!(
        "  heavy.pdf, list guard {:.0} MB: read abandoned at {abandon_ms:.0} ms; RSS {rss0:.0} MB before, peak {peak:.0} MB \
         during the read, {rss_after:.0} MB after the abandon, tile peak {tpeak:.0} MB, {rss_end:.0} MB at the end; \
         direct tile {tile_ms:.0} ms; {s:?}",
        cfg.list_guard_mb
    );
    assert!(peak - rss0 > cfg.list_guard_mb, "the read never grew past the guard ({rss0:.0} -> {peak:.0} MB)");
    // RSS drops back: what the partial list took is given back.
    assert!(rss_after - rss0 < 40.0, "RSS did not drop back: {rss0:.0} MB before, {rss_after:.0} MB after the abandon");
    assert!(rss_end - rss0 < 60.0, "RSS {rss_end:.0} MB at the end vs {rss0:.0} MB before");
    assert_eq!((s.lists_built, s.reads_abandoned, s.direct_renders), (0, 1, 3), "{s:?}");
}

/// Acceptance: the trim flag drops an idle list, and aborts a read in flight (the page is then drawn
/// directly).
#[test]
fn trim_drops_idle_list_and_aborts_read() {
    let _g = serial();
    let e = Engine::start_with(GuardConfig { list_guard_mb: 0.0, soft_limit_mb: 0.0 }).expect("start");
    open(&e, &tiny_pdf());
    assert!(e.send(Request::ShowPage { page: 0, fit_width: 800 }));
    until(&e, LONG, |ev| matches!(ev, Event::Fit { .. }));
    assert_eq!(e.stats().lists_built, 1);

    // A light trim (onStop / UI_HIDDEN, task 2bf.2) keeps the list.
    guard::request_trim(TrimKind::Background);
    guard::request_trim(TrimKind::Light);
    std::thread::sleep(Duration::from_millis(200));
    while let Ok(ev) = e.events().try_recv() {
        assert!(matches!(ev, Event::MemoryReleased { had_list: false, .. }), "light trim gave {}", short(&ev));
    }
    assert!(e.send(Request::ShowPage { page: 0, fit_width: 800 }));
    until(&e, LONG, |ev| matches!(ev, Event::Fit { .. }));
    assert_eq!(e.stats().lists_built, 1, "the list must survive a light trim");

    // Idle engine: a hard trim wakes it and the list goes at once.
    let t = Instant::now();
    guard::request_trim(TrimKind::TrimMemory);
    assert_eq!(guard::trim_kind(), TrimKind::TrimMemory as u8);
    match next(&e, Duration::from_secs(2)) {
        Event::MemoryReleased { reason: MemReason::Trim, had_list: true, rss_mb } => {
            eprintln!("  idle trim: list released in {:.1} ms (RSS {rss_mb:.0} MB)", ms(t.elapsed()))
        }
        other => panic!("expected MemoryReleased, got {}", short(&other)),
    }
    assert!(e.send(Request::DropList));
    assert_eq!(next(&e, LONG), Event::Dropped { had_list: false });
    // A second trim with nothing held sends nothing.
    guard::request_trim(TrimKind::TrimMemory);
    assert!(e.send(Request::DropList));
    assert_eq!(next(&e, LONG), Event::Dropped { had_list: false });
    // The page reads again on demand.
    assert!(e.send(Request::ShowPage { page: 0, fit_width: 800 }));
    until(&e, LONG, |ev| matches!(ev, Event::Fit { .. }));
    assert_eq!(e.stats().lists_built, 2);

    // A hard trim aborts a read in flight; the page is read again with a list, not drawn directly
    // (task 2bf.2).
    let Some(path) = corpus("heavy.pdf") else {
        eprintln!("SKIP (in-flight part): heavy.pdf missing");
        return;
    };
    open(&e, &path);
    mem::trim();
    assert!(e.send(Request::ShowPage { page: 0, fit_width: FIT_WIDTH }));
    until(&e, Duration::from_secs(10), |ev| matches!(ev, Event::Progress { stage: Stage::Read, ops, .. } if *ops > 0));
    std::thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    guard::request_trim(TrimKind::LowMemory);
    while e.stats().reads_abandoned == 0 {
        assert!(t.elapsed() < Duration::from_secs(1), "read not stopped within 1 s of the hard trim");
        std::thread::sleep(Duration::from_millis(5));
    }
    let abort_ms = ms(t.elapsed());
    let tail = until(&e, LONG, |ev| matches!(ev, Event::Fit { .. } | Event::DirectMode { .. }));
    let last = &tail.last().unwrap().1;
    assert!(matches!(last, Event::Fit { page: 0, .. }), "hard trim mid-read gave {}", short(last));
    eprintln!("  hard trim during heavy read: stopped {abort_ms:.0} ms after the trim, read again with a list");
    let s = e.stats();
    assert_eq!((s.lists_built, s.reads_abandoned), (3, 1), "{s:?}");
    // The next view uses that list (no further read).
    assert!(e.send(Request::ShowPage { page: 0, fit_width: FIT_WIDTH }));
    until(&e, LONG, |ev| matches!(ev, Event::Fit { .. }));
    assert_eq!(e.stats().lists_built, 3);
}

/// The soft limit: a read that takes the process past it is abandoned (reason SoftLimit) and the page
/// drawn directly.
#[test]
fn soft_limit_abandons_read() {
    let Some(path) = corpus("heavy.pdf") else { return };
    let _g = serial();
    let e = Engine::start_with(GuardConfig { list_guard_mb: 0.0, soft_limit_mb: 0.0 }).expect("probe");
    drop(e);
    mem::trim();
    let rss0 = mem::rss_mb();
    let soft = rss0 + 30.0;
    let e = Engine::start_with(GuardConfig { list_guard_mb: 0.0, soft_limit_mb: soft }).expect("start");
    open(&e, &path);
    let t0 = Instant::now();
    assert!(e.send(Request::ShowPage { page: 0, fit_width: FIT_WIDTH }));
    let (evs, peak) = until_peak(&e, LONG, |ev| matches!(ev, Event::DirectMode { .. } | Event::LowRes { .. } | Event::Error { .. }));
    let Event::DirectMode { page: 0, reason: MemReason::SoftLimit, rss_mb } = evs.last().unwrap().1 else {
        panic!("expected DirectMode(SoftLimit), got {}", short(&evs.last().unwrap().1))
    };
    eprintln!(
        "  soft limit {soft:.0} MB: read abandoned at {:.0} ms; RSS {rss0:.0} MB at start, peak {peak:.0} MB, {rss_mb:.0} MB after",
        ms(evs.last().unwrap().0 - t0)
    );
    assert!(rss_mb < soft, "still over the soft limit after the release");
    // The page is then drawn directly; stop it there to keep the test short.
    assert!(e.cancel());
    until(&e, LONG, |ev| matches!(ev, Event::Cancelled));
    let s = e.stats();
    assert_eq!((s.lists_built, s.reads_abandoned), (0, 1), "{s:?}");
}

/// A one-page PDF drawing a 512 x 512 RGB image (uncompressed, 768 KB): rendering it puts the decoded
/// image in MuPDF's store. (The synthetic corpus has no images; its fonts live in the document's own
/// resource tables, not the store.)
fn image_pdf() -> std::path::PathBuf {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("guard-image.pdf");
    let (w, h) = (512usize, 512usize);
    let pixels: Vec<u8> = (0..w * h).flat_map(|i| [(i % 251) as u8, (i / 7 % 253) as u8, 90]).collect();
    let content = b"q 400 0 0 400 100 100 cm /Im0 Do Q\n".to_vec();
    let objs: Vec<(String, Option<Vec<u8>>)> = vec![
        ("<< /Type /Catalog /Pages 2 0 R >>".into(), None),
        ("<< /Type /Pages /Count 1 /Kids [3 0 R] >>".into(), None),
        ("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 600] /Contents 4 0 R /Resources << /XObject << /Im0 5 0 R >> >> >>".into(), None),
        (format!("<< /Length {} >>", content.len()), Some(content)),
        (
            format!("<< /Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Length {} >>", pixels.len()),
            Some(pixels),
        ),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offs = Vec::new();
    for (i, (dict, stream)) in objs.iter().enumerate() {
        offs.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{dict}\n", i + 1).as_bytes());
        if let Some(s) = stream {
            out.extend_from_slice(b"stream\n");
            out.extend_from_slice(s);
            out.extend_from_slice(b"\nendstream\n");
        }
        out.extend_from_slice(b"endobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in &offs {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
    std::fs::write(&path, out).expect("write guard-image.pdf");
    path
}

/// The store gauge reads the live store: rendering an image page fills it, `empty_store` empties it,
/// and an engine trim empties it too.
#[test]
fn store_fills_and_empties() {
    let _g = serial();
    let path = image_pdf();
    guard::empty_store();
    let s0 = guard::store_size_bytes();
    {
        let doc = greenline_engine::Doc::open(&path).unwrap();
        let r = doc.render_fit(0, 800).unwrap();
        assert!(non_white(&r.image) > 0);
        let s1 = guard::store_size_bytes();
        guard::empty_store();
        let s2 = guard::store_size_bytes();
        eprintln!("  store (this thread): {s0} bytes before, {s1} bytes after an image page, {s2} after empty_store");
        assert!(s1 >= 512 * 512 * 3, "the decoded image is not in the store ({s1} bytes)");
        assert_eq!(s2, 0);
    }
    // Through the engine: the gauge sees the image, and a trim releases it (no list involved).
    let e = Engine::start_with(GuardConfig { list_guard_mb: 0.0, soft_limit_mb: 0.0 }).expect("start");
    open(&e, &path);
    assert!(e.send(Request::ShowPage { page: 0, fit_width: 800 }));
    until(&e, LONG, |ev| matches!(ev, Event::Fit { .. }));
    let before = e.stats().store_bytes;
    assert!(e.send(Request::DropList));
    assert_eq!(next(&e, LONG), Event::Dropped { had_list: true });
    guard::request_trim(TrimKind::TrimMemory);
    assert!(matches!(next(&e, Duration::from_secs(2)), Event::MemoryReleased { reason: MemReason::Trim, had_list: false, .. }));
    assert!(e.send(Request::DropList)); // a round trip, so the gauge has been updated
    assert_eq!(next(&e, LONG), Event::Dropped { had_list: false });
    let after = e.stats().store_bytes;
    eprintln!("  engine store gauge: {before} bytes after the image page, {after} after a trim");
    assert!(before >= 512 * 512 * 3 && after == 0);
}

/// Acceptance: walking all 2,000 pages of set.pdf through the engine (each page read into a list,
/// low-res and fit rendered, the list dropped on page change) keeps RSS flat; the guard never trips.
#[test]
fn set_walk_2000_pages_stays_flat() {
    let Some(path) = corpus("set.pdf") else { return };
    let _g = serial();
    let e = Engine::start().expect("start");
    let pages = open(&e, &path);
    assert_eq!(pages, 2000);
    mem::trim();
    let rss0 = mem::rss_mb();
    let t0 = Instant::now();
    let mut samples: Vec<(i32, f64)> = Vec::new();
    let mut store_max = 0u64;
    for page in 0..pages {
        assert!(e.send(Request::ShowPage { page, fit_width: 800 }));
        let evs = until(&e, LONG, |ev| matches!(ev, Event::Fit { .. } | Event::Error { .. }));
        assert!(matches!(evs.last().unwrap().1, Event::Fit { .. }), "page {page}: {}", short(&evs.last().unwrap().1));
        store_max = store_max.max(e.stats().store_bytes);
        if page % 100 == 99 {
            samples.push((page + 1, mem::rss_mb()));
        }
    }
    let secs = t0.elapsed().as_secs_f64();
    let s = e.stats();
    let line: Vec<String> = samples.iter().map(|(p, r)| format!("{p}:{r:.0}")).collect();
    eprintln!(
        "  set.pdf walk, 2000 pages in {secs:.1} s; RSS {rss0:.0} MB at start, after every 100 pages: {}; peak {:.0} MB; \
         store max {:.1} MB; {s:?}",
        line.join(" "),
        mem::peak_mb(),
        store_max as f64 / 1048576.0
    );
    assert_eq!((s.lists_built, s.reads_abandoned, s.memory_releases), (2000, 0, 0), "{s:?}");
    // Flat: after the first 200 pages (fonts and caches warm), RSS moves less than 40 MB.
    let warm: Vec<f64> = samples.iter().filter(|(p, _)| *p > 200).map(|(_, r)| *r).collect();
    let (lo, hi) = warm.iter().fold((f64::MAX, f64::MIN), |(lo, hi), r| (lo.min(*r), hi.max(*r)));
    assert!(hi - lo < 40.0, "RSS not flat over the walk: {lo:.0}..{hi:.0} MB");
    assert!(store_max <= STORE_CAP_BYTES as u64);
}

// 2bf.1: reproductions of the phone's "direct mode" on an ordinary sheet (phone test, 25 Nov 2026). Both
// state the wanted behaviour: the SAF picker's light trims (onStop / UI_HIDDEN) must not throw a page
// into direct mode.

/// A light trim (onStop) arriving in the middle of a read lets the read finish with a list.
#[test]
fn repro_light_trim_mid_read_keeps_list() {
    let _g = serial();
    let Some(path) = corpus("heavy.pdf") else {
        eprintln!("SKIP: heavy.pdf missing");
        return;
    };
    let e = Engine::start_with(GuardConfig { list_guard_mb: 0.0, soft_limit_mb: 0.0 }).expect("start");
    open(&e, &path);
    assert!(e.send(Request::ShowPage { page: 0, fit_width: FIT_WIDTH }));
    until(&e, Duration::from_secs(10), |ev| matches!(ev, Event::Progress { stage: Stage::Read, ops, .. } if *ops > 0));
    guard::request_trim(TrimKind::Background);
    let evs = until(&e, LONG, |ev| matches!(ev, Event::Fit { .. } | Event::DirectMode { .. }));
    let last = &evs.last().unwrap().1;
    assert!(matches!(last, Event::Fit { .. }), "light trim mid-read gave {}", short(last));
    assert_eq!(e.stats().reads_abandoned, 0);
}

/// A trim that arrived before the open (the picker covering the app) is not replayed into the read.
#[test]
fn repro_trim_before_open_is_not_replayed() {
    let _g = serial();
    let Some(path) = corpus("heavy.pdf") else {
        eprintln!("SKIP: heavy.pdf missing");
        return;
    };
    let e = Engine::start_with(GuardConfig { list_guard_mb: 0.0, soft_limit_mb: 0.0 }).expect("start");
    guard::request_trim(TrimKind::Background);
    guard::request_trim(TrimKind::TrimMemory);
    std::thread::sleep(Duration::from_millis(200));
    open(&e, &path);
    assert!(e.send(Request::ShowPage { page: 0, fit_width: FIT_WIDTH }));
    let evs = until(&e, LONG, |ev| matches!(ev, Event::Fit { .. } | Event::DirectMode { .. }));
    let last = &evs.last().unwrap().1;
    assert!(matches!(last, Event::Fit { .. }), "trim before open gave {}", short(last));
    assert_eq!(e.stats().reads_abandoned, 0);
}