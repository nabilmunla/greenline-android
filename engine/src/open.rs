//! Opening a document from a file descriptor (task 2b.5, the Android SAF path).
//!
//! On Android the Kotlin side picks a PDF with `ACTION_OPEN_DOCUMENT`, opens it with
//! `ContentResolver.openFileDescriptor(uri, "r")` and hands the detached fd to Rust. MuPDF needs
//! random access, so the fd is first probed with `fstat` + `lseek(SEEK_END)` / `lseek(SEEK_SET, 0)`:
//! a pipe or socket (what some cloud providers hand out) is refused with [`NOT_SEEKABLE`]; there is
//! no copy fallback in 2b. A seekable fd is opened through its `/proc/self/fd/N` path.
//!
//! [`FdDoc`] owns both the document and the fd: the fd stays open while the document lives and is
//! closed right after the document is dropped.
//!
//! Confidentiality: nothing here logs. The `/proc/self/fd/N` path names a number, not the file, but
//! MuPDF error text is still never meant for logs or the UI (see [`crate::Error::Mupdf`]).

use std::ops::Deref;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use crate::{Doc, Error, Result};

/// The fixed error code ([`Error::Invalid`]) for an fd that cannot seek (pipe, socket, ...).
pub const NOT_SEEKABLE: &str = "E_FD_NOT_SEEKABLE";

/// What kind of file an fd refers to (`st_mode & S_IFMT`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdKind {
    Regular,
    Pipe,
    Socket,
    /// Anything else (character device, directory, ...) or an fd `fstat` rejects.
    Other,
}

impl FdKind {
    /// Lower-case name, for numbers-only reports.
    pub fn as_str(self) -> &'static str {
        match self {
            FdKind::Regular => "regular",
            FdKind::Pipe => "pipe",
            FdKind::Socket => "socket",
            FdKind::Other => "other",
        }
    }
}

/// Result of [`probe_fd`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FdProbe {
    /// `lseek(SEEK_END)` and the rewind `lseek(SEEK_SET, 0)` both succeeded.
    pub seekable: bool,
    pub kind: FdKind,
    /// Size in MiB: the `SEEK_END` offset when seekable, otherwise `st_size` (0 for pipes/sockets).
    pub size_mb: f64,
}

/// Probe an fd without taking it: `fstat` for the kind and size, then `lseek(SEEK_END)` and
/// `lseek(SEEK_SET, 0)` to see whether it seeks (leaving a seekable fd rewound to offset 0).
/// An fd `fstat` rejects (closed, invalid) reports kind `Other`, not seekable, size 0.
pub fn probe_fd(fd: RawFd) -> FdProbe {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return FdProbe { seekable: false, kind: FdKind::Other, size_mb: 0.0 };
    }
    let kind = match st.st_mode & libc::S_IFMT {
        libc::S_IFREG => FdKind::Regular,
        libc::S_IFIFO => FdKind::Pipe,
        libc::S_IFSOCK => FdKind::Socket,
        _ => FdKind::Other,
    };
    let end = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
    let seekable = end >= 0 && unsafe { libc::lseek(fd, 0, libc::SEEK_SET) } == 0;
    let bytes = if seekable { end as u64 } else { st.st_size.max(0) as u64 };
    FdProbe { seekable, kind, size_mb: bytes as f64 / (1024.0 * 1024.0) }
}

/// A document opened from an fd, together with that fd.
///
/// Field order matters: Rust drops fields in declaration order, so `doc` (and MuPDF's own handle on
/// the file) goes first and the fd is closed after it. Like [`Doc`], this is `!Send`.
pub struct FdDoc {
    doc: Doc,
    fd: OwnedFd,
    probe: FdProbe,
}

impl FdDoc {
    pub fn doc(&self) -> &Doc {
        &self.doc
    }

    /// The probe taken when the document was opened.
    pub fn probe(&self) -> FdProbe {
        self.probe
    }

    /// The fd's number (still owned by this `FdDoc`; closed when it drops).
    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl Deref for FdDoc {
    type Target = Doc;

    fn deref(&self) -> &Doc {
        &self.doc
    }
}

/// Take ownership of `fd`, probe it, refuse it with [`Error::Invalid`]`(`[`NOT_SEEKABLE`]`)` if it
/// cannot seek, else open `/proc/self/fd/N` with [`Doc::open`]. On any error the fd is closed (it is
/// dropped with the `OwnedFd`).
pub fn open_fd(fd: OwnedFd) -> Result<FdDoc> {
    let probe = probe_fd(fd.as_raw_fd());
    if !probe.seekable {
        return Err(Error::Invalid(NOT_SEEKABLE));
    }
    let doc = Doc::open(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
    Ok(FdDoc { doc, fd, probe })
}
