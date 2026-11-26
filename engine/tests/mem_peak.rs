//! DS.5 / bug sweep BUG-07: a measurement run's peak must be its own, not the process's lifetime peak.
use greenline_engine::mem;

#[test]
fn peak_can_be_reset_for_a_new_run() {
    let mut big = vec![0u8; 400 << 20];
    for i in (0..big.len()).step_by(4096) {
        big[i] = 1;
    }
    let big = std::hint::black_box(big);
    let high = mem::peak_mb();
    drop(big);
    mem::trim();
    assert!(high - mem::rss_mb() > 200.0, "setup: peak {high:.0} MB, rss {:.0} MB", mem::rss_mb());
    assert!(mem::reset_peak(), "the kernel refused to reset the peak");
    let after = mem::peak_mb();
    eprintln!("  peak {high:.0} MB before reset, {after:.0} MB after (rss {:.0} MB)", mem::rss_mb());
    assert!(after < high - 200.0, "peak not reset: {after:.0} MB");
}
