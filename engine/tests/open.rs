//! Tests for opening a document from a file descriptor (engine/src/open.rs, task 2b.5).
//!
//! - `fd_*`: the 3-page tiny PDF written by the test helpers; always runs.
//! - `corpus_set_via_fd`: the synthetic corpus `corpus/set.pdf` (git-ignored); SKIPs if absent.
//! Run: cargo test --release -p greenline-engine -j 8 -- --nocapture

mod common;

use std::ffi::CString;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use common::*;
use greenline_engine::open::{open_fd, probe_fd, FdKind, NOT_SEEKABLE};
use greenline_engine::{Doc, Error};

fn open_ro(path: &Path) -> OwnedFd {
    let c = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    assert!(fd >= 0, "libc::open failed");
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn is_open(fd: RawFd) -> bool {
    let r = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    r != -1
}

#[test]
fn fd_open_tiny_page_count() {
    let _g = serial();
    let path = tiny_pdf();
    let bytes = std::fs::metadata(&path).unwrap().len();
    let fd = open_ro(&path);

    let p = probe_fd(std::os::fd::AsRawFd::as_raw_fd(&fd));
    assert!(p.seekable);
    assert_eq!(p.kind, FdKind::Regular);
    assert!((p.size_mb - bytes as f64 / 1048576.0).abs() < 1e-9, "size_mb {}", p.size_mb);

    let doc = open_fd(fd).unwrap();
    assert_eq!(doc.page_count(), 3);
    assert!(doc.probe().seekable);
    // the document works through the /proc/self/fd path
    let r = doc.render_fit(1, 400).unwrap();
    assert_rgba_shape(&r.image, 400, None);
    assert!(non_white(&r.image) > 100);
}

#[test]
fn fd_closed_when_doc_drops() {
    let _g = serial();
    let doc = open_fd(open_ro(&tiny_pdf())).unwrap();
    let raw = doc.raw_fd();
    assert!(is_open(raw), "fd must stay open while the document lives");
    assert_eq!(doc.page_count(), 3);
    drop(doc);
    assert!(!is_open(raw), "fd {raw} must be closed after the FdDoc drops");
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
}

/// 2bf.4: a /proc/self/fd path is opened through the pread stream (never reopened). The pages are
/// pixel-identical to a path open, and the fd's own offset does not move.
#[test]
fn fd_stream_matches_path_open() {
    let _g = serial();
    let mut files = vec![tiny_pdf()];
    files.extend(corpus("lines.pdf"));
    for path in files {
        let fd = open_ro(&path);
        let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
        let by_path = Doc::open(&path).unwrap();
        let by_fd = Doc::open(format!("/proc/self/fd/{raw}")).unwrap();
        assert_eq!(by_fd.page_count(), by_path.page_count());
        for page in [0, by_path.page_count() - 1] {
            let a = by_path.render_fit(page, 600).unwrap().image;
            let b = by_fd.render_fit(page, 600).unwrap().image;
            assert_eq!((a.width, a.height), (b.width, b.height));
            assert!(a.data == b.data, "page {page} differs between path and fd stream");
        }
        assert_eq!(unsafe { libc::lseek(raw, 0, libc::SEEK_CUR) }, 0, "fd offset moved");
    }
}

/// 2bf.4: opening through the stream does not load the file into memory (the old fallback did).
#[test]
fn fd_stream_open_does_not_copy_the_file() {
    let _g = serial();
    let Some(path) = corpus("heavy.pdf") else {
        eprintln!("SKIP: heavy.pdf missing");
        return;
    };
    let size_mb = std::fs::metadata(&path).unwrap().len() as f64 / 1048576.0;
    let fd = open_ro(&path);
    greenline_engine::mem::trim();
    let rss0 = greenline_engine::mem::rss_mb();
    let doc = Doc::open(format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&fd))).unwrap();
    let rss1 = greenline_engine::mem::rss_mb();
    eprintln!("  fd stream open of a {size_mb:.0} MB file: RSS {rss0:.0} -> {rss1:.0} MB");
    assert!(doc.page_count() > 0);
    assert!(rss1 - rss0 < 50.0, "RSS grew {:.0} MB opening a {size_mb:.0} MB file", rss1 - rss0);
}

