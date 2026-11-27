//! Measurement results (task 2b.7): the typed record behind every line of the measurement screen's
//! results file, its JSON-lines writer and reader, the probable-kill check, the scripted run's plan
//! (tile positions, refill tiles, spread pages) and the Part 2 pass-mark summary.
//!
//! # Numbers only, by construction
//!
//! A [`Record`] holds numbers, bools and the fixed enums of this module, nothing else. Every field's type
//! must implement [`Field`], a sealed trait implemented only for `u32`, `u64`, `i32`, `f64`, `bool`, the
//! fixed enums ([`Label`], [`Kind`], [`Step`], [`Provider`], [`Failure`], [`Verdict`], `FdKind`,
//! `MemReason`) and `Option` of those. `String`, `&str`, paths and anything else cannot implement it
//! (the trait is sealed), so a free-text field does not compile:
//!
//! ```compile_fail
//! fn is_field<T: greenline_engine::results::Field>() {}
//! is_field::<String>();
//! ```
//! ```compile_fail
//! fn is_field<T: greenline_engine::results::Field>() {}
//! is_field::<&'static str>();
//! ```
//!
//! The reader is just as strict: a line with an unknown key, a duplicate key, a string outside the
//! enums, an escape sequence, a nested value or `null` is dropped. [`ResultsFile::export`] re-serialises
//! the records it could parse, so even a damaged or tampered file cannot carry text out.
//!
//! # The file
//!
//! One JSON object per line, appended and `fsync`ed at once ([`ResultsFile::append`]), so a run killed
//! by Android still has every finished step on disk. A run is `run_start`, its `step` lines, then
//! `run_end`; a run with no end found at the next launch gets a `probable_kill` line
//! ([`ResultsFile::recover`]).
//!
//! Nothing here logs.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::api::MemReason;
use crate::open::FdKind;
use crate::{mem, spread_pages, Bounds, FIT_WIDTH, TILE_SIZE, TILE_ZOOM};

/// Version of the line format (`v` in every record).
pub const SCHEMA: u32 = 1;

// ---------------------------------------------------------------------------------------------
// the plan of a scripted run

/// The glspike `dl` positions: centres of the four 400 % tiles, as page fractions.
pub const TILE_FRACTIONS: [(f32, f32); 4] = [(0.5, 0.5), (0.25, 0.25), (0.75, 0.6), (0.4, 0.8)];
/// Pages visited by the browse step (spread through the file, first and last included).
pub const SPREAD_PAGES: i32 = 20;
/// Fit width of the first view, the browse and the walk (the engine's `FIT_WIDTH`).
pub const MEASURE_FIT_WIDTH: u32 = FIT_WIDTH;
/// The refill is measured at this zoom position (page fractions of the viewport centre).
pub const REFILL_CENTRE: (f32, f32) = (0.5, 0.5);

/// What a scripted run does after the first view, for one page size and viewport.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// Top-left pixels of the four 400 % tiles ([`TILE_FRACTIONS`]).
    pub tiles: Vec<(i32, i32)>,
    /// Viewport top-left at 400 % for the refill (page pixels; negative when the page is narrower).
    pub view_x0: i32,
    pub view_y0: i32,
    /// Grid tiles (top-left pixels, multiples of `TILE_SIZE`) covering that viewport, nearest the centre
    /// first: the full-screen refill.
    pub refill: Vec<(i32, i32)>,
    /// Pages for the browse step: [`SPREAD_PAGES`] spread pages, page 0 moved to the end (the first view
    /// left page 0's display list in memory, so it would not be a fresh read first). Empty for a 1-page
    /// file (the step is skipped).
    pub pages: Vec<i32>,
}

/// The plan for a `pages`-page file whose first page is `pt_w` x `pt_h` points, on a `view_w` x `view_h`
/// pixel viewport.
pub fn plan(pages: i32, pt_w: f32, pt_h: f32, view_w: u32, view_h: u32) -> Plan {
    let b = Bounds { x0: 0.0, y0: 0.0, x1: pt_w.max(0.0), y1: pt_h.max(0.0) };
    let tiles = TILE_FRACTIONS.iter().map(|&(fx, fy)| b.tile_origin_at(TILE_ZOOM, fx, fy)).collect();
    let (vw, vh) = (view_w.max(1) as i64, view_h.max(1) as i64);
    let pw = (pt_w.max(0.0) * TILE_ZOOM).ceil() as i64;
    let ph = (pt_h.max(0.0) * TILE_ZOOM).ceil() as i64;
    let origin = |p: i64, v: i64, f: f32| -> i64 {
        if p <= v {
            (p - v) / 2
        } else {
            ((p as f64 * f as f64 - v as f64 / 2.0).round() as i64).clamp(0, p - v)
        }
    };
    let ox = origin(pw, vw, REFILL_CENTRE.0);
    let oy = origin(ph, vh, REFILL_CENTRE.1);
    let t = TILE_SIZE as i64;
    let mut refill = Vec::new();
    if pw > 0 && ph > 0 {
        let (nx, ny) = ((pw + t - 1) / t, (ph + t - 1) / t);
        let (tx0, tx1) = ((ox.max(0)) / t, ((ox + vw - 1) / t).min(nx - 1));
        let (ty0, ty1) = ((oy.max(0)) / t, ((oy + vh - 1) / t).min(ny - 1));
        let (cx, cy) = (ox as f64 + vw as f64 / 2.0, oy as f64 + vh as f64 / 2.0);
        let mut v: Vec<(f64, i32, i32)> = Vec::new();
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                let d = ((tx * t + t / 2) as f64 - cx).hypot((ty * t + t / 2) as f64 - cy);
                v.push((d, (tx * t) as i32, (ty * t) as i32));
            }
        }
        v.sort_by(|a, b| a.0.total_cmp(&b.0));
        refill = v.into_iter().map(|(_, x, y)| (x, y)).collect();
    }
    let mut spread = if pages > 1 { spread_pages(pages, SPREAD_PAGES) } else { Vec::new() };
    if !spread.is_empty() {
        spread.rotate_left(1); // page 0 last
    }
    Plan { tiles, view_x0: ox as i32, view_y0: oy as i32, refill, pages: spread }
}

