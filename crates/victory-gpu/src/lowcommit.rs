//! Back-off while Windows commit is short. Ported from Pavl3000/cgbj-fork
//! (ab75ac3e, 18bdf793), adapted to this tree.
//!
//! * A host allocation that Windows refuses for lack of memory
//!   (`E_OUTOFMEMORY`) pauses the texture upload pass for 500 ms instead of
//!   asking again for every small texture.
//! * The pass is deferred while the commit still available is below a
//!   reserve (bake prefetch needs half of what look-ahead needs), so the
//!   game's own allocations are not starved.
//! * Speculative per-frame staging is capped while the host RAM band is
//!   Watch or High (`crate::host::sweep_band`). Bake passes are exempt.
//!
//! `VICTORY_LOWCOMMIT_BACKOFF=1` turns all of it on (off by default).
//! `VICTORY_LOWCOMMIT_RESERVE_MB` sets the look-ahead commit reserve (1024;
//! bakes use half). `VICTORY_LOWCOMMIT_STAGE_MB` sets the Watch-band staging
//! cap per frame (96; the High band is capped at 16).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_COMMIT_SAMPLE: AtomicU64 = AtomicU64::new(0);
static AVAILABLE_COMMIT: AtomicU64 = AtomicU64::new(u64::MAX);
static RETRY_UPLOAD_AT: AtomicU64 = AtomicU64::new(0);

pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_LOWCOMMIT_BACKOFF").is_some_and(|v| v == "1");
        tracing::info!(on, "low-commit back-off (off by default; VICTORY_LOWCOMMIT_BACKOFF=1 turns it on)");
        on
    })
}

fn reserve() -> u64 {
    static MB: OnceLock<u64> = OnceLock::new();
    *MB.get_or_init(|| {
        std::env::var("VICTORY_LOWCOMMIT_RESERVE_MB").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(1024)
    }) << 20
}

/// Commit still available to the system, in bytes.
fn available_commit() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    (unsafe { GlobalMemoryStatusEx(&mut m) } != 0).then_some(m.ullAvailPageFile)
}

/// A host allocation failed: an out-of-memory refusal pauses uploads.
pub(crate) fn allocation_error(error: &windows::core::Error) {
    if !enabled() || error.code() != windows::Win32::Foundation::E_OUTOFMEMORY {
        return;
    }
    let until = crate::host::now_ms().saturating_add(500);
    if RETRY_UPLOAD_AT.fetch_max(until, Ordering::Relaxed) == 0 {
        tracing::warn!(
            "low-commit back-off engaged: a host allocation was refused (out of memory); texture uploads pause 500 ms after each refusal"
        );
    }
}

/// Whether uploads are paused after a refused allocation.
#[allow(dead_code)]
pub(crate) fn upload_refused() -> bool {
    enabled() && crate::host::now_ms() < RETRY_UPLOAD_AT.load(Ordering::Relaxed)
}

fn has_upload_reserve(commit: u64, bake: bool, reserve: u64) -> bool {
    commit >= if bake { reserve / 2 } else { reserve }
}

/// Whether the texture pass may stage now. A bake pass (its inputs are
/// sampled at once) is held to half the reserve. Direct consumers bypass
/// this gate.
pub(crate) fn admit_texture_pass(bake: bool) -> bool {
    if !enabled() {
        return true;
    }
    let now = crate::host::now_ms();
    if now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) {
        return false;
    }
    let next = NEXT_COMMIT_SAMPLE.load(Ordering::Relaxed);
    if now >= next
        && NEXT_COMMIT_SAMPLE.compare_exchange(next, now.saturating_add(32), Ordering::Relaxed, Ordering::Relaxed).is_ok()
        && let Some(commit) = available_commit()
    {
        AVAILABLE_COMMIT.store(commit, Ordering::Relaxed);
    }
    let commit = AVAILABLE_COMMIT.load(Ordering::Relaxed);
    let admitted = has_upload_reserve(commit, bake, reserve());
    if !admitted {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(
                available_commit_mb = commit >> 20,
                bake,
                "low-commit back-off engaged: texture pass deferred, commit available is below the upload reserve"
            )
        });
    }
    admitted
}

fn stage_cap(bytes: u64, band: crate::host::RamBand, watch_cap: u64) -> u64 {
    match band {
        crate::host::RamBand::Calm => bytes,
        crate::host::RamBand::Watch => bytes.min(watch_cap),
        crate::host::RamBand::High => bytes.min(16 << 20),
    }
}

/// The frame's final speculative staging ceiling. `bytes` is the ceiling the
/// pass worked out (frame spend so far plus its allowance); the cap applies
/// to the allowance, so a single texture can still cross it.
pub(crate) fn prefetch_limit(bytes: u64, spent: u64) -> u64 {
    if !enabled() {
        return bytes;
    }
    static WATCH_MB: OnceLock<u64> = OnceLock::new();
    let watch = *WATCH_MB.get_or_init(|| {
        std::env::var("VICTORY_LOWCOMMIT_STAGE_MB").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(96)
    }) << 20;
    let band = crate::host::sweep_band();
    let allowance = bytes.saturating_sub(spent);
    let capped = stage_cap(allowance, band, watch);
    if capped < allowance {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(
                ?band,
                cap_mb = capped >> 20,
                "low-commit back-off engaged: speculative texture staging per frame is capped"
            )
        });
    }
    spent.saturating_add(capped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::RamBand;

    #[test]
    fn bake_gets_half_the_reserve() {
        let r = 1024 << 20;
        assert!(!has_upload_reserve(511 << 20, true, r));
        assert!(has_upload_reserve(512 << 20, true, r));
        assert!(!has_upload_reserve(512 << 20, false, r));
        assert!(has_upload_reserve(1024 << 20, false, r));
    }

    #[test]
    fn staging_is_capped_only_under_pressure() {
        let w = 96 << 20;
        assert_eq!(stage_cap(512 << 20, RamBand::Calm, w), 512 << 20);
        assert_eq!(stage_cap(512 << 20, RamBand::Watch, w), w);
        assert_eq!(stage_cap(512 << 20, RamBand::High, w), 16 << 20);
        assert_eq!(stage_cap(8 << 20, RamBand::High, w), 8 << 20);
    }
}
