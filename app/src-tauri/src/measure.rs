//! The measurement screen's Rust side (task 2b.7): the tester measures the Part 2 pass marks on the phone
//! and exports the numbers, without Termux.
//!
//! The page (`ui/measure.js`) runs the scripted steps that need the WebView (painting the first view,
//! fetching and painting 400 % tiles, the refill, the spread pages, the walk); this module owns
//! everything else:
//! - `measure_start`: the SAF pick and engine open (`open::pick_and_open`, the viewer's path), then the
//!   run's `run_start` line (device numbers) and its `open` step. The provider authority is reduced to
//!   the fixed [`Provider`] enum here and dropped; it never reaches the page or the file.
//! - `measure_plan`: the tile positions, refill tiles and spread pages (`results::plan`), so the page
//!   executes a plan the engine crate computes and tests.
//! - `measure_step`: one finished step from the page, as a JSON object of numbers and fixed names. It is
//!   accepted only through `results::ui_step` (strict typed parse: no text, no unknown keys), then the
//!   main-process VmRSS / VmHWM, the direct-mode flag and reason and the engine counters are added, and
//!   the line is appended and fsynced at once.
//! - `measure_end`, `measure_state` (labels used, next label, pass-mark summary, kills found at launch)
//!   and `measure_export` (re-serialised records -> `ACTION_CREATE_DOCUMENT` via the glfile plugin).
//! - [`init`] at launch: a run with no end in the file gets a `probable_kill` line.
//!
//! The results file is `<app data dir>/measure/results.jsonl` (app-private). Confidentiality: every value
//! written or returned is a number, a bool or a fixed enum name; nothing here logs.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use greenline_engine::results::{
    self, Failure, Kind, Label, Provider, Record, ResultsFile, Step, MEASURE_FIT_WIDTH,
};
use serde::Serialize;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_glfile::GlFileExt;

use crate::host::Host;
use crate::open::pick_and_open;

/// Measurement state, kept in Tauri state.
pub struct Measure {
    file: ResultsFile,
    run: Mutex<Option<Run>>,
    /// Labels of the runs found unfinished at this launch (recorded as `probable_kill`).
    recovered: Vec<Label>,
    /// The launch-time recovery could not read or write the results file (BUG-24).
    recover_failed: bool,
}

struct Run {
    id: u64,
    label: Label,
    /// Next line number.
    seq: u32,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// At launch: find the results file and record any run the last session left unfinished.
pub fn init(app: &AppHandle) -> std::io::Result<Measure> {
    let dir = app.path().app_data_dir().map_err(|_| std::io::Error::other("no app data dir"))?.join("measure");
    std::fs::create_dir_all(&dir)?;
    let file = ResultsFile::new(dir.join("results.jsonl"));
    let (recovered, recover_failed) = match file.recover() {
        Ok(k) => (k.iter().map(|r| r.label).collect(), false),
        Err(_) => (Vec::new(), true),
    };
    Ok(Measure { file, run: Mutex::new(None), recovered, recover_failed })
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl Measure {
    /// Append `r` as the next line of the current run (with its `seq`).
    fn put(&self, run: &mut Run, mut r: Record) -> Result<u32, &'static str> {
        r.seq = Some(run.seq);
        self.file.append(&r).map_err(|_| E_WRITE)?;
        run.seq += 1;
        Ok(run.seq - 1)
    }

    /// End the current run, if any.
    fn end_run(&self, cancelled: bool) -> Result<bool, &'static str> {
        let mut g = lock(&self.run);
        let Some(mut run) = g.take() else { return Ok(false) };
        let r = Record { cancelled: Some(cancelled), ..Record::new(run.id, run.label, Kind::RunEnd) }.with_memory();
        self.put(&mut run, r)?;
        Ok(true)
    }
}

const E_NO_RUN: &str = "E_NO_RUN";
const E_WRITE: &str = "E_WRITE";
const E_BAD_STEP: &str = "E_BAD_STEP";
const E_BAD_LABEL: &str = "E_BAD_LABEL";
const E_READ: &str = "E_READ";

/// Memory and engine fields every step line carries.
fn with_engine_state(mut r: Record, host: &Host, page: Option<i32>) -> Record {
    let direct = page.and_then(|p| host.direct_mode(p));
    let s = host.stats();
    r.direct = Some(direct.is_some());
    r.direct_reason = direct;
    r.direct_renders = Some(s.direct_renders);
    r.mem_releases = Some(s.memory_releases);
    r.reads_abandoned = Some(s.reads_abandoned);
    r.with_memory()
}

#[derive(Serialize)]
pub struct StartInfo {
    run: u64,
    doc: u64,
    pages: i32,
    /// From the fd reaching Rust to this answer leaving it (the page adds its own time to the paint).
    /// Page 0 has already been requested (ShowPage at `fit_width`).
    since_fd_ms: f64,
    fit_width: u32,
}

