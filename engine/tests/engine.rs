//! Integration tests for greenline-engine.
//!
//! - `tiny_*`: a 3-page PDF written by the test itself; always runs.
//! - `lines_*` / `set_*`: the synthetic corpus (`corpus/lines.pdf`, `corpus/set.pdf`, git-ignored).
//!   If a file is absent the test prints a SKIP message and passes.
//! Run in release (debug MuPDF on the 1M-line sheet is very slow):
//!   cargo test --release -p greenline-engine -j 8 -- --nocapture

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::*;
use greenline_engine::{mem, spread_pages, Cookie, Doc, Error, FIT_WIDTH, TILE_SIZE, TILE_ZOOM};

/// Pixels allowed to differ (by more than 2 in any channel) between a direct render and a display-list
/// render of the same region. MuPDF replays the same operations, so this should be 0; a little slack
/// keeps the test from failing on a one-pixel rounding difference.
const MAX_DIFF_FRACTION: f64 = 0.001;

fn assert_near(what: &str, a: &greenline_engine::Rgba, b: &greenline_engine::Rgba) {
    let (n, max) = diff(a, b, 2);
    let total = (a.width * a.height) as usize;
    eprintln!("{what}: {n} of {total} pixels differ by more than 2 (max channel delta {max})");
    assert!(
        (n as f64) <= MAX_DIFF_FRACTION * total as f64,
        "{what}: {n} of {total} pixels differ (max delta {max})"
    );
}

// ------------------------------------------------------------------------------------------ tiny

#[test]
fn tiny_open_and_fit() {
    let _g = serial();
    let doc = Doc::open(tiny_pdf()).unwrap();
    assert_eq!(doc.page_count(), 3);
    assert!(doc.open_ms() >= 0.0);

    let r = doc.render_fit(0, FIT_WIDTH).unwrap();
    assert_rgba_shape(&r.image, FIT_WIDTH, None);
    assert!((2070..=2072).contains(&r.image.height), "portrait height {}", r.image.height);
    assert!(non_white(&r.image) > 1000, "something must be drawn");
    assert!(r.timings.render_ms > 0.0 && r.timings.total_ms() >= r.timings.render_ms);

    // landscape page: different aspect ratio
    let l = doc.render_fit(1, FIT_WIDTH).unwrap();
    assert_rgba_shape(&l.image, FIT_WIDTH, None);
    assert!(l.image.height < l.image.width);

    // a different width
    let s = doc.render_fit(0, 400).unwrap();
    assert_rgba_shape(&s.image, 400, None);
}

#[test]
fn tiny_errors() {
    let _g = serial();
    let doc = Doc::open(tiny_pdf()).unwrap();
    assert!(matches!(doc.render_fit(3, FIT_WIDTH), Err(Error::PageOutOfRange { page: 3, count: 3 })));
    assert!(matches!(doc.render_fit(-1, FIT_WIDTH), Err(Error::PageOutOfRange { .. })));
    assert!(matches!(doc.render_fit(0, 0), Err(Error::Invalid(_))));
    assert!(matches!(doc.render_tile(0, 0.0, 0, 0), Err(Error::Invalid(_))));
    assert!(Doc::open("/definitely/not/here.pdf").is_err());
}

#[test]
fn tiny_tile_and_list_match_direct() {
    let _g = serial();
    let doc = Doc::open(tiny_pdf()).unwrap();
    let built = doc.build_display_list(0).unwrap();
    assert!(!built.list.is_empty());
    assert_eq!(built.list.bounds(), doc.page_bounds(0).unwrap());
    assert!(built.timings.list_ms > 0.0);

    // fit
    let d = doc.render_fit(0, FIT_WIDTH).unwrap();
    let l = built.list.render_fit(FIT_WIDTH).unwrap();
    assert_eq!((l.image.width, l.image.height), (d.image.width, d.image.height));
    assert_eq!(l.timings.load_ms, 0.0);
    assert_near("tiny fit", &d.image, &l.image);

    // tiles: grid tile, and a free-origin tile, at 400 % (page is 2448 x 3168 px there)
    for (tx, ty) in [(0, 0), (1, 1), (2, 3)] {
        let d = doc.render_tile(0, TILE_ZOOM, tx, ty).unwrap();
        assert_rgba_shape(&d.image, TILE_SIZE, Some(TILE_SIZE));
        let l = built.list.render_tile(TILE_ZOOM, tx, ty).unwrap();
        assert_rgba_shape(&l.image, TILE_SIZE, Some(TILE_SIZE));
        assert_near(&format!("tiny tile {tx},{ty}"), &d.image, &l.image);
    }
    let b = doc.page_bounds(0).unwrap();
    let (x0, y0) = b.tile_origin_at(TILE_ZOOM, 0.25, 0.75);
    let d = doc.render_tile_at(0, TILE_ZOOM, x0, y0).unwrap();
    let l = built.list.render_tile_at(TILE_ZOOM, x0, y0).unwrap();
    assert!(non_white(&d.image) > 1000, "this tile covers the blue rectangle");
    assert_near("tiny tile at 0.25,0.75", &d.image, &l.image);
}