// ---------------------------------------------------------------------------------------------
// field types

mod sealed {
    pub trait Sealed {}
}

/// A value read from a results line. Strings are only ever enum names.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Raw<'a> {
    Num(f64),
    Bool(bool),
    Str(&'a str),
}

/// A type that may appear in a [`Record`]: numbers, bools, the fixed enums and `Option` of them.
/// Sealed: no other type can implement it (see the module docs).
pub trait Field: sealed::Sealed + Sized {
    /// The JSON text of the value, or None to leave the key out (an absent option, a non-finite number).
    fn json(&self) -> Option<String>;
    /// The value from a parsed JSON value of the right type, else None.
    fn from_raw(v: Raw<'_>) -> Option<Self>;
    /// The value for a key that may be missing: only `Option` accepts a missing key.
    fn from_key(v: Option<Raw<'_>>) -> Option<Self> {
        v.and_then(Self::from_raw)
    }
    /// The empty value (`None`, 0, false, the first enum variant), for `Default`.
    fn empty() -> Self;
}

/// Numbers are written with at most 3 decimals (plenty for ms and MB, and keeps lines short).
fn num(v: f64) -> Option<String> {
    if !v.is_finite() {
        return None;
    }
    let r = (v * 1000.0).round() / 1000.0;
    Some(if r == 0.0 { "0".into() } else { format!("{r}") })
}

impl sealed::Sealed for f64 {}
impl Field for f64 {
    fn json(&self) -> Option<String> {
        num(*self)
    }
    fn from_raw(v: Raw<'_>) -> Option<Self> {
        match v {
            Raw::Num(n) => Some(n),
            _ => None,
        }
    }
    fn empty() -> Self {
        0.0
    }
}

macro_rules! int_field {
    ($($t:ty),*) => {$(
        impl sealed::Sealed for $t {}
        impl Field for $t {
            fn json(&self) -> Option<String> {
                Some(self.to_string())
            }
            fn from_raw(v: Raw<'_>) -> Option<Self> {
                match v {
                    Raw::Num(n) if n.fract() == 0.0 && n >= <$t>::MIN as f64 && n <= <$t>::MAX as f64 => Some(n as $t),
                    _ => None,
                }
            }
            fn empty() -> Self {
                0
            }
        }
    )*};
}
int_field!(u32, u64, i32);

impl sealed::Sealed for bool {}
impl Field for bool {
    fn json(&self) -> Option<String> {
        Some(if *self { "true" } else { "false" }.into())
    }
    fn from_raw(v: Raw<'_>) -> Option<Self> {
        match v {
            Raw::Bool(b) => Some(b),
            _ => None,
        }
    }
    fn empty() -> Self {
        false
    }
}

impl<T: Field> sealed::Sealed for Option<T> {}
impl<T: Field> Field for Option<T> {
    fn json(&self) -> Option<String> {
        self.as_ref().and_then(T::json)
    }
    fn from_raw(v: Raw<'_>) -> Option<Self> {
        T::from_raw(v).map(Some)
    }
    fn from_key(v: Option<Raw<'_>>) -> Option<Self> {
        match v {
            None => Some(None),
            Some(r) => T::from_raw(r).map(Some),
        }
    }
    fn empty() -> Self {
        None
    }
}

/// A fixed enum written as one of its names.
macro_rules! fixed_enum {
    ($(#[$m:meta])* pub enum $name:ident { $($(#[$vm:meta])* $v:ident = $s:literal,)+ }) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name { $($(#[$vm])* $v,)+ }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$v,)+];
            /// The fixed name written to the results file.
            pub fn name(self) -> &'static str {
                match self { $($name::$v => $s,)+ }
            }
            pub fn from_name(s: &str) -> Option<$name> {
                match s { $($s => Some($name::$v),)+ _ => None }
            }
        }

        impl sealed::Sealed for $name {}
        impl Field for $name {
            fn json(&self) -> Option<String> {
                Some(format!("\"{}\"", self.name()))
            }
            fn from_raw(v: Raw<'_>) -> Option<Self> {
                match v { Raw::Str(s) => $name::from_name(s), _ => None }
            }
            fn empty() -> Self {
                $name::ALL[0]
            }
        }
    };
}

fixed_enum! {
    /// The size-rank label of a test file (f00 = biggest). The only way a file is named anywhere.
    pub enum Label {
        F00 = "f00", F01 = "f01", F02 = "f02", F03 = "f03", F04 = "f04", F05 = "f05",
        F06 = "f06", F07 = "f07", F08 = "f08", F09 = "f09", F10 = "f10", F11 = "f11",
    }
}

fixed_enum! {
    /// What a line is (`event` in the file).
    pub enum Kind {
        /// A run began: label, device info.
        RunStart = "run_start",
        /// One finished step of a run.
        Step = "step",
        /// The run ended (`cancelled` if the user stopped it).
        RunEnd = "run_end",
        /// Written at the next launch for a run that has no end: the app was most likely killed.
        ProbableKill = "probable_kill",
    }
}

fixed_enum! {
    /// The steps of a scripted run.
    pub enum Step {
        /// SAF pick and engine open: open_ms, pick_to_fd_ms, the fd probe, the provider.
        Open = "open",
        /// fd received -> low-res fit painted (`lowres_ms`) and -> full fit painted (`fit_ms`).
        FirstView = "first_view",
        /// One 400 % tile at a glspike `dl` position: render, handoff (total - engine wait) and total ms.
        Tile = "tile",
        /// Every visible 400 % tile at one zoom position, until all are painted (`ms`).
        Refill = "refill",
        /// One spread page through ShowPage at fit 1600 (`ms` in the UI, `engine_ms` in the engine).
        Page = "page",
        /// Whole-file walk: progress so far (every 25 pages).
        WalkProgress = "walk_progress",
        /// Whole-file walk: end (`walked` pages, `cancelled`).
        Walk = "walk",
    }
}

fixed_enum! {
    /// The document provider, from the URI authority. The authority string itself is never stored.
    pub enum Provider {
        Downloads = "downloads",
        ExternalStorage = "external_storage",
        Drive = "drive",
        Media = "media",
        Other = "other",
    }
}