#[derive(Serialize)]
pub struct StartFailure {
    /// Fixed code (`E_PICK_CANCELLED`, `E_NOT_SEEKABLE`, ...).
    code: &'static str,
    /// Whether a run (with the failed open step) was recorded.
    recorded: bool,
}

/// Pick a file and open it for run `label` (0 = f00 .. 11 = f11): records `run_start` and the `open`
/// step. A cancelled pick records nothing.
#[tauri::command]
pub async fn measure_start(
    app: AppHandle,
    host: State<'_, Arc<Host>>,
    measure: State<'_, Measure>,
    label: u32,
) -> Result<StartInfo, StartFailure> {
    let fail = |code: &'static str, recorded: bool| StartFailure { code, recorded };
    let label = *Label::ALL.get(label as usize).ok_or(fail(E_BAD_LABEL, false))?;
    // A run the page abandoned (reload, back) ends here as cancelled.
    measure.end_run(true).map_err(|c| fail(c, false))?;

    let opened = pick_and_open(&app, &host).await;
    match &opened {
        Err(f) if f.code == "E_PICK_CANCELLED" => return Err(fail(f.code, false)),
        // First view starts at once (before the lines below are written and fsynced); the page picks up
        // the `gl-image` events of this document.
        Ok(_) => {
            host.show_page(0, MEASURE_FIT_WIDTH);
        }
        Err(_) => {}
    }
    let id = now_ms();
    let mut run = Run { id, label, seq: 0 };
    let device = results::device();
    let start = Record { app_ver: results::version_code(env!("CARGO_PKG_VERSION")), ..Record::new(id, label, Kind::RunStart) }
        .with_device(device)
        .with_memory();
    // BUG-07: this run's peak is its own, not the process's lifetime peak.
    greenline_engine::mem::reset_peak();
    if let Err(c) = measure.put(&mut run, start) {
        // BUG-09: nothing was recorded; do not leave the document (and its fd) open.
        if opened.is_ok() {
            host.close();
        }
        return Err(fail(c, false));
    }

    let mut open = Record::step(id, label, Step::Open);
    match &opened {
        Ok(o) => {
            open.pages = Some(o.info.pages);
            open.size_mb = Some(o.info.size_mb);
            open.seekable = Some(o.info.seekable);
            open.fd_kind = Some(fd_kind(o.info.kind));
            open.provider = Some(Provider::from_authority(&o.info.authority));
            open.pick_to_fd_ms = Some(o.info.pick_to_fd_ms);
            open.open_ms = Some(o.info.open_ms);
        }
        Err(f) => {
            open.failure = Some(Failure::from_code(f.code));
            open.size_mb = f.size_mb;
            open.seekable = f.seekable;
            open.fd_kind = f.kind.map(fd_kind);
            open.provider = f.authority.as_deref().map(Provider::from_authority);
            open.pick_to_fd_ms = f.pick_to_fd_ms;
        }
    }
    let open = with_engine_state(open, &host, Some(0));
    if let Err(c) = measure.put(&mut run, open) {
        // BUG-09: close the document and end the run, so the next launch does not record a kill.
        if opened.is_ok() {
            host.close();
        }
        let end = Record { cancelled: Some(false), ..Record::new(id, label, Kind::RunEnd) }.with_memory();
        let _ = measure.put(&mut run, end);
        return Err(fail(c, true));
    }

    match opened {
        Ok(o) => {
            *lock(&measure.run) = Some(run);
            Ok(StartInfo {
                run: id,
                doc: o.info.doc,
                pages: o.info.pages,
                since_fd_ms: o.fd_at.elapsed().as_secs_f64() * 1000.0,
                fit_width: MEASURE_FIT_WIDTH,
            })
        }
        Err(f) => {
            let end = Record { cancelled: Some(false), ..Record::new(id, label, Kind::RunEnd) }.with_memory();
            let _ = measure.put(&mut run, end);
            Err(fail(f.code, true))
        }
    }
}

fn fd_kind(k: &str) -> greenline_engine::open::FdKind {
    use greenline_engine::open::FdKind;
    match k {
        "regular" => FdKind::Regular,
        "pipe" => FdKind::Pipe,
        "socket" => FdKind::Socket,
        _ => FdKind::Other,
    }
}

#[derive(Serialize)]
pub struct PlanInfo {
    tiles: Vec<(i32, i32)>,
    view_x0: i32,
    view_y0: i32,
    refill: Vec<(i32, i32)>,
    pages: Vec<i32>,
    zoom: f32,
}

/// The scripted run's plan for this file and viewport (pixels).
#[tauri::command]
pub fn measure_plan(pages: i32, pt_w: f32, pt_h: f32, view_w: u32, view_h: u32) -> PlanInfo {
    let p = results::plan(pages, pt_w, pt_h, view_w, view_h);
    PlanInfo {
        tiles: p.tiles,
        view_x0: p.view_x0,
        view_y0: p.view_y0,
        refill: p.refill,
        pages: p.pages,
        zoom: greenline_engine::TILE_ZOOM,
    }
}