#[test]
fn tiny_walk() {
    let _g = serial();
    let doc = Doc::open(tiny_pdf()).unwrap();
    let mut seen = Vec::new();
    let s = doc.walk(FIT_WIDTH, |st| {
        seen.push(st.page);
        assert!(st.ms >= 0.0);
        true
    })
    .unwrap();
    assert_eq!(seen, vec![0, 1, 2]);
    assert_eq!((s.pages, s.stopped), (3, false));

    let mut count = 0;
    let s = doc.walk(FIT_WIDTH, |_| {
        count += 1;
        count < 2
    })
    .unwrap();
    assert_eq!((s.pages, s.stopped), (2, true));

    let s = doc.walk_pages(spread_pages(3, 20), 800, |_| true).unwrap();
    assert_eq!(s.pages, 3);
    assert!(doc.walk_pages([5], 800, |_| true).is_err());
}

#[test]
fn tiny_cookie_list_matches_plain_list() {
    let _g = serial();
    let doc = Doc::open(tiny_pdf()).unwrap();
    let plain = doc.build_display_list(0).unwrap();
    let cookie = Cookie::new().unwrap();
    let with = doc.build_display_list_with_cookie(0, &cookie).unwrap();
    eprintln!("cookie after tiny build: progress {} max {} errors {} incomplete {}",
        cookie.progress(), cookie.max_progress(), cookie.errors(), cookie.incomplete());
    assert_eq!(cookie.errors(), 0);
    assert!(!cookie.incomplete());
    assert!(!with.list.is_empty());
    let a = plain.list.render_fit(FIT_WIDTH).unwrap();
    let b = with.list.render_fit(FIT_WIDTH).unwrap();
    assert_near("tiny cookie list vs plain list", &a.image, &b.image);
}

// ------------------------------------------------------------------------------------------ mem

#[test]
fn mem_helpers_are_plausible() {
    let _g = serial();
    let page = mem::page_size();
    assert!(page >= 4096 && page.is_power_of_two(), "page size {page}");
    let r0 = mem::rss_mb();
    let p0 = mem::peak_mb();
    assert!(r0 > 1.0 && r0 < 100_000.0, "rss {r0}");
    assert!(p0 >= r0 - 1.0 && p0 < 100_000.0, "peak {p0} rss {r0}");

    // touch 200 MB: rss must rise by most of it, and the peak must follow
    let mut v = vec![0u8; 200 * 1024 * 1024];
    for i in (0..v.len()).step_by(page.max(4096)) {
        v[i] = 1;
    }
    let r1 = mem::rss_mb();
    let p1 = mem::peak_mb();
    assert!(r1 - r0 > 150.0, "rss rose only {} MB", r1 - r0);
    assert!(p1 >= r1 - 1.0);
    std::hint::black_box(&v);
    drop(v);
    mem::trim(); // must not crash; the 200 MB block was an mmap and is already back
    assert!(mem::peak_mb() >= p1 - 1.0, "the peak never goes down");
}

// ------------------------------------------------------------------------------------------ lines.pdf (1M lines, one A0 sheet)

