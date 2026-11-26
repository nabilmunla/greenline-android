//! Measurement screen acceptance on the host (task 2b.7).
//!
//! A PDF under a planted name (`/tmp/PLANTED-SECRET-NAME.pdf`, a copy of the tiny PDF) is opened through
//! an fd, exactly as the app does it (probe, then `/proc/self/fd/N`), and the scripted run goes through
//! the engine worker: open, first view, the four 400 % tiles of the plan, the refill, the spread pages
//! and the whole-file walk. Every step goes through `results.rs` as on the phone (the page's steps
//! through `results::ui_step`, the strict parser the app uses). Then:
//! - the export does not contain "PLANTED" (nor the path, nor ".pdf"), and every value in it is a
//!   number, a bool or a fixed enum name;
//! - `tools/leakcheck.sh` passes on it with a temp deny-list holding PLANTED-SECRET-NAME, and fails on a
//!   control file that does contain the name (so the deny-list is really used);
//! - the measure page's JS, HTML and the app's Rust never call a console or log function.

mod common;

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::{serial, tiny_pdf};
use greenline_engine::api::{Event, Request};
use greenline_engine::open::probe_fd;
use greenline_engine::results::{self, parse_flat, Kind, Label, Provider, Raw, Record, ResultsFile, Step};
use greenline_engine::worker::Engine;
use greenline_engine::TILE_ZOOM;

const PLANTED: &str = "PLANTED-SECRET-NAME";
const LONG: Duration = Duration::from_secs(120);

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// The run as the app keeps it: id, label, next line number, the file.
struct Run<'a> {
    file: &'a ResultsFile,
    id: u64,
    label: Label,
    seq: u32,
    direct_page: Option<i32>,
}

impl Run<'_> {
    fn put(&mut self, mut r: Record) {
        r.seq = Some(self.seq);
        self.file.append(&r).expect("append");
        self.seq += 1;
    }

    /// A step as the measure page reports it (JSON object of numbers and fixed names), through the same
    /// strict parser as the app's `measure_step`, plus what the app adds.
    fn ui(&mut self, json: String) {
        let mut r = results::ui_step(&json, self.id, self.label).unwrap_or_else(|| panic!("refused: {json}"));
        r.direct = Some(r.page.is_some() && r.page == self.direct_page);
        self.put(r.with_memory());
    }
}

/// The next event matching `want`, noting direct mode on the way; panics on an engine error.
fn wait(e: &Engine, run: &mut Run, want: impl Fn(&Event) -> bool) -> Event {
    let deadline = Instant::now() + LONG;
    loop {
        let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
        let ev = e.events().recv_timeout(left).expect("engine event");
        if let Event::DirectMode { page, .. } = ev {
            run.direct_page = Some(page);
        }
        if want(&ev) {
            return ev;
        }
        if let Event::Error { code, tile } = ev {
            panic!("engine error {code:?} (tile {tile:?})");
        }
    }
}

