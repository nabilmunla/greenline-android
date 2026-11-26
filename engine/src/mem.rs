//! Process memory helpers (Linux and Android). All figures are in MB (1 MB = 1048576 bytes).
//!
//! Every function returns 0.0 (or `false`) if /proc is unreadable; none of them panics.
//! The page size always comes from `sysconf(_SC_PAGESIZE)`: Android 15+ devices may use 16 KB pages.

const MB: f64 = 1_048_576.0;

/// The kernel page size in bytes, from `sysconf(_SC_PAGESIZE)`. 0 if sysconf fails.
pub fn page_size() -> usize {
    let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if v > 0 {
        v as usize
    } else {
        0
    }
}

/// Resident set size now, from /proc/self/statm (field 2, in pages) times the page size.
pub fn rss_mb() -> f64 {
    let pages: f64 = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
        .unwrap_or(0.0);
    pages * page_size() as f64 / MB
}

/// Peak resident set size (high-water mark): VmHWM from /proc/self/status, falling back to
/// `getrusage` ru_maxrss (kilobytes on Linux and Android) when /proc has no VmHWM line.
pub fn peak_mb() -> f64 {
    if let Some(kb) = vm_hwm_kb() {
        return kb as f64 * 1024.0 / MB;
    }
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) } != 0 {
        return 0.0;
    }
    u.ru_maxrss as f64 / 1024.0
}

fn vm_hwm_kb() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = s.lines().find(|l| l.starts_with("VmHWM:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Ask the C allocator to give free memory back to the kernel. Returns true if a trim call was made.
///
/// - Linux with glibc: `malloc_trim(0)`.
/// - Android (bionic has no `malloc_trim`): `mallopt(M_PURGE, 0)`, which makes scudo release free pages.
/// - Anything else (musl, macOS, Windows): does nothing and returns false.
pub fn trim() -> bool {
    trim_impl()
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn trim_impl() -> bool {
    unsafe { libc::malloc_trim(0) };
    true
}

#[cfg(target_os = "android")]
fn trim_impl() -> bool {
    extern "C" {
        fn mallopt(param: libc::c_int, value: libc::c_int) -> libc::c_int;
    }
    const M_PURGE: libc::c_int = -101; // <malloc.h>, bionic
    unsafe { mallopt(M_PURGE, 0) };
    true
}

#[cfg(not(any(all(target_os = "linux", target_env = "gnu"), target_os = "android")))]
fn trim_impl() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_size_is_sane() {
        let p = page_size();
        assert!(p >= 4096 && p.is_power_of_two(), "page size {p}");
    }

    #[test]
    fn rss_and_peak_are_plausible() {
        let r = rss_mb();
        let p = peak_mb();
        assert!(r > 0.5 && r < 1_000_000.0, "rss {r}");
        assert!(p > 0.5 && p < 1_000_000.0, "peak {p}");
        assert!(p + 1.0 >= r, "peak {p} below rss {r}");
    }
}

/// Reset the peak (VmHWM) to the current RSS, so a measurement run reports its own peak instead of the
/// process's lifetime peak (bug sweep BUG-07). Linux and Android: writes 5 to /proc/self/clear_refs.
/// False if the kernel refused.
pub fn reset_peak() -> bool {
    std::fs::write("/proc/self/clear_refs", "5").is_ok()
}
