//! CTA's replay census (PLAN.md "Order changed", A; ours). Before lists are
//! replayed, this measures how much of each frame's list stream repeats a
//! stream from an earlier frame. It works on the words thin recording writes
//! down (`VICTORY_THIN=1`), per 1024-word piece and per whole list. A piece
//! or list counts as a hit when the same words were seen in an earlier
//! frame. Raw words only: upload pointers and other per-frame values in the
//! stream make the count a floor, not a ceiling.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

/// Pieces, pieces seen before, words, words seen before, lists, lists seen
/// before, distinct keys kept.
static STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
static FRAME: AtomicU32 = AtomicU32::new(1);
/// Key -> the last frame it was seen in.
static SEEN: Mutex<Option<HashMap<u64, u32>>> = Mutex::new(None);
/// Keys dropped once not seen for this many frames.
const KEEP_FRAMES: u32 = 600;

/// A 64-bit hash of `words` (multiply-rotate, FxHash style: no extra crate).
pub(crate) fn hash(words: &[u64]) -> u64 {
    const K: u64 = 0x517c_c1b7_2722_0a95;
    let mut h = words.len() as u64;
    for &w in words {
        h = (h.rotate_left(5) ^ w).wrapping_mul(K);
    }
    h ^ (h >> 29)
}

/// Folds one piece's key into a list's running key.
pub(crate) fn fold(sum: u64, piece: u64) -> u64 {
    hash(&[sum, piece])
}

fn seen(key: u64) -> bool {
    let frame = FRAME.load(Relaxed);
    let mut map = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    match map.insert(key, frame) {
        Some(last) => last < frame,
        None => false,
    }
}

/// A piece of a list's words, handed over for translation; its key.
pub(crate) fn piece(words: &[u64]) -> u64 {
    let key = hash(words);
    let hit = seen(key);
    STATS[0].fetch_add(1, Relaxed);
    STATS[2].fetch_add(words.len() as u64, Relaxed);
    if hit {
        STATS[1].fetch_add(1, Relaxed);
        STATS[3].fetch_add(words.len() as u64, Relaxed);
    }
    key
}

/// A whole list, closed: `key` is its pieces' keys folded.
pub(crate) fn list(key: u64) {
    STATS[4].fetch_add(1, Relaxed);
    if seen(key ^ 0x9e37_79b9_7f4a_7c15) {
        STATS[5].fetch_add(1, Relaxed);
    }
}

/// A frame was presented.
pub(crate) fn frame() {
    let frame = FRAME.fetch_add(1, Relaxed) + 1;
    if frame % 120 == 0 {
        let mut map = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(map) = map.as_mut() {
            map.retain(|_, last| frame - *last < KEEP_FRAMES);
            STATS[6].store(map.len() as u64, Relaxed);
        }
    }
}

/// For the frame-rate line.
pub(crate) fn log(frames: u64) {
    if !crate::list::thin::wanted() {
        return;
    }
    let s: Vec<u64> = STATS[..6].iter().map(|s| s.swap(0, Relaxed)).collect();
    let pct = |a: u64, b: u64| if b == 0 { 0.0 } else { 100.0 * a as f64 / b as f64 };
    let f = frames.max(1) as f64;
    tracing::info!(
        target: "cta_mem",
        "cta replay census per frame: {:.0} lists, {:.0}% seen before; {:.0} pieces, {:.0}% seen before; {:.0} words, {:.0}% in pieces seen before; {} keys kept",
        s[4] as f64 / f,
        pct(s[5], s[4]),
        s[0] as f64 / f,
        pct(s[1], s[0]),
        s[2] as f64 / f,
        pct(s[3], s[2]),
        STATS[6].load(Relaxed),
    );
}
