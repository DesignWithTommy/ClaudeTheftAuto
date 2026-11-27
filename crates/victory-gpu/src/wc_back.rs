//! `VICTORY_SKIP_WC_BACK=1` (off by default), an EXPERIMENT: what shaders
//! wrote to a card copy of title memory the title reserved write-combining
//! is not fetched back into title memory. The title's CPU hardly reads
//! such memory (a census found 97 of 100 MB a frame fetched back from it),
//! so the fetch is skipped. Expect artifacts where the title's CPU writes
//! part of a piece the card wrote: the next upload sends the stale bytes
//! there. Only a declared range wholly inside one write-combining
//! reservation is skipped; any other range is fetched as before.
//!
//! Ported from martindevbr/VItoria-Spark; its `VICTORY_SKIP_WC_COMPARE`
//! (which skips the card's compare too) is not ported: a device removal was
//! reported with it.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// A reservation of `XMemVirtualAlloc`, as the title asked for it.
struct Reservation {
    len: usize,
    protect: u32,
}

static RESERVATIONS: Mutex<BTreeMap<usize, Reservation>> = Mutex::new(BTreeMap::new());

/// Pieces left out: count and bytes.
static SKIPPED: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// The console's write-combining protection bit.
const WRITE_COMBINE: u32 = 0x400;

fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_SKIP_WC_BACK").is_some_and(|v| v == "1");
        if on {
            tracing::warn!(
                "ARMED: M1LK_SKIP_WC_BACK=1 - shader writes to write-combining title memory are NOT copied back (experiment)"
            );
        }
        on
    })
}

/// The runtime reserved `[at, at + len)` for the title (`XMemVirtualAlloc`
/// at an address of its choice), with the title's own `protect` (before
/// the runtime cleans it). Kept only with the experiment on.
pub fn title_memory_reserved(at: usize, len: usize, _kind: u32, _flags: u64, protect: u32) {
    if !on() || at == 0 || len == 0 {
        return;
    }
    RESERVATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(at, Reservation { len, protect });
}

/// Every declared title range must lie wholly inside a single write-combining
/// reservation.
fn all_wc_in(reservations: &BTreeMap<usize, Reservation>, ranges: &[(usize, usize)]) -> bool {
    !ranges.is_empty()
        && ranges.iter().all(|&(from, to)| {
            // Seeking from the exclusive end also detects a new reservation
            // beginning inside the range. An allocation ending at `to` fits.
            from < to
                && reservations
                    .range(..to)
                    .next_back()
                    .is_some_and(|(&base, r)| {
                        base <= from
                            && r.protect & WRITE_COMBINE != 0
                            && base.checked_add(r.len).is_some_and(|end| to <= end)
                    })
        })
}

/// Whether the piece of `bytes` bytes of a copy whose declared range is
/// `declared` is left on the card. A mixed declaration keeps every piece, so
/// the normal read-back covers it in full.
pub(crate) fn skip_back(declared: (usize, usize), bytes: u64) -> bool {
    if !on() {
        return false;
    }
    let wc = all_wc_in(
        &RESERVATIONS.lock().unwrap_or_else(|e| e.into_inner()),
        &[declared],
    );
    if wc {
        let n = SKIPPED[0].fetch_add(1, Ordering::Relaxed) + 1;
        let b = SKIPPED[1].fetch_add(bytes, Ordering::Relaxed) + bytes;
        if n.is_power_of_two() {
            tracing::info!(
                "M1LK_SKIP_WC_BACK: {n} pieces ({} MB) of write-combining memory left on the card so far",
                b >> 20
            );
        }
    }
    wc
}

#[cfg(test)]
mod tests {
    use super::{Reservation, all_wc_in};
    use std::collections::BTreeMap;

    fn map(items: &[(usize, usize, u32)]) -> BTreeMap<usize, Reservation> {
        items
            .iter()
            .map(|&(at, len, protect)| (at, Reservation { len, protect }))
            .collect()
    }

    #[test]
    fn a_range_inside_one_write_combining_reservation_counts() {
        let m = map(&[(0x1000, 0x1000, 0x400 | 4)]);
        assert!(all_wc_in(&m, &[(0x1000, 0x2000)]));
        assert!(all_wc_in(&m, &[(0x1800, 0x1900)]));
    }

    #[test]
    fn mixed_or_spanning_ranges_do_not() {
        let m = map(&[(0x1000, 0x1000, 0x400 | 4), (0x2000, 0x1000, 4)]);
        assert!(!all_wc_in(&m, &[(0x1800, 0x2800)]));
        assert!(!all_wc_in(&m, &[(0x2000, 0x2800)]));
        assert!(!all_wc_in(&m, &[]));
        assert!(!all_wc_in(&m, &[(0x500, 0x1800)]));
    }
}
