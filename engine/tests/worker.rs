//! Engine worker tests (2b.3). The tiny PDF tests always run; the corpus tests skip with a message if
//! `corpus/lines.pdf` or `corpus/heavy.pdf` is absent (`python3 tools/mkcorpus.py corpus lines heavy`).
//! Run with `cargo test --release -p greenline-engine -j 8 -- --nocapture` to see the timings.

mod common;

use std::time::{Duration, Instant};

use common::{assert_rgba_shape, corpus, non_white, serial, tiny_pdf};
use greenline_engine::api::{ErrorCode, Event, Request, Stage, LOWRES_WIDTH, PROGRESS_EVERY_MS};
use greenline_engine::worker::{Engine, SharedCookie};
use greenline_engine::{mem, Rgba, FIT_WIDTH, TILE_SIZE, TILE_ZOOM};

const LONG: Duration = Duration::from_secs(120);

/// Next event, or panic after `timeout`.
fn next(e: &Engine, timeout: Duration) -> Event {
    e.events().recv_timeout(timeout).unwrap_or_else(|err| panic!("no event within {timeout:?}: {err}"))
}

/// Events up to and including the first one matching `stop`, each with its arrival time.
fn until(e: &Engine, timeout: Duration, stop: impl Fn(&Event) -> bool) -> Vec<(Instant, Event)> {
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let ev = next(e, left.max(Duration::from_millis(1)));
        let done = stop(&ev);
        out.push((Instant::now(), ev));
        if done {
            return out;
        }
    }
}

fn open(e: &Engine, path: &std::path::Path) -> i32 {
    assert!(e.send(Request::Open { path: path.to_str().unwrap().into() }));
    match next(e, LONG) {
        Event::Opened { pages, open_ms } => {
            eprintln!("  opened: {pages} pages in {open_ms:.1} ms");
            pages
        }
        other => panic!("expected Opened, got {}", short(&other)),
    }
}

/// Event without the pixel bytes, for messages.
fn short(e: &Event) -> String {
    match e {
        Event::LowRes { page, w, h, ms, .. } => format!("LowRes{{page {page}, {w}x{h}, {ms:.1} ms}}"),
        Event::Fit { page, w, h, ms, .. } => format!("Fit{{page {page}, {w}x{h}, {ms:.1} ms}}"),
        Event::Tile { id, gen, w, h, render_ms, .. } => format!("Tile{{id {id}, gen {gen}, {w}x{h}, {render_ms:.1} ms}}"),
        other => format!("{other:?}"),
    }
}