/// 2bf review finding 7: a damaged (truncated) PDF behaves the same through the fd stream as through
/// a path open: MuPDF's repair seeks around, past the end included.
#[test]
fn fd_stream_truncated_file_matches_path_open() {
    let _g = serial();
    let full = std::fs::read(tiny_pdf()).unwrap();
    for keep in [0.6, 0.8, 0.95] {
        let path = std::env::temp_dir().join(format!("gl-truncated-{}.pdf", (keep * 100.0) as u32));
        std::fs::write(&path, &full[..(full.len() as f64 * keep) as usize]).unwrap();
        let fd = open_ro(&path);
        let by_path = Doc::open(&path).map(|d| d.page_count());
        let by_fd = Doc::open(format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&fd))).map(|d| d.page_count());
        assert_eq!(by_path.is_ok(), by_fd.is_ok(), "keep {keep}: path {by_path:?} vs fd {by_fd:?}");
        if let (Ok(a), Ok(b)) = (by_path, by_fd) {
            assert_eq!(a, b, "keep {keep}");
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// 2bf.4: the stream refuses what is not a regular file.
#[test]
fn fd_stream_refuses_a_pipe() {
    let _g = serial();
    let mut p = [0; 2];
    assert_eq!(unsafe { libc::pipe(p.as_mut_ptr()) }, 0);
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(p[0]), OwnedFd::from_raw_fd(p[1])) };
    let res = greenline_engine::open_fd_document(std::os::fd::AsRawFd::as_raw_fd(&r));
    assert!(matches!(res, Err(Error::Invalid("E_FD_NOT_REGULAR"))));
    drop((r, w));
}

/// 2bf.4: a file that is not a PDF fails cleanly (MuPDF's error is caught in C, not unwound).
#[test]
fn fd_stream_bad_file_fails_cleanly() {
    let _g = serial();
    let path = std::env::temp_dir().join("gl-not-a-pdf.bin");
    std::fs::write(&path, vec![0x55u8; 4096]).unwrap();
    let fd = open_ro(&path);
    let res = Doc::open(format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&fd)));
    assert!(res.is_err());
    // and the engine still works afterwards
    assert_eq!(Doc::open(tiny_pdf()).unwrap().page_count(), 3);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn fd_pipe_refused() {
    let _g = serial();
    let mut fds = [0 as RawFd; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // something in the pipe, so a reader would not just see EOF
    let hdr = b"%PDF-1.4\n";
    assert_eq!(unsafe { libc::write(fds[1], hdr.as_ptr().cast(), hdr.len()) }, hdr.len() as isize);

    let p = probe_fd(fds[0]);
    assert!(!p.seekable);
    assert_eq!(p.kind, FdKind::Pipe);
    assert_eq!(p.size_mb, 0.0);

    match open_fd(rd) {
        Err(Error::Invalid(code)) => assert_eq!(code, NOT_SEEKABLE),
        Err(e) => panic!("wrong error: {e:?}"),
        Ok(_) => panic!("a pipe must be refused"),
    }
    // open_fd took the read end and closed it on the error path
    assert!(!is_open(fds[0]), "refused fd must be closed");
    drop(wr);
}

#[test]
fn fd_probe_invalid() {
    let _g = serial();
    let p = probe_fd(-1);
    assert!(!p.seekable);
    assert_eq!(p.kind, FdKind::Other);
    assert_eq!(p.size_mb, 0.0);
}

#[test]
fn corpus_set_via_fd() {
    let _g = serial();
    let Some(path) = corpus("set.pdf") else { return };
    let expected = Doc::open(&path).unwrap().page_count();
    let doc = open_fd(open_ro(&path)).unwrap();
    assert_eq!(doc.page_count(), expected);
    eprintln!("set.pdf via fd: {} pages, {:.1} MB, open {:.1} ms", doc.page_count(), doc.probe().size_mb, doc.open_ms());
}
