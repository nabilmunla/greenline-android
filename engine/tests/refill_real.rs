//! 2c.2b: where the time goes on the REAL drawings (phone test: sharpening takes well over half a minute on
//! the phone). Reads `GL_REAL_DIR` (a private local folder outside the repo with f00.pdf .. f11.pdf,
//! named by size rank only) and prints numbers only. Never run in CI; never commits anything.
//!
//! Per file, page 0: list build, the fit image from the list, then a phone viewport (1080 x 2400 device
//! px) at the sheet centre as ONE region at levels 2, 4 and 16 px/pt, the same region drawn directly
//! from the page (no list), and the same region at a corner (to see whether culling helps).
//! Run: GL_REAL_DIR=<folder of test PDFs> cargo test --release -p greenline-engine --test refill_real -- --ignored --nocapture

mod common;

use std::time::Instant;

use common::serial;
use greenline_engine::Doc;

const VIEW_W: f32 = 1080.0;
const VIEW_H: f32 = 2400.0;

#[test]
#[ignore = "real-file benchmark: needs GL_REAL_DIR"]
fn refill_real_files() {
    let _g = serial();
    let Ok(dir) = std::env::var("GL_REAL_DIR") else {
        eprintln!("SKIP: GL_REAL_DIR not set");
        return;
    };
    let only = std::env::var("GL_REAL_ONLY").ok();
    for i in 0..12 {
        let label = format!("f{i:02}");
        if only.as_deref().is_some_and(|o| !o.split(',').any(|x| x == label)) {
            continue;
        }
        let path = std::path::Path::new(&dir).join(format!("{label}.pdf"));
        if !path.is_file() {
            continue;
        }
        let t = Instant::now();
        let doc = match Doc::open(&path) {
            Ok(d) => d,
            Err(_) => {
                eprintln!("{label}: open failed");
                continue;
            }
        };
        let open_ms = t.elapsed().as_secs_f64() * 1000.0;
        greenline_engine::mem::trim();
        let rss0 = greenline_engine::mem::rss_mb();
        let built = match doc.build_display_list(0) {
            Ok(b) => b,
            Err(_) => {
                eprintln!("{label}: list failed");
                continue;
            }
        };
        let b = built.list.bounds();
        let rss_list = greenline_engine::mem::rss_mb();
        eprintln!("{label}: RSS {rss0:.0} MB before the list, {rss_list:.0} MB with it (+{:.0} MB)", rss_list - rss0);
        let fit = built.list.render_fit(1600).map(|r| r.timings.total_ms()).unwrap_or(-1.0);
        eprintln!(
            "{label}: {} pages, page0 {:.0}x{:.0} pt, open {open_ms:.0} ms, list {:.0} ms, fit(1600) {fit:.0} ms",
            doc.page_count(),
            b.width(),
            b.height(),
            built.timings.load_ms + built.timings.list_ms
        );
        for level in [2.0f32, 4.0, 16.0] {
            let (pw, ph) = (b.width() * level, b.height() * level);
            let w = VIEW_W.min(pw) as u32;
            let h = VIEW_H.min(ph) as u32;
            let cx = ((pw - w as f32) / 2.0).max(0.0) as i32;
            let cy = ((ph - h as f32) / 2.0).max(0.0) as i32;
            let centre = built.list.render_region_cookie(level, cx, cy, w, h, None).map(|r| r.timings.total_ms()).unwrap_or(-1.0);
            let corner = built.list.render_region_cookie(level, 0, 0, w, h, None).map(|r| r.timings.total_ms()).unwrap_or(-1.0);
            eprintln!("    L{level:<2} {w}x{h}: list centre {centre:>7.0} ms, list corner {corner:>7.0} ms");
        }
    }
}