fn rgba(rgba: &[u8], w: u32, h: u32) -> Rgba {
    Rgba { width: w, height: h, data: rgba.to_vec() }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[test]
fn shared_cookie_is_send_sync_and_aborts() {
    fn send_sync<T: Send + Sync + Clone>() {}
    send_sync::<SharedCookie>();
    let c = SharedCookie::new();
    assert!(!c.is_aborted());
    let c2 = c.clone();
    std::thread::spawn(move || c2.abort()).join().unwrap();
    assert!(c.is_aborted());
    assert_eq!(c.progress(), 0);
}

#[test]
fn tiny_show_page_tiles_and_errors() {
    let _g = serial();
    let e = Engine::start().expect("start");

    // Before any document.
    assert!(e.send(Request::Tile { id: 1, page: 0, zoom: 1.0, x0: 0, y0: 0, gen: 0 }));
    assert_eq!(next(&e, LONG), Event::Error { code: ErrorCode::NoDocument, tile: Some(1) });
    assert!(e.send(Request::Open { path: "/nonexistent/x.pdf".into() }));
    assert_eq!(next(&e, LONG), Event::Error { code: ErrorCode::OpenFailed, tile: None });

    let pages = open(&e, &tiny_pdf());
    assert_eq!(pages, 3);

    // ShowPage: Progress (at least the final one), then LowRes, then Fit.
    assert!(e.send(Request::ShowPage { page: 0, fit_width: 800 }));
    let evs = until(&e, LONG, |ev| matches!(ev, Event::Fit { .. }));
    let kinds: Vec<String> = evs.iter().map(|(_, ev)| short(ev)).collect();
    eprintln!("  tiny ShowPage: {kinds:?}");
    let lowres = evs.iter().position(|(_, ev)| matches!(ev, Event::LowRes { .. })).expect("LowRes");
    assert!(evs[..lowres].iter().any(|(_, ev)| matches!(ev, Event::Progress { stage: Stage::Read, .. })));
    match &evs[lowres].1 {
        Event::LowRes { page, rgba: px, w, h, .. } => {
            assert_eq!((*page, *w), (0, LOWRES_WIDTH));
            let img = rgba(px, *w, *h);
            assert_rgba_shape(&img, LOWRES_WIDTH, None);
            assert!(non_white(&img) > 0);
        }
        _ => unreachable!(),
    }
    match &evs.last().unwrap().1 {
        Event::Fit { page, rgba: px, w, h, .. } => {
            assert_eq!((*page, *w), (0, 800));
            let img = rgba(px, *w, *h);
            assert_rgba_shape(&img, 800, None);
            assert!(non_white(&img) > 0);
        }
        _ => unreachable!(),
    }
    assert_eq!(e.stats().lists_built, 1);

    // Same page again reuses the list (no second read). A fit width under 600 gives a LowRes that wide.
    assert!(e.send(Request::ShowPage { page: 0, fit_width: 300 }));
    let evs = until(&e, LONG, |ev| matches!(ev, Event::Fit { .. }));
    assert!(evs.iter().any(|(_, ev)| matches!(ev, Event::LowRes { w: 300, list_ms, .. } if *list_ms == 0.0)));
    assert_eq!(e.stats().lists_built, 1);

    // A tile on the current page.
    assert!(e.send(Request::Tile { id: 7, page: 0, zoom: 2.0, x0: 0, y0: 0, gen: 1 }));
    match next(&e, LONG) {
        Event::Tile { id, gen, page, rgba: px, w, h, .. } => {
            assert_eq!((id, gen, page), (7, 1, 0));
            let img = rgba(&px, w, h);
            assert_rgba_shape(&img, TILE_SIZE, Some(TILE_SIZE));
            assert!(non_white(&img) > 0);
        }
        other => panic!("expected Tile, got {}", short(&other)),
    }
    // A tile on another page switches the list (one list at a time).
    assert!(e.send(Request::Tile { id: 8, page: 1, zoom: 1.0, x0: 0, y0: 0, gen: 1 }));
    let evs = until(&e, LONG, |ev| !matches!(ev, Event::Progress { .. }));
    assert!(matches!(evs.last().unwrap().1, Event::Tile { id: 8, page: 1, .. }), "{}", short(&evs.last().unwrap().1));
    assert_eq!(e.stats().lists_built, 2);
    // An older generation than one already seen is skipped.
    assert!(e.send(Request::Tile { id: 9, page: 1, zoom: 1.0, x0: 0, y0: 0, gen: 0 }));
    assert_eq!(next(&e, LONG), Event::TileSkipped { id: 9, gen: 0 });

    // Errors carry fixed codes. (Asking for another page than the last one shown cancels first, DS.2.)
    assert!(e.send(Request::ShowPage { page: 3, fit_width: 800 }));
    assert_eq!(next(&e, LONG), Event::Cancelled);
    assert_eq!(next(&e, LONG), Event::Error { code: ErrorCode::PageOutOfRange, tile: None });
    assert!(e.send(Request::ShowPage { page: 0, fit_width: 0 }));
    assert_eq!(next(&e, LONG), Event::Cancelled);
    assert_eq!(next(&e, LONG), Event::Error { code: ErrorCode::InvalidRequest, tile: None });
    assert!(e.send(Request::Tile { id: 10, page: 1, zoom: 0.0, x0: 0, y0: 0, gen: 2 }));
    assert_eq!(next(&e, LONG), Event::Error { code: ErrorCode::InvalidRequest, tile: Some(10) });

    // DropList and an idle Cancel are acknowledged.
    assert!(e.send(Request::DropList));
    assert_eq!(next(&e, LONG), Event::Dropped { had_list: true });
    assert!(e.send(Request::DropList));
    assert_eq!(next(&e, LONG), Event::Dropped { had_list: false });
    assert!(e.cancel());
    assert_eq!(next(&e, LONG), Event::Cancelled);
    // Work queued before a Cancel is skipped; the Cancel is still acknowledged once.
    assert!(e.send(Request::Close));
    let pages = open(&e, &tiny_pdf());
    assert_eq!(pages, 3);
    let client = e.client();
    assert!(client.send(Request::Tile { id: 11, page: 0, zoom: 1.0, x0: 0, y0: 0, gen: 0 }));
    assert!(client.send(Request::Cancel));
    let evs = until(&e, LONG, |ev| matches!(ev, Event::Cancelled));
    let tail: Vec<String> = evs.iter().map(|(_, ev)| short(ev)).collect();
    // The tile either rendered before the engine saw the cancel, or was skipped: never both, never lost.
    assert!(
        tail == ["TileSkipped { id: 11, gen: 0 }", "Cancelled"] || (tail.len() >= 2 && tail[tail.len() - 2].starts_with("Tile{id 11")),
        "{tail:?}"
    );

    // Shutdown via the queue ends the thread; the event stream then disconnects.
    assert!(e.send(Request::Shutdown));
    let t = Instant::now();
    loop {
        match e.events().recv_timeout(Duration::from_secs(10)) {
            Ok(_) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(err) => panic!("event stream still open after Shutdown: {err}"),
        }
    }
    eprintln!("  shutdown in {:.1} ms", ms(t.elapsed()));
    assert!(!e.send(Request::Close), "send after shutdown must fail");
    e.shutdown();
}

#[test]
fn lines_first_progress_then_lowres_before_fit() {
    let Some(path) = corpus("lines.pdf") else { return };
    let _g = serial();
    let e = Engine::start().expect("start");
    open(&e, &path);
    let t0 = Instant::now();
    assert!(e.send(Request::ShowPage { page: 0, fit_width: FIT_WIDTH }));
    let evs = until(&e, LONG, |ev| matches!(ev, Event::Fit { .. } | Event::Error { .. }));
    let first_progress = evs.iter().find(|(_, ev)| matches!(ev, Event::Progress { .. })).expect("a Progress event");
    let first_ms = ms(first_progress.0 - t0);
    let lowres = evs.iter().position(|(_, ev)| matches!(ev, Event::LowRes { .. })).expect("LowRes");
    let fit = evs.iter().position(|(_, ev)| matches!(ev, Event::Fit { .. })).expect("Fit");
    for (t, ev) in &evs {
        eprintln!("  +{:7.1} ms  {}", ms(*t - t0), short(ev));
    }
    eprintln!(
        "  lines.pdf: first Progress {first_ms:.0} ms; LowRes at {:.0} ms; Fit at {:.0} ms",
        ms(evs[lowres].0 - t0),
        ms(evs[fit].0 - t0)
    );
    assert!(first_ms <= 1000.0, "first Progress after {first_ms:.0} ms");
    assert!(lowres < fit, "LowRes must come before Fit");
    match (&evs[lowres].1, &evs[fit].1) {
        (Event::LowRes { w: lw, h: lh, rgba: lp, list_ms, .. }, Event::Fit { w, h, rgba: fp, .. }) => {
            assert_eq!((*lw, *w), (LOWRES_WIDTH, FIT_WIDTH));
            assert!(*list_ms > 0.0);
            let (li, fi) = (rgba(lp, *lw, *lh), rgba(fp, *w, *h));
            assert_rgba_shape(&li, LOWRES_WIDTH, None);
            assert_rgba_shape(&fi, FIT_WIDTH, None);
            assert!(non_white(&li) > 0 && non_white(&fi) > 0);
        }
        _ => unreachable!(),
    }
    // Progress ticks keep coming at most every PROGRESS_EVERY_MS (+ scheduling slack) while a step runs.
    check_progress_gaps(&evs);
}

/// Consecutive Progress events of one stage are at most 250 ms apart.
fn check_progress_gaps(evs: &[(Instant, Event)]) {
    let mut last: Option<(Stage, f64)> = None;
    for (_, ev) in evs {
        if let Event::Progress { stage, elapsed_ms, .. } = ev {
            if let Some((s, prev)) = last {
                if s == *stage {
                    let gap = elapsed_ms - prev;
                    assert!(gap <= 250.0, "Progress gap {gap:.0} ms in {stage:?} (target {PROGRESS_EVERY_MS} ms)");
                }
            }
            last = Some((*stage, *elapsed_ms));
        }
    }
}

#[test]
fn heavy_cancel_mid_read_releases_memory() {
    let Some(path) = corpus("heavy.pdf") else {
        eprintln!("SKIP: heavy.pdf missing (python3 tools/mkcorpus.py corpus heavy)");
        return;
    };
    let _g = serial();
    let e = Engine::start().expect("start");
    open(&e, &path);
    mem::trim();
    let rss0 = mem::rss_mb();
    let t0 = Instant::now();
    assert!(e.send(Request::ShowPage { page: 0, fit_width: FIT_WIDTH }));
    // Let the read run for ~0.9 s (the whole read takes ~2 s on the WSL host), watching memory grow.
    let mut evs = Vec::new();
    let mut rss_peak = rss0;
    while t0.elapsed() < Duration::from_millis(900) {
        if let Ok(ev) = e.events().recv_timeout(Duration::from_millis(20)) {
            assert!(matches!(ev, Event::Progress { stage: Stage::Read, .. }), "read finished too early: {}", short(&ev));
            evs.push((Instant::now(), ev));
        }
        rss_peak = rss_peak.max(mem::rss_mb());
    }
    let first = evs.first().map(|(t, _)| ms(*t - t0)).expect("no Progress in 0.9 s");
    let tc = Instant::now();
    assert!(e.cancel());
    let tail = until(&e, Duration::from_secs(10), |ev| matches!(ev, Event::Cancelled));
    let cancel_ms = ms(tail.last().unwrap().0 - tc);
    let rss1 = mem::rss_mb();
    let ops = evs.iter().rev().find_map(|(_, ev)| if let Event::Progress { ops, .. } = ev { Some(*ops) } else { None });
    eprintln!(
        "  heavy.pdf: first Progress {first:.0} ms, {} Progress events in 0.9 s (last ops {ops:?}); \
         Cancelled {cancel_ms:.0} ms after cancel(); RSS {rss0:.0} MB before, {rss_peak:.0} MB at cancel, {rss1:.0} MB after",
        evs.len()
    );
    assert!(first <= 1000.0, "first Progress after {first:.0} ms");
    assert!(cancel_ms <= 1000.0, "Cancelled {cancel_ms:.0} ms after cancel()");
    assert!(
        tail.iter().all(|(_, ev)| matches!(ev, Event::Progress { .. } | Event::Cancelled)),
        "no LowRes/Fit after a cancelled read: {:?}",
        tail.iter().map(|(_, ev)| short(ev)).collect::<Vec<_>>()
    );
    evs.extend(tail);
    check_progress_gaps(&evs);
    assert!(rss_peak - rss0 > 50.0, "the read should have grown memory (rss {rss0:.0} -> {rss_peak:.0} MB)");
    assert!(rss1 - rss0 < 30.0, "memory not released: {rss0:.0} MB before, {rss1:.0} MB after");
    let s = e.stats();
    assert_eq!((s.lists_built, s.cancelled_steps), (0, 1), "{s:?}");

    // The engine is still usable after the cancel: a tile reads the page again, cancelled again quickly.
    assert!(e.send(Request::Tile { id: 1, page: 0, zoom: 1.0, x0: 0, y0: 0, gen: 0 }));
    std::thread::sleep(Duration::from_millis(300));
    let tc = Instant::now();
    assert!(e.cancel());
    let tail = until(&e, Duration::from_secs(10), |ev| matches!(ev, Event::Cancelled));
    let cancel2_ms = ms(tail.last().unwrap().0 - tc);
    eprintln!("  heavy.pdf: tile-triggered read cancelled in {cancel2_ms:.0} ms");
    assert!(cancel2_ms <= 1000.0);
    assert!(tail.iter().any(|(_, ev)| matches!(ev, Event::TileSkipped { id: 1, .. })));
}

#[test]
fn lines_cancel_during_fit_render() {
    let Some(path) = corpus("lines.pdf") else { return };
    let _g = serial();
    let e = Engine::start().expect("start");
    open(&e, &path);
    let t0 = Instant::now();
    assert!(e.send(Request::ShowPage { page: 0, fit_width: FIT_WIDTH }));
    until(&e, LONG, |ev| matches!(ev, Event::LowRes { .. }));
    let low_ms = ms(t0.elapsed());
    std::thread::sleep(Duration::from_millis(100));
    let tc = Instant::now();
    assert!(e.cancel());
    let tail = until(&e, Duration::from_secs(10), |ev| matches!(ev, Event::Cancelled));
    let cancel_ms = ms(tail.last().unwrap().0 - tc);
    let got_fit = tail.iter().any(|(_, ev)| matches!(ev, Event::Fit { .. }));
    eprintln!("  lines.pdf: LowRes at {low_ms:.0} ms; cancel during Fit -> Cancelled in {cancel_ms:.0} ms (Fit delivered first: {got_fit})");
    assert!(cancel_ms <= 1000.0, "Cancelled {cancel_ms:.0} ms after cancel()");
    // The complete list survives a cancelled render: the next ShowPage of the same page does not re-read.
    assert!(e.send(Request::ShowPage { page: 0, fit_width: 400 }));
    let evs = until(&e, LONG, |ev| matches!(ev, Event::Fit { .. }));
    assert!(evs.iter().any(|(_, ev)| matches!(ev, Event::LowRes { list_ms, .. } if *list_ms == 0.0)));
    assert_eq!(e.stats().lists_built, 1);
}

#[test]
fn lines_stale_tiles_are_dropped_unrendered() {
    let Some(path) = corpus("lines.pdf") else { return };
    let _g = serial();
    let e = Engine::start().expect("start");
    open(&e, &path);
    let b = greenline_engine::Doc::open(&path).unwrap().page_bounds(0).unwrap();
    // The ShowPage keeps the engine busy (list read + two fit renders) while 51 tiles queue up behind it.
    assert!(e.send(Request::ShowPage { page: 0, fit_width: FIT_WIDTH }));
    for gen in 1..=50u64 {
        let (x0, y0) = b.tile_origin_at(TILE_ZOOM, 0.01 * gen as f32, 0.5);
        assert!(e.send(Request::Tile { id: gen, page: 0, zoom: TILE_ZOOM, x0, y0, gen }));
    }
    let (x0, y0) = b.tile_origin_at(TILE_ZOOM, 0.5, 0.5);
    assert!(e.send(Request::Tile { id: 51, page: 0, zoom: TILE_ZOOM, x0, y0, gen: 51 }));
    let tq = Instant::now();
    let evs = until(&e, LONG, |ev| matches!(ev, Event::Tile { .. } | Event::Error { .. }));
    let skipped: Vec<u64> =
        evs.iter().filter_map(|(_, ev)| if let Event::TileSkipped { id, .. } = ev { Some(*id) } else { None }).collect();
    let fit_before = evs.iter().position(|(_, ev)| matches!(ev, Event::Fit { .. }));
    match &evs.last().unwrap().1 {
        Event::Tile { id, gen, rgba: px, w, h, render_ms, .. } => {
            assert_eq!((*id, *gen), (51, 51));
            let img = rgba(px, *w, *h);
            assert_rgba_shape(&img, TILE_SIZE, Some(TILE_SIZE));
            assert!(non_white(&img) > 0);
            eprintln!(
                "  lines.pdf: 50 stale tiles skipped, newest rendered in {render_ms:.0} ms ({:.0} ms after queueing)",
                ms(evs.last().unwrap().0 - tq)
            );
        }
        other => panic!("expected the newest Tile, got {}", short(other)),
    }
    assert!(fit_before.is_some(), "the ShowPage finished first");
    assert_eq!(skipped, (1..=50).collect::<Vec<_>>());
    let s = e.stats();
    assert_eq!((s.tiles_rendered, s.tiles_skipped), (1, 50), "{s:?}");
    // Nothing else is in flight.
    assert!(e.events().recv_timeout(Duration::from_millis(200)).is_err());
}


/// DS.1 (bug sweep BUG-02): a region being drawn is stopped as soon as a newer region is queued, so the
/// newer view does not wait for a render nobody will see. The display-list read is never stopped by this.
#[test]
fn lines_inflight_region_is_stopped_by_a_newer_one() {
    let Some(path) = corpus("lines.pdf") else { return };
    let _g = serial();
    let e = Engine::start().expect("start");
    open(&e, &path);
    assert!(e.send(Request::ShowPage { page: 0, fit_width: 400 }));
    until(&e, LONG, |ev| matches!(ev, Event::Fit { .. }));
    let b = greenline_engine::Doc::open(&path).unwrap().page_bounds(0).unwrap();
    let zoom = 16.0f32;
    let (bx, by) = b.tile_origin_at(zoom, 0.3, 0.3);
    let big = |id: u64, gen: u64| Request::Region { id, page: 0, zoom, x0: bx, y0: by, w: 4096, h: 2048, gen };
    // Baseline: one big region alone.
    let t = Instant::now();
    assert!(e.send(big(1, 1)));
    let evs = until(&e, LONG, |ev| matches!(ev, Event::Tile { .. } | Event::TileSkipped { .. } | Event::Error { .. }));
    assert!(matches!(evs.last().unwrap().1, Event::Tile { id: 1, .. }), "baseline region: {}", short(&evs.last().unwrap().1));
    let full = ms(t.elapsed());
    if full < 300.0 {
        eprintln!("SKIP: the big region took only {full:.0} ms here, too fast to tell a stop from a finish");
        return;
    }
    // Same big region again, then a small newer one while the big one is being drawn.
    assert!(e.send(big(2, 2)));
    std::thread::sleep(Duration::from_millis((full * 0.25) as u64));
    let (sx, sy) = b.tile_origin_at(zoom, 0.6, 0.6);
    let tq = Instant::now();
    assert!(e.send(Request::Region { id: 3, page: 0, zoom, x0: sx, y0: sy, w: 256, h: 256, gen: 3 }));
    let evs = until(&e, LONG, |ev| matches!(ev, Event::Tile { id: 3, .. } | Event::Error { .. }));
    let newer = ms(tq.elapsed());
    let ids: Vec<String> = evs.iter().map(|(_, ev)| short(ev)).collect();
    eprintln!("  lines.pdf: big region alone {full:.0} ms; newer small region answered {newer:.0} ms after queueing ({ids:?})");
    assert!(evs.iter().any(|(_, ev)| matches!(ev, Event::TileSkipped { id: 2, .. })), "the older region was not stopped: {ids:?}");
    assert!(newer < full * 0.6, "the newer region waited {newer:.0} ms (a full big render is {full:.0} ms)");
    // A queued region older than one already sent is skipped unrendered.
    assert!(e.send(Request::Region { id: 4, page: 0, zoom, x0: sx, y0: sy, w: 256, h: 256, gen: 2 }));
    let evs = until(&e, LONG, |ev| matches!(ev, Event::Tile { .. } | Event::TileSkipped { .. } | Event::Error { .. }));
    assert!(matches!(evs.last().unwrap().1, Event::TileSkipped { id: 4, .. }), "stale region: {}", short(&evs.last().unwrap().1));
    // drop_regions: stop the region being drawn without sending a new one.
    assert!(e.send(big(5, 10)));
    std::thread::sleep(Duration::from_millis((full * 0.25) as u64));
    let td = Instant::now();
    e.client().supersede_regions(11);
    let evs = until(&e, LONG, |ev| matches!(ev, Event::Tile { id: 5, .. } | Event::TileSkipped { id: 5, .. } | Event::Error { .. }));
    let stop = ms(td.elapsed());
    assert!(matches!(evs.last().unwrap().1, Event::TileSkipped { id: 5, .. }), "dropped region: {}", short(&evs.last().unwrap().1));
    assert!(stop < full * 0.5, "the dropped region took {stop:.0} ms to stop");
    // The list survived: nothing was re-read.
    assert_eq!(e.stats().lists_built, 1);
}


/// DS.2 (bug sweep BUG-01): asking for another page stops the page being read, so the new page does
/// not wait for all of the old one. Asking for the same page again keeps the read going.
#[test]
fn heavy_show_another_page_stops_the_old_read() {
    let Some(path) = corpus("heavy.pdf") else { return };
    let _g = serial();
    let e = Engine::start().expect("start");
    open(&e, &path);
    assert!(e.send(Request::ShowPage { page: 0, fit_width: FIT_WIDTH }));
    std::thread::sleep(Duration::from_millis(300));
    // The same page again: no cancel, the read goes on.
    assert!(e.send(Request::ShowPage { page: 0, fit_width: FIT_WIDTH }));
    std::thread::sleep(Duration::from_millis(200));
    // Another page (out of range for this one-page file, so it ends in an error): the read must stop.
    let tq = Instant::now();
    assert!(e.send(Request::ShowPage { page: 1, fit_width: FIT_WIDTH }));
    let evs = until(&e, LONG, |ev| matches!(ev, Event::Error { tile: None, .. }));
    let took = ms(tq.elapsed());
    let names: Vec<String> = evs.iter().filter(|(_, ev)| !matches!(ev, Event::Progress { .. })).map(|(_, ev)| short(ev)).collect();
    eprintln!("  heavy.pdf: another page asked for during the read -> answered in {took:.0} ms ({names:?})");
    let cancelled_at = evs.iter().position(|(_, ev)| matches!(ev, Event::Cancelled));
    assert!(cancelled_at.is_some(), "no Cancelled: the old read was not stopped ({names:?})");
    let fit_before = evs[..cancelled_at.unwrap()].iter().any(|(_, ev)| matches!(ev, Event::Fit { .. }));
    assert!(!fit_before, "page 1 of the old read finished first: {names:?}");
    assert!(took < 1500.0, "the new page waited {took:.0} ms");
    // Opening resets it: the first page shown after an open does not cancel anything.
    open(&e, &path);
    assert!(e.send(Request::ShowPage { page: 0, fit_width: 400 }));
    let evs = until(&e, LONG, |ev| matches!(ev, Event::Fit { .. }));
    assert!(!evs.iter().any(|(_, ev)| matches!(ev, Event::Cancelled)), "a cancel after open");
}

/// `LowRes` and `Fit` carry the page size in points, equal to `Doc::page_bounds`: portrait and landscape
/// pages of the tiny PDF, through the display list and (with the list guard forced to trip) direct mode.
#[test]
fn tiny_images_carry_page_size_in_points() {
    let _g = serial();
    let path = tiny_pdf();
    let doc = greenline_engine::Doc::open(&path).unwrap();
    let check = |e: &Engine, page: i32, what: &str| {
        let b = doc.page_bounds(page).unwrap();
        assert!(e.send(Request::ShowPage { page, fit_width: FIT_WIDTH }));
        let evs = until(e, LONG, |ev| matches!(ev, Event::Fit { .. } | Event::Error { .. }));
        let mut seen = 0;
        for (_, ev) in &evs {
            match ev {
                Event::LowRes { page: p, pt_w, pt_h, .. } | Event::Fit { page: p, pt_w, pt_h, .. } => {
                    assert_eq!(*p, page);
                    assert_eq!((*pt_w, *pt_h), (b.width(), b.height()), "{what} page {page}");
                    seen += 1;
                }
                Event::Error { .. } => panic!("{what} page {page}: {}", short(ev)),
                _ => {}
            }
        }
        assert_eq!(seen, 2, "{what} page {page}: LowRes and Fit");
        (b.width(), b.height())
    };

    let e = Engine::start().expect("start");
    assert_eq!(open(&e, &path), 3);
    assert_eq!(check(&e, 0, "list"), (612.0, 792.0));
    assert_eq!(check(&e, 1, "list"), (842.0, 595.0));
    drop(e);

    // Direct mode: a 1 MB soft limit is always exceeded, so the engine drops the list between steps and
    // puts the page in direct mode (the Fit of the first show at the latest, every image of the second).
    let limits = greenline_engine::guard::GuardConfig { list_guard_mb: 0.0, soft_limit_mb: 1.0 };
    let e = Engine::start_with(limits).expect("start");
    assert_eq!(open(&e, &path), 3);
    assert_eq!(check(&e, 1, "direct"), (842.0, 595.0));
    assert_eq!(check(&e, 1, "direct"), (842.0, 595.0));
    assert!(e.stats().direct_renders >= 3, "direct mode was used: {:?}", e.stats());
}

/// DS.5 / bug sweep BUG-08: a region origin so large that `x0 + w` overflows is refused, not drawn as a
/// 64 MB white image (or a panic on the engine thread in a debug build).
#[test]
fn tiny_region_origin_overflow_is_refused() {
    let _g = serial();
    let e = Engine::start().expect("start");
    open(&e, &tiny_pdf());
    assert!(e.send(Request::Region { id: 1, page: 0, zoom: 1.0, x0: i32::MAX - 1000, y0: 0, w: 4096, h: 16, gen: 1 }));
    let evs = until(&e, LONG, |ev| matches!(ev, Event::Tile { .. } | Event::TileSkipped { .. } | Event::Error { .. }));
    match &evs.last().unwrap().1 {
        Event::Error { tile: Some(1), .. } => {}
        other => panic!("expected an Error for the overflowing origin, got {}", short(other)),
    }
    // The engine still works.
    assert!(e.send(Request::Region { id: 2, page: 0, zoom: 1.0, x0: 0, y0: 0, w: 64, h: 64, gen: 2 }));
    let evs = until(&e, LONG, |ev| matches!(ev, Event::Tile { .. } | Event::TileSkipped { .. } | Event::Error { .. }));
    assert!(matches!(evs.last().unwrap().1, Event::Tile { id: 2, .. }), "{}", short(&evs.last().unwrap().1));
}

/// DS.5 / bug sweep BUG-11: the engine draws at anti-aliasing level 4 (the approved setting), not
/// MuPDF's default 8: its region equals a local render at AA 4 and differs from one at AA 8.
#[test]
fn lines_engine_draws_at_aa_4() {
    let Some(path) = corpus("lines.pdf") else { return };
    let _g = serial();
    let e = Engine::start().expect("start");
    open(&e, &path);
    assert!(e.send(Request::Region { id: 1, page: 0, zoom: 2.0, x0: 200, y0: 200, w: 512, h: 512, gen: 1 }));
    let evs = until(&e, LONG, |ev| matches!(ev, Event::Tile { .. } | Event::TileSkipped { .. } | Event::Error { .. }));
    let Event::Tile { rgba: px, w, h, .. } = &evs.last().unwrap().1 else { panic!("{}", short(&evs.last().unwrap().1)) };
    let engine = rgba(px, *w, *h);
    let doc = greenline_engine::Doc::open(&path).unwrap();
    let list = doc.build_display_list(0).unwrap();
    let local = |aa: i32| {
        greenline_engine::set_aa_level(aa);
        let r = list.list.render_region_cookie(2.0, 200, 200, 512, 512, None).unwrap();
        r.image
    };
    let at4 = local(4);
    let at8 = local(8);
    greenline_engine::set_aa_level(8);
    let (d4, _) = common::diff(&engine, &at4, 0);
    let (d8, _) = common::diff(&engine, &at8, 0);
    eprintln!("  lines.pdf: engine region differs from AA 4 in {d4} px, from AA 8 in {d8} px");
    assert_eq!(d4, 0, "the engine does not draw at AA 4");
    assert!(d8 > 0, "AA 4 and AA 8 look the same here; the test cannot tell them apart");
}
