//! 2c.1 refill reproduction: how long a phone-sized view takes to sharpen at each tile level.
//!
//! For the synthetic dense sheets (`corpus/heavy.pdf`, `corpus/lines.pdf`; SKIP if absent) the list is
//! built once, then the tiles covering a phone viewport (1080 x 2400 device px, the test phone) centred
//! on the sheet are rendered one at a time from the list, as the single engine thread does today, at
//! levels 1, 2, 4, 8 and 16 device px per point. Prints count, median and max per tile and the serial
//! total (= time to fully sharp with one thread). Numbers only.
//! Run: cargo test --release -p greenline-engine -j 8 --test refill -- --nocapture --test-threads 1

mod common;

use common::{corpus, serial};
use greenline_engine::{Doc, TILE_SIZE};

const VIEW_W: f32 = 1080.0;
const VIEW_H: f32 = 2400.0;

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

#[test]
#[ignore = "benchmark (about 2 min): cargo test --release -p greenline-engine --test refill -- --ignored --nocapture"]
fn refill_by_level() {
    let _g = serial();
    for name in ["heavy.pdf", "lines.pdf"] {
        let Some(path) = corpus(name) else {
            eprintln!("SKIP: {name} missing");
            continue;
        };
        let doc = Doc::open(&path).unwrap();
        let built = doc.build_display_list(0).unwrap();
        let b = built.list.bounds();
        eprintln!(
            "{name}: page {:.0} x {:.0} pt, list {:.0} ms",
            b.width(),
            b.height(),
            built.timings.load_ms + built.timings.list_ms
        );
        eprintln!("  level | tiles | median ms | max ms | serial total s");
        for level in [1.0f32, 2.0, 4.0, 8.0, 16.0] {
            let (pw, ph) = (b.width() * level, b.height() * level);
            // viewport centred on the sheet, clipped to it
            let x0 = ((pw - VIEW_W) / 2.0).max(0.0);
            let y0 = ((ph - VIEW_H) / 2.0).max(0.0);
            let x1 = (x0 + VIEW_W).min(pw);
            let y1 = (y0 + VIEW_H).min(ph);
            let t = TILE_SIZE as f32;
            let (tx0, tx1) = ((x0 / t) as i32, ((x1 - 1.0) / t) as i32);
            let (ty0, ty1) = ((y0 / t) as i32, ((y1 - 1.0) / t) as i32);
            let mut ms = Vec::new();
            for ty in ty0..=ty1 {
                for tx in tx0..=tx1 {
                    let r = built.list.render_tile(level, tx, ty).unwrap();
                    assert_eq!(r.image.width, TILE_SIZE);
                    ms.push(r.timings.total_ms());
                }
            }
            let total: f64 = ms.iter().sum();
            let max = ms.iter().cloned().fold(0.0, f64::max);
            let n = ms.len();
            // 2c.2: the same viewport as ONE region render (one walk of the list).
            let (rw, rh) = ((x1 - x0).ceil() as u32, (y1 - y0).ceil() as u32);
            let r = built.list.render_region_cookie(level, x0 as i32, y0 as i32, rw, rh, None).unwrap();
            assert_eq!((r.image.width, r.image.height), (rw, rh));
            let region = r.timings.total_ms();
            eprintln!(
                "  {level:>5} | {n:>5} | {:>9.0} | {max:>6.0} | {:>14.2} | one region {rw}x{rh}: {:.2} s ({:.1}x faster)",
                median(&mut ms),
                total / 1000.0,
                region / 1000.0,
                total / region
            );
        }
    }
}