#[test]
fn lines_pdf_direct_and_list() {
    let Some(path) = corpus("lines.pdf") else { return };
    let _g = serial();
    let doc = Doc::open(&path).unwrap();
    assert_eq!(doc.page_count(), 1);
    let b = doc.page_bounds(0).unwrap();
    assert_eq!((b.width(), b.height()), (3370.0, 2384.0));

    // fit: width exactly 1600, RGBA length w*h*4
    let d = doc.render_fit(0, FIT_WIDTH).unwrap();
    assert_rgba_shape(&d.image, 1600, None);
    assert!(non_white(&d.image) > 100_000, "sheet must be drawn");
    eprintln!("lines direct fit: {}x{} load {:.0} ms render {:.0} ms convert {:.1} ms", d.image.width, d.image.height,
        d.timings.load_ms, d.timings.render_ms, d.timings.convert_ms);

    // tile at 400 % in the middle of the sheet (the glspike position)
    let (x0, y0) = b.tile_origin_at(TILE_ZOOM, 0.5, 0.5);
    let dt = doc.render_tile_at(0, TILE_ZOOM, x0, y0).unwrap();
    assert_rgba_shape(&dt.image, TILE_SIZE, Some(TILE_SIZE));
    assert_eq!(dt.image.data.len(), 1024 * 1024 * 4);
    assert!(non_white(&dt.image) > 10_000);
    eprintln!("lines direct tile: render {:.0} ms convert {:.1} ms", dt.timings.render_ms, dt.timings.convert_ms);

    // display list: build once, then fit and tiles from it
    let rss_before = mem::rss_mb();
    let built = doc.build_display_list(0).unwrap();
    let rss_with_list = mem::rss_mb();
    eprintln!("lines list build: load {:.0} ms list {:.0} ms; rss {:.0} -> {:.0} MB; peak {:.0} MB",
        built.timings.load_ms, built.timings.list_ms, rss_before, rss_with_list, mem::peak_mb());
    let lf = built.list.render_fit(FIT_WIDTH).unwrap();
    assert_rgba_shape(&lf.image, 1600, None);
    eprintln!("lines list fit: render {:.0} ms", lf.timings.render_ms);
    assert_near("lines fit: list vs direct", &d.image, &lf.image);

    let lt = built.list.render_tile_at(TILE_ZOOM, x0, y0).unwrap();
    assert_rgba_shape(&lt.image, TILE_SIZE, Some(TILE_SIZE));
    eprintln!("lines list tile: render {:.0} ms", lt.timings.render_ms);
    assert_near("lines tile: list vs direct", &dt.image, &lt.image);

    // the other three glspike positions, list only (must all draw something and be 1024x1024x4)
    for (fx, fy) in [(0.25f32, 0.25f32), (0.75, 0.6), (0.4, 0.8)] {
        let (x, y) = b.tile_origin_at(TILE_ZOOM, fx, fy);
        let t = built.list.render_tile_at(TILE_ZOOM, x, y).unwrap();
        assert_rgba_shape(&t.image, TILE_SIZE, Some(TILE_SIZE));
        assert!(non_white(&t.image) > 10_000);
    }
    drop(built);
    mem::trim();
}

#[test]
fn lines_pdf_cookie_progress_and_abort() {
    let Some(path) = corpus("lines.pdf") else { return };
    let _g = serial();
    let doc = Doc::open(&path).unwrap();

    // 1. normal cookie build: same picture as the plain list, progress counted
    let cookie = Cookie::new().unwrap();
    let t = Instant::now();
    let full = doc.build_display_list_with_cookie(0, &cookie).unwrap();
    let full_s = t.elapsed();
    eprintln!("cookie build, no abort: {:.2} s; progress {} max {} errors {} incomplete {}", full_s.as_secs_f64(),
        cookie.progress(), cookie.max_progress(), cookie.errors(), cookie.incomplete());
    assert!(cookie.progress() > 0, "the cookie must count progress during the build");
    assert_eq!(cookie.errors(), 0);
    let a = full.list.render_fit(FIT_WIDTH).unwrap();
    assert!(non_white(&a.image) > 100_000);
    let full_progress = cookie.progress();
    let full_nonwhite = non_white(&a.image);
    drop(full);

    // 2. abort from another thread while the build runs: a watcher thread polls the progress counter and
    //    aborts once the build is a sixth of the way through. Cookie is !Send and abort() takes
    //    &mut self, so this goes through a raw pointer; it shows that mupdf 0.8's Cookie works across
    //    threads (plain int fields, no locking). The worker task needs its own wrapper.
    struct Shared(*mut Cookie);
    unsafe impl Send for Shared {}
    let cookie = Cookie::new().unwrap();
    let shared = Shared(&cookie as *const Cookie as *mut Cookie);
    let done = AtomicBool::new(false);
    let threshold = full_progress / 6;
    let t = Instant::now();
    let (res, seen, abort_at) = std::thread::scope(|s| {
        let done = &done;
        let h = s.spawn(move || {
            let shared = shared;
            let limit = Instant::now() + Duration::from_secs(30);
            while !done.load(Ordering::Acquire) && Instant::now() < limit {
                let p = unsafe { (*shared.0).progress() };
                if p >= threshold {
                    let at = Instant::now();
                    unsafe { (*shared.0).abort() };
                    return Some((p, at));
                }
                std::hint::spin_loop();
            }
            None
        });
        let r = doc.build_display_list_with_cookie(0, &cookie);
        done.store(true, Ordering::Release);
        let w = h.join().unwrap();
        (r, w.map(|x| x.0), w.map(|x| x.1))
    });
    let total = t.elapsed();
    let after_abort = abort_at.map(|a| a.elapsed()).unwrap_or_default();
    eprintln!(
        "cookie build, abort at progress >= {threshold}: returned {} after {:.3} s total ({:.3} s after the abort); progress at abort {:?}, final {} of {full_progress}; incomplete {}",
        if res.is_ok() { "Ok (partial list)" } else { "Err" },
        total.as_secs_f64(),
        after_abort.as_secs_f64(),
        seen,
        cookie.progress(),
        cookie.incomplete()
    );
    assert!(seen.is_some(), "the watcher must see progress from another thread while the build runs");
    assert!(cookie.progress() < full_progress, "the abort must stop the build early");
    assert!(after_abort < Duration::from_secs(1), "the build must return within 1 s of the abort");
    if let Ok(partial) = res {
        let p = partial.list.render_fit(FIT_WIDTH).unwrap();
        eprintln!("partial list draws {} non-white pixels, full list {}", non_white(&p.image), full_nonwhite);
        assert!(non_white(&p.image) < full_nonwhite, "a partial list must draw less than the full one");
    }
    mem::trim();
}

