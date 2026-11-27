//! Where the frame line's "copies" milliseconds go (`VICTORY_COPY_PROF=1`,
//! diagnostics builds only; off: one read of a static per site, no clock).
//!
//! Each slot counts calls, microseconds and bytes since the last frame
//! line; [`log`] prints them per frame once a second. Slots nest as noted
//! in [`Slot`], so they do not sum to the frame line's copies.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Whether the profile is on. Read once.
#[inline]
pub(crate) fn on() -> bool {
    if !cfg!(feature = "diagnostics") {
        return false;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_COPY_PROF").is_some_and(|v| v == "1"))
}

/// The parts of a submission's copying. "In" means inside the slot above.
#[derive(Clone, Copy)]
pub(crate) enum Slot {
    /// `Step::Copy`: a title buffer copy at submission, whole step (its
    /// `title_written` flush and journal note are counted in here too).
    StepCopy,
    /// `Step::Copy` in: `watch::wrote` (journal lock + page inserts).
    StepWrote,
    /// `Step::Fill/Fills`: upload fills.
    StepFill,
    /// `Batch::title_written` (flush before a title write), whole.
    TitleWritten,
    /// `Batch::flush` in: `watch::ask_ahead` (sort, GetWriteWatch).
    AskAhead,
    /// `Batch::flush` in: `nothing_new` quiet-refresh filter.
    Quiet,
    /// `Batch::update`: one host copy brought up to date, whole.
    Update,
    /// `Update` in: `watch::writes` (journal answer or GetWriteWatch).
    Writes,
    /// `Writes` in: waiting for the journal lock.
    JournalLock,
    /// `Update` in: `copy_stale` page walk (minus the copies it makes).
    StaleWalk,
    /// `Update` in: `Shadow::copy` calls (title memory into the copy).
    Copy,
    /// `Copy` in: `watch::writing` guard open (title destinations only).
    GuardOpen,
    /// `Copy` in: `ReadProcessMemory` bulk read.
    Read,
    /// `Copy` in: fallback after a failed bulk read (sparse/pieces).
    Fallback,
    /// `Copy` in: `changed` and `card_changed` notes.
    Notes,
    /// Whole-buffer copy of memory that is not write-watched.
    Whole,
    /// `Step::GpuWrite`: `flush_one` and `Shadow::writing`.
    GpuWrite,
    /// Not in "copies": the submission's one `ask_once` asking.
    AskOnce,
    /// `Copy` in `Read`: reads that took the in-place path
    /// (`VICTORY_COPY_PLAIN=1`); the rest of `Read` went through the kernel.
    ReadPlain,
    /// GetWriteWatch calls that failed (memory not write-watched, or a
    /// merged range across reservations), wherever made; inside `writes`,
    /// `ask_ahead` or `ask_once*`.
    #[cfg_attr(not(feature = "diagnostics"), allow(dead_code))] // only built by diagnostics builds
    WatchFailed,
}

const NAMES: [&str; 20] = [
    "step_copy",
    " step_wrote",
    "step_fill",
    "title_written",
    " ask_ahead",
    " quiet_filter",
    "update",
    " writes",
    "  journal_lock",
    " stale_walk",
    " copy",
    "  guard_open",
    "  read_rpm",
    "  fallback",
    "  notes",
    " whole_copy",
    "gpu_write",
    "ask_once*",
    "   read_plain",
    " watch_failed",
];

const N: usize = NAMES.len();
static CELLS: [[AtomicU64; 3]; N] = [const { [const { AtomicU64::new(0) }; 3] }; N];

/// A running timer; free when the profile is off.
pub(crate) struct Timer(Option<Instant>);

#[inline]
pub(crate) fn start() -> Timer {
    Timer(on().then(Instant::now))
}

impl Timer {
    /// Adds one call of `slot` with `bytes` bytes.
    #[inline]
    pub(crate) fn end(self, slot: Slot, bytes: usize) {
        if let Some(t) = self.0 {
            let c = &CELLS[slot as usize];
            c[0].fetch_add(1, Ordering::Relaxed);
            c[1].fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
            c[2].fetch_add(bytes as u64, Ordering::Relaxed);
        }
    }
}

/// A failed GetWriteWatch call that took `took`.
#[cfg_attr(not(feature = "diagnostics"), allow(dead_code))] // only called by diagnostics builds
pub(crate) fn failed(took: std::time::Duration) {
    if on() {
        let c = &CELLS[Slot::WatchFailed as usize];
        c[0].fetch_add(1, Ordering::Relaxed);
        c[1].fetch_add(took.as_micros() as u64, Ordering::Relaxed);
    }
}

/// Once a second, from the frame line: per frame, `name ms (calls, MB)`.
pub(crate) fn log(frames: f64) {
    if !on() {
        return;
    }
    let mut line = String::new();
    for (i, name) in NAMES.iter().enumerate() {
        let c = &CELLS[i];
        let (n, us, bytes) = (c[0].swap(0, Ordering::Relaxed), c[1].swap(0, Ordering::Relaxed), c[2].swap(0, Ordering::Relaxed));
        if n == 0 {
            continue;
        }
        line.push_str(&format!(
            "{name} {:.1} ms ({:.0} calls {:.1} MB); ",
            us as f64 / 1e3 / frames,
            n as f64 / frames,
            bytes as f64 / 1e6 / frames
        ));
    }
    if let Some(reads) = crate::plaincopy::profile_line(frames) {
        line.push_str(&reads);
        line.push_str("; ");
    }
    tracing::info!("copy profile ms/frame: {line}(indented parts are inside the line above; * is not in copies)");
}
