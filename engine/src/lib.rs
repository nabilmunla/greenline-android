//! greenline-engine: a small, synchronous API over MuPDF (the `mupdf` 0.8 crate).
//!
//! This is the MuPDF path of the phase 1 spike (`spike/src/main.rs`: `mu`, `dl`, `walk`) turned into a
//! library. The MuPDF calls are the ones that were measured on the phone; nothing here prints or logs,
//! and the only JSON is [`results`]' numbers-only measurement lines (typed so they cannot hold text).
//! Anything glspike measured is returned to the caller in a small timings struct.
//!
//! Output is always straight RGBA, 4 bytes per pixel, alpha 255 (what the WebView's `ImageData` takes).
//! MuPDF renders into RGB pixmaps (the proven path); the RGB to RGBA step is timed separately
//! (`convert_ms`).
//!
//! # Threading
//!
//! Only [`worker`] starts threads; everything else is synchronous. MuPDF's context is thread-local (`mupdf::Context::get()` clones
//! a base context per thread, sharing the object store), so a [`Doc`] or a [`DisplayList`] must be
//! created, used and dropped on one thread. Both types are deliberately `!Send` and `!Sync`, even
//! though the `mupdf` crate marks its own `DisplayList` as `Send + Sync`. The engine worker thread
//! ([`worker::Engine`]) owns all of them.
//!
//! ```compile_fail
//! fn needs_send<T: Send>() {}
//! needs_send::<greenline_engine::Doc>();
//! ```
//! ```compile_fail
//! fn needs_send<T: Send>() {}
//! needs_send::<greenline_engine::DisplayList>();
//! ```

pub mod api;
pub mod guard;
pub mod mem;
pub mod view;
pub mod worker;
pub mod open;
pub mod results;

use std::fmt;
use std::marker::PhantomData;
use std::path::Path;
use std::time::Instant;

use mupdf::{Colorspace, Device, Document, IRect, Matrix, Pixmap, Rect};

/// Re-exported so callers of [`Doc::build_display_list_with_cookie`] can name it.
pub use mupdf::Cookie;

/// Width of the whole-page fit view, in pixels (glspike `FIT_W`).
pub const FIT_WIDTH: u32 = 1600;
/// Most pixels in a fit image (DS2.2): 16 Mpx, the same as the largest region (4096 x 4096).
pub const FIT_MAX_PX: u64 = 16 << 20;
/// Longest side of a fit image (DS2.2): a margin for the WebView, not a measured limit.
pub const FIT_MAX_SIDE: u32 = 16384;
/// Tile edge, in pixels (glspike `TILE`). Tiles are always `TILE_SIZE` x `TILE_SIZE`.
pub const TILE_SIZE: u32 = 1024;
/// The 400 % zoom glspike measured (glspike `ZOOM`).
pub const TILE_ZOOM: f32 = 4.0;

// ---------------------------------------------------------------------------------------------
// errors

