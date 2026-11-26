//! Shared helpers for the engine integration tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use greenline_engine::Rgba;

static HEAVY: Mutex<()> = Mutex::new(());

/// Tests run on parallel threads by default; the big sheets need hundreds of MB each, and the memory
/// readings are only meaningful when nothing else is running. Every test that touches MuPDF takes this.
pub fn serial() -> MutexGuard<'static, ()> {
    HEAVY.lock().unwrap_or_else(|e| e.into_inner())
}

/// `corpus/<name>` at the repo root (or under $GL_CORPUS). None, with a message, if the file is absent:
/// the test then passes without checking anything. Generate with `python3 tools/mkcorpus.py corpus lines set`.
pub fn corpus(name: &str) -> Option<PathBuf> {
    let dir = std::env::var_os("GL_CORPUS")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("corpus"));
    let p = dir.join(name);
    if p.is_file() {
        Some(p)
    } else {
        eprintln!("SKIP: corpus file {name} not found in {} (run: python3 tools/mkcorpus.py corpus lines set)", dir.display());
        None
    }
}

/// A small 3-page PDF written to the target tmp dir: always available, so the API is exercised even
/// without the corpus. Page 1 and 3 are 612 x 792, page 2 is 842 x 595 (landscape). Graphics only.
pub fn tiny_pdf() -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("tiny.pdf");
    let sizes = [(612, 792), (842, 595), (612, 792)];
    let mut out: Vec<u8> = b"%PDF-1.4\n".to_vec();
    let mut offs: Vec<usize> = Vec::new();
    let n_objs = 2 + sizes.len() * 2;
    let kids: Vec<String> = (0..sizes.len()).map(|i| format!("{} 0 R", 4 + i * 2)).collect();
    let mut objs: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        format!("<< /Type /Pages /Count {} /Kids [{}] >>", sizes.len(), kids.join(" ")),
    ];
    let mut streams: Vec<Option<String>> = vec![None, None];
    for (i, (w, h)) in sizes.iter().enumerate() {
        let content = format!(
            "0 0 1 rg 100 100 {} {} re f 1 0 0 RG 10 w 0 0 m {} {} l S 0 0.6 0 rg 20 20 60 60 re f\n",
            w / 2,
            h / 4,
            w,
            h
        );
        objs.push(format!("<< /Length {} >>", content.len()));
        streams.push(Some(content));
        objs.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Contents {} 0 R >>", 3 + i * 2));
        streams.push(None);
    }
    for (i, body) in objs.iter().enumerate() {
        offs.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{}\n", i + 1, body).as_bytes());
        if let Some(s) = &streams[i] {
            out.extend_from_slice(format!("stream\n{s}endstream\n").as_bytes());
        }
        out.extend_from_slice(b"endobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", n_objs + 1).as_bytes());
    for o in &offs {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", n_objs + 1).as_bytes());
    std::fs::write(&path, out).expect("write tiny.pdf");
    path
}

/// How different two RGBA images are: (pixels differing by more than `tol` in any channel, max channel delta).
pub fn diff(a: &Rgba, b: &Rgba, tol: u8) -> (usize, u8) {
    assert_eq!((a.width, a.height), (b.width, b.height), "image sizes differ");
    let mut n = 0;
    let mut max = 0u8;
    for (pa, pb) in a.data.chunks_exact(4).zip(b.data.chunks_exact(4)) {
        let d = pa.iter().zip(pb).map(|(x, y)| x.abs_diff(*y)).max().unwrap();
        max = max.max(d);
        if d > tol {
            n += 1;
        }
    }
    (n, max)
}

/// Number of pixels that are not pure white (to prove something was drawn).
pub fn non_white(a: &Rgba) -> usize {
    a.data.chunks_exact(4).filter(|p| p[0] != 255 || p[1] != 255 || p[2] != 255).count()
}

pub fn assert_rgba_shape(img: &Rgba, w: u32, h: Option<u32>) {
    assert_eq!(img.width, w, "width");
    if let Some(h) = h {
        assert_eq!(img.height, h, "height");
    }
    assert!(img.height > 0);
    assert_eq!(img.data.len(), (img.width * img.height * 4) as usize, "rgba length");
    assert!(img.data.chunks_exact(4).all(|p| p[3] == 255), "alpha must be 255 everywhere");
}