impl Provider {
    /// Map a URI authority to the fixed set. Unknown providers are `Other`.
    pub fn from_authority(authority: &str) -> Provider {
        match authority {
            "com.android.providers.downloads.documents" => Provider::Downloads,
            "com.android.externalstorage.documents" => Provider::ExternalStorage,
            "com.android.providers.media.documents" | "media" => Provider::Media,
            a if a.starts_with("com.google.android.apps.docs") => Provider::Drive,
            _ => Provider::Other,
        }
    }
}

fixed_enum! {
    /// Why a step or an open failed. Engine error codes go in `engine_error` (a number).
    pub enum Failure {
        /// The picker's provider could not open the document.
        PickFailed = "pick_failed",
        /// The fd is a pipe or socket (refused, no copy fallback).
        NotSeekable = "not_seekable",
        BadFd = "bad_fd",
        /// MuPDF could not open the document.
        OpenFailed = "open_failed",
        /// Another open was in flight.
        Busy = "busy",
        /// The engine thread is gone.
        Engine = "engine",
        /// The engine answered with an error (`engine_error` has the code).
        EngineError = "engine_error",
        /// A tile or image request failed in the WebView (fetch or decode).
        Fetch = "fetch",
        Other = "other",
    }
}

impl Failure {
    /// Map the app's fixed open/pick error codes (`E_...`) to the fixed set.
    pub fn from_code(code: &str) -> Failure {
        match code {
            "E_NOT_SEEKABLE" => Failure::NotSeekable,
            "E_OPEN_FAILED" => Failure::OpenFailed,
            "E_BAD_FD" => Failure::BadFd,
            "E_BUSY" => Failure::Busy,
            "E_ENGINE" => Failure::Engine,
            c if c.starts_with("E_PICK_") => Failure::PickFailed,
            _ => Failure::Other,
        }
    }
}

fixed_enum! {
    /// A pass mark's outcome in the [`Summary`].
    pub enum Verdict {
        Pass = "pass",
        Fail = "fail",
        /// The mark does not apply to this file (first view is only marked for f06, f08, f09; the page
        /// mark needs more than one page).
        NotApplicable = "n/a",
        /// The step was not recorded (run killed, cancelled or failed before it).
        Missing = "missing",
    }
}

fn fd_kind_name(k: FdKind) -> &'static str {
    k.as_str()
}

impl sealed::Sealed for FdKind {}
impl Field for FdKind {
    fn json(&self) -> Option<String> {
        Some(format!("\"{}\"", fd_kind_name(*self)))
    }
    fn from_raw(v: Raw<'_>) -> Option<Self> {
        match v {
            Raw::Str("regular") => Some(FdKind::Regular),
            Raw::Str("pipe") => Some(FdKind::Pipe),
            Raw::Str("socket") => Some(FdKind::Socket),
            Raw::Str("other") => Some(FdKind::Other),
            _ => None,
        }
    }
    fn empty() -> Self {
        FdKind::Other
    }
}

/// Fixed name of a memory-guard reason (the same names the app's `gl-direct` / `gl-memory` events use).
pub fn reason_name(r: MemReason) -> &'static str {
    match r {
        MemReason::ListGuard => "list_guard",
        MemReason::SoftLimit => "soft_limit",
        MemReason::Trim => "trim",
    }
}

impl sealed::Sealed for MemReason {}
impl Field for MemReason {
    fn json(&self) -> Option<String> {
        Some(format!("\"{}\"", reason_name(*self)))
    }
    fn from_raw(v: Raw<'_>) -> Option<Self> {
        match v {
            Raw::Str("list_guard") => Some(MemReason::ListGuard),
            Raw::Str("soft_limit") => Some(MemReason::SoftLimit),
            Raw::Str("trim") => Some(MemReason::Trim),
            _ => None,
        }
    }
    fn empty() -> Self {
        MemReason::ListGuard
    }
}

/// Every string a results line may contain as a value: the names of all the fixed enums.
pub fn enum_names() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = Vec::new();
    v.extend(Label::ALL.iter().map(|x| x.name()));
    v.extend(Kind::ALL.iter().map(|x| x.name()));
    v.extend(Step::ALL.iter().map(|x| x.name()));
    v.extend(Provider::ALL.iter().map(|x| x.name()));
    v.extend(Failure::ALL.iter().map(|x| x.name()));
    v.extend(Verdict::ALL.iter().map(|x| x.name()));
    v.extend([FdKind::Regular, FdKind::Pipe, FdKind::Socket, FdKind::Other].map(fd_kind_name));
    v.extend([MemReason::ListGuard, MemReason::SoftLimit, MemReason::Trim].map(reason_name));
    v
}

// ---------------------------------------------------------------------------------------------
// flat JSON reader (only what the writer produces)

struct P<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> P<'a> {
    fn ws(&mut self) {
        while self.s.as_bytes().get(self.i).is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }
    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.s.as_bytes().get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    /// A string with no escapes (the writer never needs one; anything with a backslash is refused).
    fn string(&mut self) -> Option<&'a str> {
        if !self.eat(b'"') {
            return None;
        }
        let rest = &self.s[self.i..];
        let end = rest.find('"')?;
        let v = &rest[..end];
        if v.contains('\\') || v.chars().any(|c| c.is_control()) {
            return None;
        }
        self.i += end + 1;
        Some(v)
    }
    fn value(&mut self) -> Option<Raw<'a>> {
        self.ws();
        let rest = &self.s[self.i..];
        if rest.starts_with('"') {
            return self.string().map(Raw::Str);
        }
        if rest.starts_with("true") {
            self.i += 4;
            return Some(Raw::Bool(true));
        }
        if rest.starts_with("false") {
            self.i += 5;
            return Some(Raw::Bool(false));
        }
        let n = rest.find(|c: char| !(c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'))).unwrap_or(rest.len());
        if n == 0 {
            return None; // null, objects, arrays: never written, never accepted
        }
        let v: f64 = rest[..n].parse().ok()?;
        if !v.is_finite() {
            return None;
        }
        self.i += n;
        Some(Raw::Num(v))
    }
}