#[derive(Serialize)]
pub struct StepInfo {
    seq: u32,
    rss_mb: f64,
    hwm_mb: f64,
    direct: bool,
    direct_reason: Option<&'static str>,
}

/// Record one finished step: `step` is a JSON object of numbers and fixed names (see
/// `results::ui_step`). Returns the memory and direct-mode numbers that went into the line.
#[tauri::command]
pub fn measure_step(host: State<'_, Arc<Host>>, measure: State<'_, Measure>, step: String) -> Result<StepInfo, &'static str> {
    let mut g = lock(&measure.run);
    let run = g.as_mut().ok_or(E_NO_RUN)?;
    let r = results::ui_step(&step, run.id, run.label).ok_or(E_BAD_STEP)?;
    drop(step);
    let page = r.page;
    let r = with_engine_state(r, &host, page);
    let info = StepInfo {
        seq: run.seq,
        rss_mb: r.rss_mb.unwrap_or(0.0),
        hwm_mb: r.hwm_mb.unwrap_or(0.0),
        direct: r.direct == Some(true),
        direct_reason: r.direct_reason.map(results::reason_name),
    };
    measure.put(run, r)?;
    Ok(info)
}

/// End the current run (`cancelled` if the user stopped it). False if no run was going.
#[tauri::command]
pub fn measure_end(measure: State<'_, Measure>, cancelled: bool) -> Result<bool, &'static str> {
    measure.end_run(cancelled)
}

#[derive(Serialize)]
pub struct StateInfo {
    /// Labels that have at least one run, in order.
    used: Vec<&'static str>,
    next_label: u32,
    /// Labels whose run was found unfinished at this launch (recorded as `probable_kill`).
    recovered: Vec<&'static str>,
    /// "E_RECOVER_FAILED" when the launch-time recovery failed (BUG-24), else empty.
    recover_error: &'static str,
    records: usize,
    running: bool,
    /// The pass-mark summary: one JSON object per label (`results::Summary`: numbers, bools and fixed
    /// names only), as a JSON array.
    summary: String,
    marks: Marks,
}

#[derive(Serialize)]
pub struct Marks {
    first_view_ms: f64,
    tile_ms: f64,
    refill_ms: f64,
    page_ms: f64,
    peak_mb: f64,
    first_view_labels: Vec<&'static str>,
}

#[tauri::command]
pub fn measure_state(measure: State<'_, Measure>) -> Result<StateInfo, &'static str> {
    let records = measure.file.read().map_err(|_| E_READ)?;
    let used = Label::ALL
        .iter()
        .filter(|l| records.iter().any(|r| r.label == **l && r.event == Kind::RunStart))
        .map(|l| l.name())
        .collect();
    let next = results::next_label(&records);
    let summary = results::summarise(&records).iter().map(|s| s.to_json()).collect::<Vec<_>>().join(",");
    Ok(StateInfo {
        used,
        next_label: Label::ALL.iter().position(|l| *l == next).unwrap_or(0) as u32,
        recovered: measure.recovered.iter().map(|l| l.name()).collect(),
        recover_error: if measure.recover_failed { "E_RECOVER_FAILED" } else { "" },
        records: records.len(),
        running: lock(&measure.run).is_some(),
        summary: format!("[{summary}]"),
        marks: Marks {
            first_view_ms: results::FIRST_VIEW_MAX_MS,
            tile_ms: results::TILE_MAX_MS,
            refill_ms: results::REFILL_MAX_MS,
            page_ms: results::PAGE_MAX_MS,
            peak_mb: results::PEAK_MAX_MB,
            first_view_labels: results::FIRST_VIEW_LABELS.iter().map(|l| l.name()).collect(),
        },
    })
}

#[derive(Serialize)]
pub struct ExportInfo {
    lines: usize,
    bytes: u64,
}

/// Export every valid record (re-serialised, so only typed values) through Android's "save as"
/// screen. The suggested name is `greenline-results-YYYYMMDD-HHMM.jsonl` from the page's local time.
#[tauri::command]
pub async fn measure_export(
    app: AppHandle,
    measure: State<'_, Measure>,
    year: u32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
) -> Result<ExportInfo, &'static str> {
    let text = measure.file.export().map_err(|_| E_READ)?;
    let lines = text.lines().count();
    let name = format!(
        "greenline-results-{:04}{:02}{:02}-{:02}{:02}.jsonl",
        year.min(9999),
        month.min(99),
        day.min(99),
        hour.min(99),
        minute.min(99)
    );
    let saved = app.glfile().save_text(&name, "application/octet-stream", &text).await.map_err(|e| e.code())?;
    Ok(ExportInfo { lines, bytes: saved.bytes })
}