#[derive(Debug)]
pub enum Error {
    /// An error from MuPDF (open, load, render, ...). Its text may contain the file path: do not log it
    /// where file names are confidential.
    Mupdf(mupdf::Error),
    /// Page index outside 0..page_count.
    PageOutOfRange { page: i32, count: i32 },
    /// A request the engine refuses (zero width, degenerate page, unsupported pixmap, ...).
    Invalid(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Mupdf(e) => write!(f, "mupdf: {e}"),
            Error::PageOutOfRange { page, count } => write!(f, "page {page} out of range (0..{count})"),
            Error::Invalid(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for Error {}

impl From<mupdf::Error> for Error {
    fn from(e: mupdf::Error) -> Self {
        Error::Mupdf(e)
    }
}

// ---------------------------------------------------------------------------------------------
// plain data

/// A page rectangle in page units (points), as MuPDF reports it (origin normally 0,0).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Bounds {
    pub fn width(&self) -> f32 {
        self.x1 - self.x0
    }
    pub fn height(&self) -> f32 {
        self.y1 - self.y0
    }
    /// Top-left pixel of a 1024 px tile centred at fraction (`fx`, `fy`) of the page at `zoom`
    /// (the positions glspike `dl` used: (0.5,0.5), (0.25,0.25), (0.75,0.6), (0.4,0.8)).
    pub fn tile_origin_at(&self, zoom: f32, fx: f32, fy: f32) -> (i32, i32) {
        let half = (TILE_SIZE / 2) as i32;
        (
            (self.width() * zoom * fx) as i32 - half,
            (self.height() * zoom * fy) as i32 - half,
        )
    }
}

impl From<Rect> for Bounds {
    fn from(r: Rect) -> Self {
        Bounds { x0: r.x0, y0: r.y0, x1: r.x1, y1: r.y1 }
    }
}

/// A rendered image: `data.len() == width * height * 4`, rows top to bottom, R G B A, alpha 255.
#[derive(Clone, Debug)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Where the time of one render went, in milliseconds.
#[derive(Clone, Copy, Debug, Default)]
pub struct RenderTimings {
    /// `Document::load_page` plus `Page::bounds` (0 for renders from a display list: the page is not touched).
    pub load_ms: f64,
    /// Pixmap allocation, white fill, and drawing (direct: reading the page content; list: replaying it).
    pub render_ms: f64,
    /// RGB pixmap to RGBA bytes.
    pub convert_ms: f64,
}

impl RenderTimings {
    pub fn total_ms(&self) -> f64 {
        self.load_ms + self.render_ms + self.convert_ms
    }
}

#[derive(Clone, Debug)]
pub struct Rendered {
    pub image: Rgba,
    pub timings: RenderTimings,
}

/// Where the time of building a display list went, in milliseconds.
#[derive(Clone, Copy, Debug, Default)]
pub struct ListTimings {
    /// `Document::load_page` plus `Page::bounds`.
    pub load_ms: f64,
    /// Reading and interpreting the whole page content into the list (`Page::to_display_list`).
    pub list_ms: f64,
}

pub struct BuiltList {
    pub list: DisplayList,
    pub timings: ListTimings,
}

/// One page of a [`Doc::walk`]: `page` is 0-based, `ms` covers load, bounds and the fit render
/// (the pixmap is dropped, not converted: glspike `walk` and the 20-page browse).
#[derive(Clone, Copy, Debug)]
pub struct WalkStep {
    pub page: i32,
    pub ms: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct WalkSummary {
    /// Pages rendered.
    pub pages: i32,
    pub total_ms: f64,
    /// True if the visitor asked to stop early.
    pub stopped: bool,
}

/// `k` page indexes spread evenly over `0..n`, first and last included (glspike `spread`; k=20 for the browse).
pub fn spread_pages(n: i32, k: i32) -> Vec<i32> {
    if n <= k {
        return (0..n).collect();
    }
    if k <= 1 {
        return vec![0];
    }
    (0..k).map(|i| ((i as i64 * (n as i64 - 1)) / (k as i64 - 1)) as i32).collect()
}

/// Set MuPDF's anti-aliasing level (0, 2, 4, 6, 8; MuPDF default 8). glspike's `GL_AA` experiment.
/// Applies to the calling thread's MuPDF context only.
pub fn set_aa_level(bits: i32) {
    mupdf::Context::get().set_aa_level(bits);
}

/// The fd number in a `/proc/self/fd/N` path.
fn fd_of_proc_path(path: &Path) -> Option<i32> {
    path.to_str()?.strip_prefix("/proc/self/fd/")?.parse().ok()
}

extern "C" {
    /// engine/csrc/fdstream.c: a PDF document over a pread-backed fz_stream on `fd`; NULL on error.
    fn gl_open_fd_document(ctx: *mut mupdf_sys::fz_context, fd: i32, size: i64) -> *mut mupdf_sys::fz_document;
}

// `mupdf::Document` is `struct Document { inner: *mut fz_document }` (mupdf 0.8.0, src/document.rs)
// and its Drop calls fz_drop_document: the pointer is its only field, so it can take ownership of a
// document opened in C.
const _: () = assert!(std::mem::size_of::<Document>() == std::mem::size_of::<*mut mupdf_sys::fz_document>());

/// Open `fd` (not taken: the caller keeps it open while the document lives) as a PDF that MuPDF reads
/// with `pread`, in pieces. No copy of the file is made and the fd's own offset is not moved.
pub fn open_fd_document(fd: i32) -> Result<Document> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 || st.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(Error::Invalid("E_FD_NOT_REGULAR"));
    }
    // SAFETY: a valid context for this thread; the C side catches every MuPDF error and returns NULL.
    let p = unsafe { gl_open_fd_document(guard::ctx(), fd, st.st_size as i64) };
    if p.is_null() {
        return Err(Error::Invalid("E_OPEN_FAILED"));
    }
    // SAFETY: see the size assert above; the new Document owns the one reference C handed back.
    Ok(unsafe { std::mem::transmute::<*mut mupdf_sys::fz_document, Document>(p) })
}

fn ms_since(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

// ---------------------------------------------------------------------------------------------
// Doc

/// An open document. Not `Send`, not `Sync`: keep it on the thread that opened it.
pub struct Doc {
    inner: Document,
    rgb: Colorspace,
    page_count: i32,
    open_ms: f64,
    _not_send: PhantomData<*const ()>,
}

impl Doc {
    /// Open by path (`/proc/self/fd/N` works too). MuPDF reads the file lazily, in pieces, as it would
    /// an SAF file descriptor; `open_ms` and `page_count` are what glspike reported as `open_ms` / `pages`.
    pub fn open(path: impl AsRef<Path>) -> Result<Doc> {
        let t = Instant::now();
        // Android refuses to reopen an SAF fd by its /proc/self/fd path (seen on the phone, 25 Nov
        // 2026), so such a path is never reopened: MuPDF reads the fd itself (task 2bf.4).
        let inner = match fd_of_proc_path(path.as_ref()) {
            Some(fd) => open_fd_document(fd)?,
            None => Document::open(path.as_ref())?,
        };
        let page_count = inner.page_count()?;
        let open_ms = ms_since(t);
        Ok(Doc { inner, rgb: Colorspace::device_rgb(), page_count, open_ms, _not_send: PhantomData })
    }

    pub fn page_count(&self) -> i32 {
        self.page_count
    }

    /// Time `Doc::open` took (open plus page count).
    pub fn open_ms(&self) -> f64 {
        self.open_ms
    }

    fn check(&self, page: i32) -> Result<()> {
        if page < 0 || page >= self.page_count {
            return Err(Error::PageOutOfRange { page, count: self.page_count });
        }
        Ok(())
    }

    fn load(&self, page: i32) -> Result<(mupdf::Page, Bounds, f64)> {
        self.check(page)?;
        let t = Instant::now();
        let p = self.inner.load_page(page)?;
        let b: Bounds = p.bounds()?.into();
        if !(b.width() > 0.0 && b.height() > 0.0) {
            return Err(Error::Invalid("page has empty bounds"));
        }
        Ok((p, b, ms_since(t)))
    }

    /// Page size in page units.
    pub fn page_bounds(&self, page: i32) -> Result<Bounds> {
        Ok(self.load(page)?.1)
    }

    /// Whole page scaled to `width` pixels wide, drawn straight from the page (`Page::to_pixmap`).
    /// For `width = 1600` on an ordinary page the result is 1600 px wide; the height follows the aspect
    /// ratio (MuPDF's rounded bbox).
    pub fn render_fit(&self, page: i32, width: u32) -> Result<Rendered> {
        let (p, b, load_ms) = self.load(page)?;
        let s = fit_scale(&b, width)?;
        let t = Instant::now();
        let pm = p.to_pixmap(&Matrix::new_scale(s, s), &self.rgb, false, false)?;
        let render_ms = ms_since(t);
        finish(&pm, load_ms, render_ms)
    }

    /// A 1024 x 1024 tile at `zoom`, tile column `tx`, row `ty` (pixel origin `tx*1024, ty*1024` in the
    /// zoomed page), drawn straight from the page. Parts of the tile outside the page stay white.
    pub fn render_tile(&self, page: i32, zoom: f32, tx: i32, ty: i32) -> Result<Rendered> {
        self.render_tile_at(page, zoom, tx * TILE_SIZE as i32, ty * TILE_SIZE as i32)
    }

    /// Like [`render_tile`](Self::render_tile) with a free pixel origin (`x0`, `y0`) in the zoomed page.
    /// This is glspike's tile: new pixmap over the rect, white fill, draw device, `Page::run`.
    pub fn render_tile_at(&self, page: i32, zoom: f32, x0: i32, y0: i32) -> Result<Rendered> {
        check_zoom(zoom)?;
        let (p, _b, load_ms) = self.load(page)?;
        let t = Instant::now();
        let tile = new_tile(&self.rgb, x0, y0)?;
        {
            let dev = Device::from_pixmap(&tile)?;
            p.run(&dev, &Matrix::new_scale(zoom, zoom))?;
        }
        let render_ms = ms_since(t);
        finish(&tile, load_ms, render_ms)
    }

    /// Read the whole page once into a display list (annotations included), as glspike `dl` did.
    pub fn build_display_list(&self, page: i32) -> Result<BuiltList> {
        let (p, b, load_ms) = self.load(page)?;
        let t = Instant::now();
        let inner = p.to_display_list(true)?;
        let list_ms = ms_since(t);
        Ok(BuiltList {
            list: DisplayList { inner, bounds: b, _not_send: PhantomData },
            timings: ListTimings { load_ms, list_ms },
        })
    }

    /// Same list, built through `DisplayList::new` + a list device + `Page::run_with_cookie`, so the
    /// build reports progress and can be aborted with a [`Cookie`] (`mupdf` 0.8 has no cookie on
    /// `Page::to_display_list`). `Page::run` includes annotations and widgets, like `to_display_list(true)`.
    ///
    /// EXPERIMENTAL: only the same-thread behaviour is tested here. `Cookie` is `!Send` and `abort()`
    /// takes `&mut self`, so aborting from another thread needs an unsafe wrapper; see the engine tests.
    pub fn build_display_list_with_cookie(&self, page: i32, cookie: &Cookie) -> Result<BuiltList> {
        let (p, b, load_ms) = self.load(page)?;
        let t = Instant::now();
        let mut inner = mupdf::DisplayList::new(Rect { x0: b.x0, y0: b.y0, x1: b.x1, y1: b.y1 })?;
        {
            let dev = Device::from_display_list(&mut inner)?;
            p.run_with_cookie(&dev, &Matrix::IDENTITY, cookie)?;
        }
        let list_ms = ms_since(t);
        Ok(BuiltList {
            list: DisplayList { inner, bounds: b, _not_send: PhantomData },
            timings: ListTimings { load_ms, list_ms },
        })
    }

    /// Fit-render the given pages in order (`page` indexes, 0-based), calling `visit` after each. The
    /// pixmaps are dropped, not converted. `visit` returns false to stop. The caller samples
    /// [`mem::rss_mb`] inside `visit` if it wants a memory trace.
    pub fn walk_pages(
        &self,
        pages: impl IntoIterator<Item = i32>,
        width: u32,
        mut visit: impl FnMut(WalkStep) -> bool,
    ) -> Result<WalkSummary> {
        let t0 = Instant::now();
        let mut done = 0;
        let mut stopped = false;
        for page in pages {
            let t = Instant::now();
            let (p, b, _) = self.load(page)?;
            let s = fit_scale(&b, width)?;
            let _pm = p.to_pixmap(&Matrix::new_scale(s, s), &self.rgb, false, false)?;
            let ms = ms_since(t);
            done += 1;
            if !visit(WalkStep { page, ms }) {
                stopped = true;
                break;
            }
        }
        Ok(WalkSummary { pages: done, total_ms: ms_since(t0), stopped })
    }

    /// Every page in order (glspike `walk`).
    pub fn walk(&self, width: u32, visit: impl FnMut(WalkStep) -> bool) -> Result<WalkSummary> {
        self.walk_pages(0..self.page_count, width, visit)
    }
}

// ---------------------------------------------------------------------------------------------
// DisplayList

/// One page's drawing commands, read once. Fit views and tiles are replayed from it (only what falls
/// inside the area is drawn). Not `Send`, not `Sync`. Drop it on page change.
pub struct DisplayList {
    inner: mupdf::DisplayList,
    bounds: Bounds,
    _not_send: PhantomData<*const ()>,
}

impl DisplayList {
    /// The page bounds the list was built for.
    pub fn bounds(&self) -> Bounds {
        self.bounds
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Whole page at `width` pixels wide, replayed from the list. Same size and placement as
    /// [`Doc::render_fit`].
    pub fn render_fit(&self, width: u32) -> Result<Rendered> {
        self.render_fit_cookie(width, None)
    }

    /// [`render_fit`](Self::render_fit) with a [`Cookie`] (progress and abort; the engine worker's hook).
    /// An aborted replay still returns `Ok` with a partly drawn image: the caller must check its own
    /// abort flag and discard it.
    pub fn render_fit_cookie(&self, width: u32, cookie: Option<&Cookie>) -> Result<Rendered> {
        let b = self.bounds;
        let s = fit_scale(&b, width)?;
        let r = fit_irect(&b, s)?;
        let rgb = Colorspace::device_rgb();
        let t = Instant::now();
        let mut pm = Pixmap::new_with_rect(&rgb, r, false)?;
        pm.clear_with(255)?;
        {
            let d = Device::from_pixmap(&pm)?;
            self.replay(&d, &Matrix::new_scale(s, s), irect_to_rect(r), cookie)?;
        }
        let render_ms = ms_since(t);
        finish(&pm, 0.0, render_ms)
    }

    /// 1024 x 1024 tile at `zoom`, column `tx`, row `ty`, replayed from the list. Same placement as
    /// [`Doc::render_tile`].
    pub fn render_tile(&self, zoom: f32, tx: i32, ty: i32) -> Result<Rendered> {
        self.render_tile_at(zoom, tx * TILE_SIZE as i32, ty * TILE_SIZE as i32)
    }

    /// Like [`render_tile`](Self::render_tile) with a free pixel origin (glspike `dl` tile).
    pub fn render_tile_at(&self, zoom: f32, x0: i32, y0: i32) -> Result<Rendered> {
        self.render_tile_at_cookie(zoom, x0, y0, None)
    }

    /// [`render_tile_at`](Self::render_tile_at) with a [`Cookie`]; same abort rule as
    /// [`render_fit_cookie`](Self::render_fit_cookie).
    pub fn render_tile_at_cookie(&self, zoom: f32, x0: i32, y0: i32, cookie: Option<&Cookie>) -> Result<Rendered> {
        self.render_region_cookie(zoom, x0, y0, TILE_SIZE, TILE_SIZE, cookie)
    }

    /// A `w` x `h` region (each side up to [`MAX_REGION`]) at `zoom` with its top-left pixel at
    /// (`x0`, `y0`): one walk of the list, however large the region (task 2c.2; 2c.1 showed the walk,
    /// not the pixels, dominates a tile's time).
    pub fn render_region_cookie(&self, zoom: f32, x0: i32, y0: i32, w: u32, h: u32, cookie: Option<&Cookie>) -> Result<Rendered> {
        check_zoom(zoom)?;
        let rgb = Colorspace::device_rgb();
        let t = Instant::now();
        let tile = new_region(&rgb, x0, y0, w, h)?;
        {
            let d = Device::from_pixmap(&tile)?;
            let area = Rect { x0: x0 as f32, y0: y0 as f32, x1: x0 as f32 + w as f32, y1: y0 as f32 + h as f32 };
            self.replay(&d, &Matrix::new_scale(zoom, zoom), area, cookie)?;
        }
        let render_ms = ms_since(t);
        finish(&tile, 0.0, render_ms)
    }

    fn replay(&self, d: &Device, ctm: &Matrix, area: Rect, cookie: Option<&Cookie>) -> Result<()> {
        match cookie {
            Some(c) => self.inner.run_with_cookie(d, ctm, area, c)?,
            None => self.inner.run(d, ctm, area)?,
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// helpers

/// Scale for a fit image `width` px wide, lowered when needed so the image stays within
/// [`FIT_MAX_PX`] pixels and [`FIT_MAX_SIDE`] per side (DS2.2: a very tall page would otherwise follow
/// the width alone into hundreds of MB). A page with no usable size is an error, not a panic (#20).
fn fit_scale(b: &Bounds, width: u32) -> Result<f32> {
    if width == 0 {
        return Err(Error::Invalid("fit width is 0"));
    }
    let (bw, bh) = (b.width() as f64, b.height() as f64);
    let finite = [b.x0, b.y0, b.x1, b.y1].iter().all(|v| v.is_finite());
    if !(finite && bw > 0.0 && bh > 0.0) {
        return Err(Error::Invalid("page has no usable size"));
    }
    let mut s = width as f64 / bw;
    // A hair under each limit, so MuPDF's outward rounding of the bbox stays inside it.
    let px = (bw * s) * (bh * s);
    if px > FIT_MAX_PX as f64 {
        s *= (FIT_MAX_PX as f64 / px).sqrt() * 0.999;
    }
    let side = FIT_MAX_SIDE as f64 - 2.0;
    if bw.max(bh) * s > side {
        s = side / bw.max(bh);
    }
    let s = s as f32;
    if !(s.is_finite() && s > 0.0) {
        return Err(Error::Invalid("page has no usable size"));
    }
    Ok(s)
}

fn check_zoom(zoom: f32) -> Result<()> {
    if !(zoom > 0.0 && zoom.is_finite()) {
        return Err(Error::Invalid("zoom must be positive"));
    }
    Ok(())
}

/// The pixel rect MuPDF gives `Page::to_pixmap` for a scale-only matrix (`fz_round_rect` of the
/// transformed bounds), so list renders have exactly the size and origin of direct renders.
/// An origin too far out for i32 pixels, or an empty rect, is an error (#20), not a saturated or
/// zero-width pixmap.
fn fit_irect(b: &Bounds, s: f32) -> Result<IRect> {
    let px = |v: f32| -> Result<i32> {
        if v.is_finite() && v.abs() < (1u32 << 30) as f32 {
            Ok(v as i32)
        } else {
            Err(Error::Invalid("page origin out of range"))
        }
    };
    let r = IRect {
        x0: px((b.x0 * s + 0.001).floor())?,
        y0: px((b.y0 * s + 0.001).floor())?,
        x1: px((b.x1 * s - 0.001).ceil())?,
        y1: px((b.y1 * s - 0.001).ceil())?,
    };
    if r.x1 <= r.x0 || r.y1 <= r.y0 {
        return Err(Error::Invalid("page has no usable size"));
    }
    Ok(r)
}

fn irect_to_rect(r: IRect) -> Rect {
    Rect { x0: r.x0 as f32, y0: r.y0 as f32, x1: r.x1 as f32, y1: r.y1 as f32 }
}

/// A white 1024 x 1024 RGB pixmap whose origin is (x0, y0).
fn new_tile(rgb: &Colorspace, x0: i32, y0: i32) -> Result<Pixmap> {
    new_region(rgb, x0, y0, TILE_SIZE, TILE_SIZE)
}

/// Largest side of a region render (task 2c.2: one render of the visible area per settled view).
pub const MAX_REGION: u32 = 4096;

/// A white `w` x `h` RGB pixmap whose origin is (x0, y0).
fn new_region(rgb: &Colorspace, x0: i32, y0: i32, w: u32, h: u32) -> Result<Pixmap> {
    if w == 0 || h == 0 || w > MAX_REGION || h > MAX_REGION {
        return Err(Error::Invalid("region size out of range"));
    }
    // BUG-08: an origin so large that x0 + w overflows would wrap into a huge white image.
    let x1 = x0.checked_add(w as i32).ok_or(Error::Invalid("region origin out of range"))?;
    let y1 = y0.checked_add(h as i32).ok_or(Error::Invalid("region origin out of range"))?;
    let mut pm = Pixmap::new_with_rect(rgb, IRect { x0, y0, x1, y1 }, false)?;
    pm.clear_with(255)?;
    Ok(pm)
}

fn finish(pm: &Pixmap, load_ms: f64, render_ms: f64) -> Result<Rendered> {
    let t = Instant::now();
    let image = pixmap_to_rgba(pm)?;
    Ok(Rendered { image, timings: RenderTimings { load_ms, render_ms, convert_ms: ms_since(t) } })
}

/// RGB (or RGBA) MuPDF pixmap to tightly packed RGBA bytes.
fn pixmap_to_rgba(pm: &Pixmap) -> Result<Rgba> {
    let (w, h) = (pm.width() as usize, pm.height() as usize);
    let n = pm.n() as usize;
    let stride = pm.stride() as usize;
    let src = pm.samples();
    if src.len() < stride * h || stride < w * n {
        return Err(Error::Invalid("pixmap samples shorter than stride * height"));
    }
    let mut data = vec![0u8; w * h * 4];
    match n {
        3 => {
            for (drow, srow) in data.chunks_exact_mut(w * 4).zip(src.chunks(stride)) {
                for (d, s) in drow.chunks_exact_mut(4).zip(srow[..w * 3].chunks_exact(3)) {
                    d[0] = s[0];
                    d[1] = s[1];
                    d[2] = s[2];
                    d[3] = 255;
                }
            }
        }
        4 => {
            for (drow, srow) in data.chunks_exact_mut(w * 4).zip(src.chunks(stride)) {
                drow.copy_from_slice(&srow[..w * 4]);
            }
        }
        _ => return Err(Error::Invalid("unsupported pixmap component count")),
    }
    Ok(Rgba { width: w as u32, height: h as u32, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spread_matches_glspike() {
        assert_eq!(spread_pages(5, 20), vec![0, 1, 2, 3, 4]);
        let s = spread_pages(2000, 20);
        assert_eq!(s.len(), 20);
        assert_eq!((s[0], s[19]), (0, 1999));
        assert!(s.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn fit_irect_rounds_like_mupdf() {
        let b = Bounds { x0: 0.0, y0: 0.0, x1: 3370.0, y1: 2384.0 };
        let s = 1600.0 / 3370.0;
        let r = fit_irect(&b, s).unwrap();
        assert_eq!((r.x0, r.y0, r.x1), (0, 0, 1600));
        // DS2.2: an ordinary sheet keeps exactly the asked width.
        let r = fit_irect(&b, fit_scale(&b, 1600).unwrap()).unwrap();
        assert_eq!((r.x0, r.y0, r.x1), (0, 0, 1600));
    }

    #[test]
    fn fit_of_a_tall_page_stays_within_the_limits() {
        for (w, h) in [(100.0f32, 3000.0f32), (1.0, 8000.0), (3000.0, 100.0), (14400.0, 14400.0)] {
            let b = Bounds { x0: 0.0, y0: 0.0, x1: w, y1: h };
            let r = fit_irect(&b, fit_scale(&b, 1600).unwrap()).unwrap();
            let (pw, ph) = ((r.x1 - r.x0) as u64, (r.y1 - r.y0) as u64);
            assert!(pw * ph <= FIT_MAX_PX && pw.max(ph) <= FIT_MAX_SIDE as u64, "{w} x {h} pt gave {pw} x {ph}");
            assert!(pw > 0 && ph > 0);
        }
        // Up to 6.5 times taller than wide, nothing changes at 1600.
        let b = Bounds { x0: 0.0, y0: 0.0, x1: 1000.0, y1: 6500.0 };
        assert_eq!(fit_irect(&b, fit_scale(&b, 1600).unwrap()).unwrap().x1, 1600);
    }

    #[test]
    fn fit_of_a_page_with_no_usable_size_is_an_error() {
        let bad = [
            Bounds { x0: 0.0, y0: 0.0, x1: 0.0, y1: 792.0 },
            Bounds { x0: 0.0, y0: 0.0, x1: 612.0, y1: 0.0 },
            Bounds { x0: 0.0, y0: 0.0, x1: f32::INFINITY, y1: 792.0 },
            Bounds { x0: 0.0, y0: 0.0, x1: f32::NAN, y1: 792.0 },
            Bounds { x0: 10.0, y0: 0.0, x1: 5.0, y1: 792.0 },
        ];
        for b in bad {
            assert!(fit_scale(&b, 1600).is_err(), "{:?}", (b.x0, b.y0, b.x1, b.y1));
        }
        let far = Bounds { x0: 1e9, y0: 0.0, x1: 1e9 + 612.0, y1: 792.0 };
        assert!(fit_scale(&far, 1600).and_then(|s| fit_irect(&far, s)).is_err());
    }
}