/// Parse one flat JSON object (`{"key": number | bool | "name", ...}`). None for anything else.
pub fn parse_flat(line: &str) -> Option<Vec<(&str, Raw<'_>)>> {
    let mut p = P { s: line.trim(), i: 0 };
    if !p.eat(b'{') {
        return None;
    }
    let mut out: Vec<(&str, Raw<'_>)> = Vec::new();
    if !p.eat(b'}') {
        loop {
            let k = p.string()?;
            if !p.eat(b':') {
                return None;
            }
            let v = p.value()?;
            if out.iter().any(|(kk, _)| *kk == k) {
                return None;
            }
            out.push((k, v));
            if p.eat(b',') {
                continue;
            }
            if p.eat(b'}') {
                break;
            }
            return None;
        }
    }
    p.ws();
    (p.i == p.s.len()).then_some(out)
}

// ---------------------------------------------------------------------------------------------
// records

/// Defines a struct whose every field is a [`Field`], with its JSON writer and strict reader.
macro_rules! record {
    ($(#[$m:meta])* pub struct $name:ident { $($(#[$fm:meta])* $f:ident: $t:ty,)+ }) => {
        $(#[$m])*
        #[derive(Clone, Debug, PartialEq)]
        pub struct $name { $($(#[$fm])* pub $f: $t,)+ }

        impl $name {
            /// Every key this type can write, in order.
            pub const KEYS: &'static [&'static str] = &[$(stringify!($f),)+];

            /// One JSON object on one line (no newline). Absent options are left out.
            pub fn to_json(&self) -> String {
                let mut s = String::from("{");
                $(
                    if let Some(v) = Field::json(&self.$f) {
                        if s.len() > 1 {
                            s.push(',');
                        }
                        s.push('"');
                        s.push_str(stringify!($f));
                        s.push_str("\":");
                        s.push_str(&v);
                    }
                )+
                s.push('}');
                s
            }

            /// Strict reader: None for an unknown or duplicate key, a missing required key, or a value of
            /// the wrong type (including any string that is not one of the fixed enum names).
            pub fn from_json(line: &str) -> Option<$name> {
                let kv = parse_flat(line)?;
                if kv.iter().any(|(k, _)| !Self::KEYS.contains(k)) {
                    return None;
                }
                let get = |k: &str| kv.iter().find(|(kk, _)| *kk == k).map(|(_, v)| *v);
                Some($name { $($f: <$t as Field>::from_key(get(stringify!($f)))?,)+ })
            }
        }

        impl Default for $name {
            fn default() -> Self {
                $name { $($f: <$t as Field>::empty(),)+ }
            }
        }
    };
}

record! {
    /// One line of the results file. Which fields are present depends on `event` and `step`.
    pub struct Record {
        /// Line format version ([`SCHEMA`]).
        v: u32,
        /// Run id: milliseconds since the Unix epoch when the run started.
        run: u64,
        label: Label,
        event: Kind,
        step: Option<Step>,
        /// `probable_kill`: the last step the run recorded before the app died.
        last_step: Option<Step>,
        /// Line number within the run (run_start is 0).
        seq: Option<u32>,

        // device (run_start). No model string: it is free text.
        /// Android API level (`ro.build.version.sdk`).
        sdk: Option<u32>,
        ram_mb: Option<u32>,
        page_kb: Option<u32>,
        cpus: Option<u32>,
        /// App version as major * 10000 + minor * 100 + patch.
        app_ver: Option<u32>,

        // open
        pages: Option<i32>,
        size_mb: Option<f64>,
        seekable: Option<bool>,
        fd_kind: Option<FdKind>,
        provider: Option<Provider>,
        pick_to_fd_ms: Option<f64>,
        open_ms: Option<f64>,
        failure: Option<Failure>,
        engine_error: Option<u32>,

        // first view
        /// fd received -> low-res fit image painted on the canvas (the first-view mark).
        lowres_ms: Option<f64>,
        /// fd received -> full fit image painted.
        fit_ms: Option<f64>,
        /// Engine: display-list read.
        list_ms: Option<f64>,
        /// Engine: ShowPage start -> low-res image ready.
        engine_lowres_ms: Option<f64>,
        /// Engine: ShowPage start -> fit image ready.
        engine_fit_ms: Option<f64>,
        /// Page size in points.
        pt_w: Option<f64>,
        pt_h: Option<f64>,

        // tiles, refill, pages
        page: Option<i32>,
        /// Tile number 1..4 (the `dl` position).
        tile: Option<u32>,
        x0: Option<i32>,
        y0: Option<i32>,
        render_ms: Option<f64>,
        /// Engine wait (X-Gl-Wait-Ms: request received -> engine answer).
        wait_ms: Option<f64>,
        /// total - wait: protocol, WebView transfer, bitmap and paint.
        handoff_ms: Option<f64>,
        /// Request -> tile painted.
        total_ms: Option<f64>,
        /// Refill: tiles painted.
        tiles: Option<u32>,
        /// Step time (refill: all tiles painted; page: show_page -> fit image event in the UI).
        ms: Option<f64>,
        /// Page: engine time, ShowPage start -> fit image ready.
        engine_ms: Option<f64>,

        // walk
        walked: Option<i32>,
        max_ms: Option<f64>,
        cancelled: Option<bool>,

        // memory after the step (main process; the WebView renders in another process)
        rss_mb: Option<f64>,
        /// VmHWM: the main process's peak RSS so far.
        hwm_mb: Option<f64>,
        /// The WebView's `performance.memory.usedJSHeapSize`, when the WebView has it.
        js_heap_mb: Option<f64>,
        /// The step's page is in direct mode (memory guard), and why.
        direct: Option<bool>,
        direct_reason: Option<MemReason>,
        direct_renders: Option<u64>,
        mem_releases: Option<u64>,
        reads_abandoned: Option<u64>,
    }
}

impl Record {
    /// A record with only the always-present fields set.
    pub fn new(run: u64, label: Label, event: Kind) -> Record {
        Record { v: SCHEMA, run, label, event, ..Record::default() }
    }

    /// A `step` record.
    pub fn step(run: u64, label: Label, step: Step) -> Record {
        Record { step: Some(step), ..Record::new(run, label, Kind::Step) }
    }

    /// Set `rss_mb` and `hwm_mb` from /proc/self (main process).
    pub fn with_memory(mut self) -> Record {
        self.rss_mb = Some(mem::rss_mb());
        self.hwm_mb = Some(mem::peak_mb());
        self
    }

    /// Set the device fields (run_start).
    pub fn with_device(mut self, d: Device) -> Record {
        self.sdk = d.sdk;
        self.ram_mb = d.ram_mb;
        self.page_kb = d.page_kb;
        self.cpus = d.cpus;
        self
    }
}

/// Keys the measurement page may not send: the app fills them in (run identity, line number, device,
/// open, memory and direct-mode fields).
const UI_FORBIDDEN: &[&str] = &[
    "last_step", "seq", "sdk", "ram_mb", "page_kb", "cpus", "app_ver", "size_mb", "seekable", "fd_kind", "provider",
    "pick_to_fd_ms", "open_ms", "rss_mb", "hwm_mb", "direct", "direct_reason", "direct_renders", "mem_releases",
    "reads_abandoned",
];

/// A step reported by the measurement page as one JSON object (`{"step":"tile","tile":1,...}`), for run
/// `run` of `label`. Accepted only if, with the app's `v`, `run`, `label` and `event` put in front, it
/// parses as a strict [`Record`] (so no text, no unknown keys, and no `run`/`label`/`event` of its own:
/// they would be duplicate keys), names a step the page performs (not `open`), and leaves the app's own
/// fields alone. The caller adds memory, direct mode and `seq`.
pub fn ui_step(json: &str, run: u64, label: Label) -> Option<Record> {
    let body = json.trim().strip_prefix('{')?;
    if body.trim_start().starts_with('}') {
        return None;
    }
    let head = Record::new(run, label, Kind::Step).to_json();
    let line = format!("{},{}", &head[..head.len() - 1], body);
    let kv = parse_flat(&line)?;
    if kv.iter().any(|(k, _)| UI_FORBIDDEN.contains(k)) {
        return None;
    }
    let r = Record::from_json(&line)?;
    match r.step {
        Some(Step::FirstView | Step::Tile | Step::Refill | Step::Page | Step::WalkProgress | Step::Walk) => Some(r),
        _ => None,
    }
}

/// App version `major.minor.patch` as major * 10000 + minor * 100 + patch.
pub fn version_code(v: &str) -> Option<u32> {
    let mut it = v.split('.').map(|p| p.parse::<u32>().ok());
    let (a, b, c) = (it.next()??, it.next()??, it.next().unwrap_or(Some(0))?);
    (b < 100 && c < 100).then_some(a * 10000 + b * 100 + c)
}

// ---------------------------------------------------------------------------------------------
// device info (numbers only; no model string)

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Device {
    pub sdk: Option<u32>,
    pub ram_mb: Option<u32>,
    pub page_kb: Option<u32>,
    pub cpus: Option<u32>,
}

pub fn device() -> Device {
    let ram_mb = std::fs::read_to_string("/proc/meminfo").ok().and_then(|s| {
        let l = s.lines().find(|l| l.starts_with("MemTotal:"))?;
        let kb: u64 = l.split_whitespace().nth(1)?.parse().ok()?;
        u32::try_from(kb / 1024).ok()
    });
    let ps = mem::page_size();
    Device {
        sdk: android_sdk(),
        ram_mb,
        page_kb: (ps > 0).then_some((ps / 1024) as u32),
        cpus: std::thread::available_parallelism().ok().map(|n| n.get() as u32),
    }
}

#[cfg(target_os = "android")]
fn android_sdk() -> Option<u32> {
    let mut buf = [0 as libc::c_char; 92]; // PROP_VALUE_MAX
    let name = b"ro.build.version.sdk\0";
    let n = unsafe { libc::__system_property_get(name.as_ptr() as *const libc::c_char, buf.as_mut_ptr()) };
    if n <= 0 {
        return None;
    }
    let s = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
    s.to_str().ok()?.trim().parse().ok()
}

#[cfg(not(target_os = "android"))]
fn android_sdk() -> Option<u32> {
    None
}

// ---------------------------------------------------------------------------------------------
// the file

/// The app-private results file (one [`Record`] per line).
#[derive(Clone, Debug)]
pub struct ResultsFile {
    path: PathBuf,
}

impl ResultsFile {
    pub fn new(path: impl Into<PathBuf>) -> ResultsFile {
        ResultsFile { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record and `fsync` it before returning. If the last line was cut short (the app was
    /// killed mid-write), it is ended first so this record starts on its own line.
    pub fn append(&self, r: &Record) -> io::Result<()> {
        let mut f = OpenOptions::new().read(true).append(true).create(true).open(&self.path)?;
        let len = f.metadata()?.len();
        let mut line = String::new();
        if len > 0 {
            let mut last = [0u8; 1];
            f.seek(SeekFrom::Start(len - 1))?;
            f.read_exact(&mut last)?;
            if last[0] != b'\n' {
                line.push('\n');
            }
        }
        line.push_str(&r.to_json());
        line.push('\n');
        f.write_all(line.as_bytes())?;
        f.sync_all()?;
        if len == 0 {
            // A new file: make its directory entry durable too.
            if let Some(dir) = self.path.parent() {
                if let Ok(d) = File::open(dir) {
                    let _ = d.sync_all();
                }
            }
        }
        Ok(())
    }

    /// Every valid record, in file order (unparsable lines are skipped). A missing file is empty.
    pub fn read(&self) -> io::Result<Vec<Record>> {
        let text = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok(String::from_utf8_lossy(&text).lines().filter_map(Record::from_json).collect())
    }

    /// The export: every valid record, re-serialised, one per line. Only typed values can come out.
    pub fn export(&self) -> io::Result<String> {
        let mut s = String::new();
        for r in self.read()? {
            s.push_str(&r.to_json());
            s.push('\n');
        }
        Ok(s)
    }

    /// At launch: every run with a `run_start` but no `run_end` / `probable_kill` gets a `probable_kill`
    /// line (with the last step it recorded). Returns the lines written.
    pub fn recover(&self) -> io::Result<Vec<Record>> {
        let kills = unfinished(&self.read()?);
        for k in &kills {
            self.append(k)?;
        }
        Ok(kills)
    }
}

/// The `probable_kill` records for the runs in `records` that never ended.
pub fn unfinished(records: &[Record]) -> Vec<Record> {
    // (run, label, last step, next seq)
    let mut open: Vec<(u64, Label, Option<Step>, u32)> = Vec::new();
    for r in records {
        match r.event {
            Kind::RunStart => {
                open.retain(|o| o.0 != r.run);
                open.push((r.run, r.label, None, 1));
            }
            Kind::Step => {
                if let Some(o) = open.iter_mut().find(|o| o.0 == r.run) {
                    o.2 = r.step.or(o.2);
                    o.3 = o.3.max(r.seq.unwrap_or(0) + 1);
                }
            }
            Kind::RunEnd | Kind::ProbableKill => open.retain(|o| o.0 != r.run),
        }
    }
    open.into_iter()
        .map(|(run, label, last, seq)| Record { last_step: last, seq: Some(seq), ..Record::new(run, label, Kind::ProbableKill) })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// pass-mark summary

/// Part 2 pass marks (PLAN.md).
pub const FIRST_VIEW_MAX_MS: f64 = 12_000.0;
pub const TILE_MAX_MS: f64 = 700.0;
pub const REFILL_MAX_MS: f64 = 2_000.0;
pub const PAGE_MAX_MS: f64 = 60.0;
/// 1.5 GB, main process only (the WebView's own process is not counted here).
pub const PEAK_MAX_MB: f64 = 1_536.0;
/// The first-view mark applies to sheets #6, #8 and #9.
pub const FIRST_VIEW_LABELS: [Label; 3] = [Label::F06, Label::F08, Label::F09];

record! {
    /// The latest run of one label against the pass marks.
    pub struct Summary {
        label: Label,
        run: u64,
        /// The run has a `probable_kill` line.
        killed: bool,
        /// The run ended normally (not cancelled, not killed).
        complete: bool,
        kill: Verdict,
        /// fd -> low-res fit painted.
        first_view_ms: Option<f64>,
        first_view: Verdict,
        /// Slowest of the four 400 % tiles, request -> painted.
        tile_max_ms: Option<f64>,
        tile: Verdict,
        refill_ms: Option<f64>,
        refill: Verdict,
        /// Spread pages, engine ShowPage -> fit image: median (the mark) and slowest.
        page_median_ms: Option<f64>,
        page_max_ms: Option<f64>,
        page: Verdict,
        /// Highest VmHWM recorded in the run's other lines (main process; not the run_start line).
        peak_mb: Option<f64>,
        peak: Verdict,
        /// Some step of the run was in direct mode.
        direct: bool,
    }
}

fn mark(v: Option<f64>, limit: f64) -> Verdict {
    match v {
        None => Verdict::Missing,
        Some(x) if x <= limit => Verdict::Pass,
        Some(_) => Verdict::Fail,
    }
}

fn max_of(it: impl Iterator<Item = f64>) -> Option<f64> {
    it.fold(None, |m, x| Some(m.map_or(x, |m: f64| m.max(x))))
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

/// The latest run of each label that has one, against the pass marks.
pub fn summarise(records: &[Record]) -> Vec<Summary> {
    let mut out = Vec::new();
    for &label in Label::ALL {
        let Some(run) = records.iter().rev().find(|r| r.label == label && r.event == Kind::RunStart).map(|r| r.run) else {
            continue;
        };
        let rs: Vec<&Record> = records.iter().filter(|r| r.run == run && r.label == label).collect();
        let steps = |s: Step| rs.iter().copied().filter(move |r| r.event == Kind::Step && r.step == Some(s));
        let killed = rs.iter().any(|r| r.event == Kind::ProbableKill);
        let complete = !killed && rs.iter().any(|r| r.event == Kind::RunEnd && r.cancelled != Some(true));
        let pages = steps(Step::Open).filter_map(|r| r.pages).last();

        let first_view_ms = steps(Step::FirstView).filter_map(|r| r.lowres_ms).last();
        let first_view =
            if FIRST_VIEW_LABELS.contains(&label) { mark(first_view_ms, FIRST_VIEW_MAX_MS) } else { Verdict::NotApplicable };
        let tile_max_ms = max_of(steps(Step::Tile).filter_map(|r| r.total_ms));
        let refill_ms = steps(Step::Refill).filter_map(|r| r.ms).last();
        let page_ms: Vec<f64> = steps(Step::Page).filter_map(|r| r.engine_ms).collect();
        let page_max_ms = max_of(page_ms.iter().copied());
        let page_median_ms = median(page_ms);
        let page = if pages.is_some_and(|p| p <= 1) { Verdict::NotApplicable } else { mark(page_median_ms, PAGE_MAX_MS) };
        // Not the run_start line: the app writes it before it resets VmHWM, so it can hold the last run's peak.
        let peak_mb = max_of(rs.iter().filter(|r| r.event != Kind::RunStart).filter_map(|r| r.hwm_mb));
        out.push(Summary {
            label,
            run,
            killed,
            complete,
            kill: if killed { Verdict::Fail } else { Verdict::Pass },
            first_view_ms,
            first_view,
            tile_max_ms,
            tile: mark(tile_max_ms, TILE_MAX_MS),
            refill_ms,
            refill: mark(refill_ms, REFILL_MAX_MS),
            page_median_ms,
            page_max_ms,
            page,
            peak_mb,
            peak: mark(peak_mb, PEAK_MAX_MB),
            direct: rs.iter().any(|r| r.direct == Some(true)),
        });
    }
    out
}

/// The first label with no run yet (the screen's default), else the last label.
pub fn next_label(records: &[Record]) -> Label {
    Label::ALL
        .iter()
        .copied()
        .find(|l| !records.iter().any(|r| r.label == *l && r.event == Kind::RunStart))
        .unwrap_or(Label::F11)
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A record with every field set (checked below: every key is written).
    fn full() -> Record {
        Record {
            v: SCHEMA,
            run: 1_764_000_000_123,
            label: Label::F07,
            event: Kind::Step,
            step: Some(Step::Tile),
            last_step: Some(Step::Refill),
            seq: Some(4),
            sdk: Some(35),
            ram_mb: Some(7680),
            page_kb: Some(16),
            cpus: Some(8),
            app_ver: Some(100),
            pages: Some(497),
            size_mb: Some(53.25),
            seekable: Some(true),
            fd_kind: Some(FdKind::Regular),
            provider: Some(Provider::Drive),
            pick_to_fd_ms: Some(12.5),
            open_ms: Some(80.125),
            failure: Some(Failure::EngineError),
            engine_error: Some(6),
            lowres_ms: Some(5120.5),
            fit_ms: Some(6000.0),
            list_ms: Some(4000.25),
            engine_lowres_ms: Some(4500.0),
            engine_fit_ms: Some(5600.0),
            pt_w: Some(3370.0),
            pt_h: Some(2384.0),
            page: Some(0),
            tile: Some(3),
            x0: Some(9598),
            y0: Some(-12),
            render_ms: Some(220.5),
            wait_ms: Some(240.0),
            handoff_ms: Some(35.75),
            total_ms: Some(275.75),
            tiles: Some(6),
            ms: Some(1500.0),
            engine_ms: Some(42.0),
            walked: Some(120),
            max_ms: Some(300.0),
            cancelled: Some(false),
            rss_mb: Some(512.5),
            hwm_mb: Some(900.0),
            js_heap_mb: Some(10.5),
            direct: Some(true),
            direct_reason: Some(MemReason::ListGuard),
            direct_renders: Some(3),
            mem_releases: Some(1),
            reads_abandoned: Some(1),
        }
    }

    #[test]
    fn a_full_record_holds_only_numbers_bools_and_enum_names() {
        let r = full();
        let line = r.to_json();
        assert!(!line.contains('\n'));
        let kv = parse_flat(&line).expect("valid flat JSON");
        assert_eq!(kv.len(), Record::KEYS.len(), "every field is written: {line}");
        let names = enum_names();
        for (k, v) in &kv {
            match v {
                Raw::Num(n) => assert!(n.is_finite(), "{k}"),
                Raw::Bool(_) => {}
                Raw::Str(s) => assert!(names.contains(s), "{k}: {s:?} is not a fixed enum name"),
            }
        }
        // Independent of the reader: every value position starts with a digit, '-', 't', 'f' or a quote
        // whose content is a fixed name.
        for part in line.trim_matches(|c| c == '{' || c == '}').split(',') {
            let (_, v) = part.split_once(':').expect("key:value");
            if let Some(s) = v.strip_prefix('"') {
                assert!(names.contains(&s.trim_end_matches('"')), "{part}");
            } else {
                assert!(v == "true" || v == "false" || v.parse::<f64>().is_ok(), "{part}");
            }
        }
        assert_eq!(Record::from_json(&line), Some(r), "round trip");
    }

    #[test]
    fn summaries_hold_only_numbers_bools_and_enum_names() {
        let s = Summary {
            label: Label::F09,
            run: 1,
            killed: false,
            complete: true,
            kill: Verdict::Pass,
            first_view_ms: Some(1.0),
            first_view: Verdict::Pass,
            tile_max_ms: Some(2.0),
            tile: Verdict::Fail,
            refill_ms: Some(3.0),
            refill: Verdict::Missing,
            page_median_ms: Some(4.0),
            page_max_ms: Some(5.0),
            page: Verdict::NotApplicable,
            peak_mb: Some(6.0),
            peak: Verdict::Pass,
            direct: false,
        };
        let line = s.to_json();
        let kv = parse_flat(&line).unwrap();
        assert_eq!(kv.len(), Summary::KEYS.len());
        let names = enum_names();
        assert!(kv.iter().all(|(_, v)| match v {
            Raw::Str(x) => names.contains(x),
            _ => true,
        }));
        assert_eq!(Summary::from_json(&line), Some(s));
    }

    #[test]
    fn the_reader_refuses_text() {
        let ok = Record::new(5, Label::F01, Kind::RunStart).to_json();
        assert!(Record::from_json(&ok).is_some());
        for bad in [
            r#"{"v":1,"run":5,"label":"PLANTED-SECRET-NAME","event":"run_start"}"#,
            r#"{"v":1,"run":5,"label":"f01","event":"run_start","name":"x"}"#,
            r#"{"v":1,"run":5,"label":"f01","event":"run_start","provider":"com.example.secret"}"#,
            r#"{"v":1,"run":5,"label":"f01","event":"run_start","pages":"12"}"#,
            r#"{"v":1,"run":5,"label":"f01","event":"run_start","pages":null}"#,
            r#"{"v":1,"run":5,"label":"f01","event":"run_start","pages":{"a":1}}"#,
            r#"{"v":1,"run":5,"label":"f0","event":"run_start"}"#,
            r#"{"v":1,"run":5,"label":"f01","label":"f02","event":"run_start"}"#,
            r#"{"v":1,"run":5,"label":"f01","event":"run_start"} trailing"#,
            r#"{"v":1,"run":5,"label":"f01"}"#,
            r#"{"v":1,"run":5.5,"label":"f01","event":"run_start"}"#,
            "",
            "not json",
        ] {
            assert_eq!(Record::from_json(bad), None, "{bad}");
        }
    }

    #[test]
    fn ui_steps_are_strict() {
        let r = ui_step(r#"{"step":"tile","tile":2,"page":0,"render_ms":12.5,"total_ms":40}"#, 77, Label::F05).unwrap();
        assert_eq!((r.run, r.label, r.event, r.step, r.tile, r.total_ms), (77, Label::F05, Kind::Step, Some(Step::Tile), Some(2), Some(40.0)));
        for bad in [
            r#"{}"#,
            r#"{"tile":1}"#,
            r#"{"step":"open","open_ms":1}"#,
            r#"{"step":"tile","label":"f00"}"#,
            r#"{"step":"tile","run":1}"#,
            r#"{"step":"tile","event":"run_end"}"#,
            r#"{"step":"tile","rss_mb":1}"#,
            r#"{"step":"tile","direct":false}"#,
            r#"{"step":"tile","note":"PLANTED-SECRET-NAME"}"#,
            r#"{"step":"PLANTED-SECRET-NAME"}"#,
            r#"{"step":"tile"},{"x":1}"#,
            r#"["step"]"#,
        ] {
            assert_eq!(ui_step(bad, 77, Label::F05), None, "{bad}");
        }
    }

    #[test]
    fn non_finite_numbers_are_left_out() {
        let r = Record { ms: Some(f64::NAN), total_ms: Some(f64::INFINITY), ..Record::new(1, Label::F00, Kind::Step) };
        let line = r.to_json();
        assert!(!line.contains("ms"), "{line}");
    }

    #[test]
    fn providers_and_failures_map_to_fixed_names() {
        assert_eq!(Provider::from_authority("com.android.providers.downloads.documents"), Provider::Downloads);
        assert_eq!(Provider::from_authority("com.android.externalstorage.documents"), Provider::ExternalStorage);
        assert_eq!(Provider::from_authority("com.google.android.apps.docs.storage"), Provider::Drive);
        assert_eq!(Provider::from_authority("com.android.providers.media.documents"), Provider::Media);
        assert_eq!(Provider::from_authority("PLANTED-SECRET-NAME"), Provider::Other);
        assert_eq!(Failure::from_code("E_NOT_SEEKABLE"), Failure::NotSeekable);
        assert_eq!(Failure::from_code("E_PICK_OPEN_FAILED"), Failure::PickFailed);
        assert_eq!(Failure::from_code("anything"), Failure::Other);
        assert_eq!(version_code("0.1.0"), Some(100));
        assert_eq!(version_code("1.2.3"), Some(10203));
        assert_eq!(version_code("x"), None);
    }

    fn temp(name: &str) -> ResultsFile {
        let dir = std::env::temp_dir().join(format!("gl-results-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        let _ = std::fs::remove_file(&p);
        ResultsFile::new(p)
    }

    #[test]
    fn an_unfinished_run_is_recorded_as_a_probable_kill_once() {
        let f = temp("kill.jsonl");
        f.append(&Record::new(1, Label::F02, Kind::RunStart)).unwrap();
        f.append(&Record { seq: Some(1), ..Record::step(1, Label::F02, Step::Open) }).unwrap();
        f.append(&Record::new(1, Label::F02, Kind::RunEnd)).unwrap();
        f.append(&Record::new(2, Label::F03, Kind::RunStart)).unwrap();
        f.append(&Record { seq: Some(1), ..Record::step(2, Label::F03, Step::Open) }).unwrap();
        f.append(&Record { seq: Some(2), ..Record::step(2, Label::F03, Step::FirstView) }).unwrap();
        // Killed mid-write: half a line at the end.
        {
            let mut h = OpenOptions::new().append(true).open(f.path()).unwrap();
            h.write_all(br#"{"v":1,"run":2,"lab"#).unwrap();
        }
        let kills = f.recover().unwrap();
        assert_eq!(kills.len(), 1);
        assert_eq!(kills[0].to_json(), r#"{"v":1,"run":2,"label":"f03","event":"probable_kill","last_step":"first_view","seq":3}"#);
        assert!(f.recover().unwrap().is_empty(), "recorded once");
        let all = f.read().unwrap();
        assert_eq!(all.len(), 7, "the cut line is skipped, the kill line is on its own line");
        let s = summarise(&all);
        assert_eq!(s.len(), 2);
        assert!(s[1].killed && s[1].kill == Verdict::Fail && !s[1].complete);
        assert!(!s[0].killed && s[0].complete);
        assert_eq!(next_label(&all), Label::F00);
        let _ = std::fs::remove_file(f.path());
    }

    #[test]
    fn summary_applies_the_pass_marks() {
        let run = 9;
        let l = Label::F08;
        let mut rs = vec![Record::new(run, l, Kind::RunStart)];
        rs.push(Record { pages: Some(1), hwm_mb: Some(400.0), ..Record::step(run, l, Step::Open) });
        rs.push(Record { lowres_ms: Some(11_000.0), hwm_mb: Some(1600.0), ..Record::step(run, l, Step::FirstView) });
        for t in [300.0, 650.0, 701.0, 100.0] {
            rs.push(Record { total_ms: Some(t), ..Record::step(run, l, Step::Tile) });
        }
        rs.push(Record { ms: Some(1999.0), ..Record::step(run, l, Step::Refill) });
        rs.push(Record::new(run, l, Kind::RunEnd));
        let s = &summarise(&rs)[0];
        assert_eq!((s.first_view, s.tile, s.refill, s.page, s.peak), (Verdict::Pass, Verdict::Fail, Verdict::Pass, Verdict::NotApplicable, Verdict::Fail));
        assert_eq!(s.tile_max_ms, Some(701.0));
        assert!(s.complete);
    }

    #[test]
    fn plan_matches_glspike_positions_and_covers_the_viewport() {
        // A1 landscape, 2000 pages, a 1080 x 2200 px screen.
        let p = plan(2000, 2384.0, 1684.0, 1080, 2200);
        let b = Bounds { x0: 0.0, y0: 0.0, x1: 2384.0, y1: 1684.0 };
        assert_eq!(p.tiles, TILE_FRACTIONS.iter().map(|&(x, y)| b.tile_origin_at(TILE_ZOOM, x, y)).collect::<Vec<_>>());
        assert_eq!(p.pages.len(), 20);
        assert_eq!(*p.pages.last().unwrap(), 0);
        assert_eq!(p.pages[18], 1999);
        // Every viewport pixel is covered by some refill tile.
        let t = TILE_SIZE as i32;
        for (x, y) in [(0, 0), (1079, 0), (0, 2199), (1079, 2199), (540, 1100)] {
            let (px, py) = (p.view_x0 + x, p.view_y0 + y);
            assert!(p.refill.iter().any(|&(tx, ty)| px >= tx && px < tx + t && py >= ty && py < ty + t), "({x},{y})");
        }
        assert!(p.refill.iter().all(|&(x, y)| x % t == 0 && y % t == 0));
        // 1-page file: no browse step. A page smaller than the screen: one tile, centred viewport.
        let small = plan(1, 200.0, 200.0, 1080, 2200);
        assert!(small.pages.is_empty());
        assert_eq!(small.refill, vec![(0, 0)]);
        assert!(small.view_x0 < 0 && small.view_y0 < 0);
    }
}