// ------------------------------------------------------------------------------------------ set.pdf (2000 pages)

#[test]
fn set_pdf_page_count_and_walk() {
    let Some(path) = corpus("set.pdf") else { return };
    let _g = serial();
    let doc = Doc::open(&path).unwrap();
    assert_eq!(doc.page_count(), 2000);
    eprintln!("set.pdf open: {:.1} ms", doc.open_ms());

    let first = doc.render_fit(0, FIT_WIDTH).unwrap();
    assert_rgba_shape(&first.image, 1600, None);
    let last = doc.render_fit(1999, FIT_WIDTH).unwrap();
    assert_rgba_shape(&last.image, 1600, None);
    assert!(doc.render_fit(2000, FIT_WIDTH).is_err());

    // 20 pages spread over the file, like the glspike browse; memory must not run away
    let rss0 = mem::rss_mb();
    let mut worst_ms: f64 = 0.0;
    let mut pages = Vec::new();
    let s = doc
        .walk_pages(spread_pages(doc.page_count(), 20), FIT_WIDTH, |st| {
            worst_ms = worst_ms.max(st.ms);
            pages.push((st.page, st.ms as u64, mem::rss_mb() as u64));
            true
        })
        .unwrap();
    assert_eq!(s.pages, 20);
    assert_eq!((pages[0].0, pages[19].0), (0, 1999));
    let rss1 = mem::rss_mb();
    eprintln!("set.pdf 20-page browse: {:.0} ms total, worst page {:.0} ms, rss {:.0} -> {:.0} MB, peak {:.0} MB",
        s.total_ms, worst_ms, rss0, rss1, mem::peak_mb());
    assert!(rss1 < 1500.0, "rss {rss1} MB after 20 pages");
    assert!(mem::peak_mb() >= rss1 - 1.0);

    // one display list on a page of the set, and a tile from it
    let built = doc.build_display_list(1000).unwrap();
    let t = built.list.render_tile(TILE_ZOOM, 1, 1).unwrap();
    assert_rgba_shape(&t.image, TILE_SIZE, Some(TILE_SIZE));
    let d = doc.render_tile(1000, TILE_ZOOM, 1, 1).unwrap();
    assert_near("set page 1001 tile", &d.image, &t.image);
}

/// DS2.2 (sweep 0.1.11 #2): the fit image of a very tall page is scaled down to stay within
/// FIT_MAX_PX pixels and FIT_MAX_SIDE per side, instead of following the width alone (a 100 x 3000 pt
/// page at 1600 px wide was 1600 x 48,000 = 77 Mpx, about 300 MB of RGBA). Both fit paths agree.
#[test]
fn tall_page_fit_is_bounded() {
    let doc = Doc::open(common::pdf_with_pages("tall.pdf", &[(100, 3000), (3370, 2384)])).unwrap();
    let a = doc.render_fit(0, FIT_WIDTH).unwrap().image;
    let (w, h) = (a.width as u64, a.height as u64);
    eprintln!("  tall page fit: {w} x {h}");
    assert!(w * h <= greenline_engine::FIT_MAX_PX, "{w} x {h} is over FIT_MAX_PX");
    assert!(h <= greenline_engine::FIT_MAX_SIDE as u64 && w <= greenline_engine::FIT_MAX_SIDE as u64, "{w} x {h}");
    assert!(non_white(&a) > 0, "the page is drawn");
    let list = doc.build_display_list(0).unwrap().list.render_fit_cookie(FIT_WIDTH, None).unwrap().image;
    assert_eq!((list.width, list.height), (a.width, a.height), "list and direct fit differ");
    // An ordinary landscape sheet is unchanged: exactly FIT_WIDTH wide.
    assert_eq!(doc.render_fit(1, FIT_WIDTH).unwrap().image.width, FIT_WIDTH);
}