/// The scripted run of the measurement screen, through the engine worker, on `pdf`.
fn scripted_run(file: &ResultsFile, pdf: &Path, label: Label) {
    let id = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
    let mut run = Run { file, id, label, seq: 0, direct_page: None };
    run.put(Record { app_ver: results::version_code("0.1.0"), ..Record::new(id, label, Kind::RunStart) }
        .with_device(results::device())
        .with_memory());

    // 1. open through an fd (the SAF path minus the picker)
    let t_pick = Instant::now();
    let fd: OwnedFd = std::fs::File::open(pdf).expect("open planted file").into();
    let pick_to_fd_ms = ms(t_pick);
    let fd_at = Instant::now();
    let probe = probe_fd(fd.as_raw_fd());
    assert!(probe.seekable);
    let e = Engine::start().expect("engine");
    assert!(e.send(Request::Open { path: format!("/proc/self/fd/{}", fd.as_raw_fd()) }));
    let (pages, open_ms) = match wait(&e, &mut run, |ev| matches!(ev, Event::Opened { .. })) {
        Event::Opened { pages, open_ms } => (pages, open_ms),
        _ => unreachable!(),
    };
    // On the phone the authority comes from the picker; here there is none.
    run.put(
        Record {
            pages: Some(pages),
            size_mb: Some(probe.size_mb),
            seekable: Some(probe.seekable),
            fd_kind: Some(probe.kind),
            provider: Some(Provider::from_authority("")),
            pick_to_fd_ms: Some(pick_to_fd_ms),
            open_ms: Some(open_ms),
            direct: Some(false),
            ..Record::step(id, label, Step::Open)
        }
        .with_memory(),
    );

    // 2. first view (no WebView here: "painted" is the engine's image arriving)
    let fit_width = results::MEASURE_FIT_WIDTH;
    assert!(e.send(Request::ShowPage { page: 0, fit_width }));
    let (list_ms, engine_lowres_ms) = match wait(&e, &mut run, |ev| matches!(ev, Event::LowRes { .. })) {
        Event::LowRes { list_ms, total_ms, .. } => (list_ms, total_ms),
        _ => unreachable!(),
    };
    let lowres_ms = ms(fd_at);
    let (engine_fit_ms, pt_w, pt_h) = match wait(&e, &mut run, |ev| matches!(ev, Event::Fit { .. })) {
        Event::Fit { total_ms, pt_w, pt_h, .. } => (total_ms, pt_w, pt_h),
        _ => unreachable!(),
    };
    let fit_ms = ms(fd_at);
    run.ui(format!(
        r#"{{"step":"first_view","page":0,"lowres_ms":{lowres_ms},"fit_ms":{fit_ms},"list_ms":{list_ms},"engine_lowres_ms":{engine_lowres_ms},"engine_fit_ms":{engine_fit_ms},"pt_w":{pt_w},"pt_h":{pt_h}}}"#
    ));

    // 3. the four 400 % tiles of the plan
    let plan = results::plan(pages, pt_w, pt_h, 1080, 2200);
    let mut gen = 0u64;
    for (i, &(x0, y0)) in plan.tiles.iter().enumerate() {
        gen += 1;
        let t = Instant::now();
        assert!(e.send(Request::Tile { id: gen, page: 0, zoom: TILE_ZOOM, x0, y0, gen }));
        let render_ms = match wait(&e, &mut run, |ev| matches!(ev, Event::Tile { .. } | Event::TileSkipped { .. })) {
            Event::Tile { render_ms, .. } => render_ms,
            other => panic!("tile skipped: {other:?}"),
        };
        let total = ms(t);
        run.ui(format!(
            r#"{{"step":"tile","page":0,"tile":{},"x0":{x0},"y0":{y0},"render_ms":{render_ms},"wait_ms":{total},"handoff_ms":0,"total_ms":{total}}}"#,
            i + 1
        ));
    }

    // 4. refill: every grid tile of the viewport, one generation
    gen += 1;
    let t = Instant::now();
    for (k, &(x0, y0)) in plan.refill.iter().enumerate() {
        assert!(e.send(Request::Tile { id: 1000 + k as u64, page: 0, zoom: TILE_ZOOM, x0, y0, gen }));
    }
    let mut render_sum = 0.0;
    for _ in &plan.refill {
        match wait(&e, &mut run, |ev| matches!(ev, Event::Tile { .. } | Event::TileSkipped { .. })) {
            Event::Tile { render_ms, .. } => render_sum += render_ms,
            other => panic!("refill tile skipped: {other:?}"),
        }
    }
    run.ui(format!(
        r#"{{"step":"refill","page":0,"tiles":{},"ms":{},"render_ms":{render_sum}}}"#,
        plan.refill.len(),
        ms(t)
    ));

    // 5. spread pages, 6. whole-file walk
    let show = |e: &Engine, run: &mut Run, p: i32| {
        let t = Instant::now();
        assert!(e.send(Request::ShowPage { page: p, fit_width }));
        match wait(e, run, |ev| matches!(ev, Event::Fit { page, .. } if *page == p)) {
            Event::Fit { total_ms, list_ms, .. } => (ms(t), total_ms, list_ms),
            _ => unreachable!(),
        }
    };
    assert_eq!(plan.pages, vec![1, 2, 0], "tiny PDF: all 3 pages, page 0 last");
    for &p in &plan.pages {
        let (ui_ms, engine_ms, list_ms) = show(&e, &mut run, p);
        run.ui(format!(r#"{{"step":"page","page":{p},"ms":{ui_ms},"engine_ms":{engine_ms},"list_ms":{list_ms}}}"#));
    }
    let t = Instant::now();
    let mut max_ms: f64 = 0.0;
    for p in 0..pages {
        max_ms = max_ms.max(show(&e, &mut run, p).1);
    }
    run.ui(format!(r#"{{"step":"walk","walked":{pages},"ms":{},"max_ms":{max_ms},"cancelled":false}}"#, ms(t)));

    run.put(Record { cancelled: Some(false), ..Record::new(id, label, Kind::RunEnd) }.with_memory());
    assert!(e.send(Request::Close));
    drop(e); // stops the engine thread (and MuPDF's handle on the file) before the fd closes
    drop(fd);
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn leakcheck(target: &Path, denylist: &Path) -> (bool, String) {
    let out = Command::new("bash")
        .arg(repo().join("tools/leakcheck.sh"))
        .arg(target)
        .env("GL_DENYLIST", denylist)
        .output()
        .expect("run tools/leakcheck.sh");
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    (out.status.success(), text)
}

#[test]
fn planted_name_never_reaches_the_export() {
    let _g = serial();
    let planted = PathBuf::from(format!("/tmp/{PLANTED}.pdf"));
    std::fs::copy(tiny_pdf(), &planted).expect("copy the tiny PDF to the planted name");

    let work = Path::new(env!("CARGO_TARGET_TMPDIR")).join("measure-planted");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(work.join("app")).unwrap();
    let file = ResultsFile::new(work.join("app/results.jsonl"));
    scripted_run(&file, &planted, Label::F04);
    std::fs::remove_file(&planted).ok();

    let export = file.export().unwrap();
    let lines: Vec<&str> = export.lines().collect();
    eprintln!("  export: {} lines, {} bytes", lines.len(), export.len());
    for l in &lines {
        eprintln!("    {l}");
    }
    // run_start, open, first_view, 4 tiles, refill, 3 pages, walk, run_end
    assert_eq!(lines.len(), 13);
    let lower = export.to_lowercase();
    for needle in ["planted", "secret", "/tmp", ".pdf", "/proc", "tiny"] {
        assert!(!lower.contains(needle), "export contains {needle:?}");
    }
    let names = results::enum_names();
    for l in &lines {
        for (k, v) in parse_flat(l).expect("flat JSON") {
            match v {
                Raw::Num(n) => assert!(n.is_finite(), "{k}"),
                Raw::Bool(_) => {}
                Raw::Str(s) => assert!(names.contains(&s), "{k}: {s:?}"),
            }
        }
    }
    let records = file.read().unwrap();
    assert!(results::unfinished(&records).is_empty(), "the run ended");
    let s = &results::summarise(&records)[0];
    assert_eq!(s.label, Label::F04);
    assert!(s.complete && !s.killed);
    assert!(s.first_view_ms.is_some() && s.tile_max_ms.is_some() && s.refill_ms.is_some() && s.page_median_ms.is_some());
    eprintln!("  summary: {}", s.to_json());

    // The exported file (as the tester saves it) passes leakcheck with the planted name on the deny-list...
    let out = work.join("export");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("greenline-results-20261125-1200.jsonl"), &export).unwrap();
    let deny = work.join("denylist");
    std::fs::write(&deny, format!("{PLANTED}\n")).unwrap();
    let (ok, text) = leakcheck(&out, &deny);
    eprintln!("  leakcheck export: {}", text.trim());
    assert!(ok && text.contains("LEAKCHECK PASS"), "{text}");

    // ...and the same deny-list catches the name in a control file (the check is real).
    let control = work.join("control");
    std::fs::create_dir_all(&control).unwrap();
    std::fs::write(control.join("x.jsonl"), format!("{export}{{\"note\":\"{PLANTED}.pdf\"}}\n")).unwrap();
    let (ok, text) = leakcheck(&control, &deny);
    assert!(!ok && text.contains("LEAKCHECK FAIL"), "{text}");
}

/// The measure page and the app's measure code never log (no console, no println, no Android Log).
#[test]
fn measure_code_never_logs() {
    let r = repo();
    for (path, banned) in [
        ("app/ui/measure.js", &["console.", "alert(", "debugger"][..]),
        ("app/ui/measure.html", &["console.", "alert(", "<script>"][..]),
        ("app/ui/viewer.js", &["console.", "alert("][..]),
        ("app/src-tauri/src/measure.rs", &["println!", "eprintln!", "dbg!", "log::", "tracing::"][..]),
        ("engine/src/results.rs", &["println!", "eprintln!", "dbg!", "log::", "tracing::"][..]),
        (
            "app/plugins/glfile/android/src/main/java/com/greenline/glfile/GlFilePlugin.kt",
            &["Log.", "println", "printStackTrace", "Toast"][..],
        ),
    ] {
        let text = std::fs::read_to_string(r.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"));
        for b in banned {
            // Doc comments may name what is banned; code may not.
            let hit = text.lines().any(|l| {
                let t = l.trim_start();
                !(t.starts_with("//") || t.starts_with("*") || t.starts_with("<!--")) && l.contains(b)
            });
            assert!(!hit, "{path} contains {b:?}");
        }
    }
}
