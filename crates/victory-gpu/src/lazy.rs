//! `VICTORY_LAZY_COMMIT=1` (off by default): physical pages the title maps
//! into its reservations are left as reserved address space and committed
//! on first use, 64 KiB at a time.
//!
//! Adapted from xcl-dev/xcl crates/xcl/src/hle/kernelx/lazy.rs @06f90c19
//! (GPL-3.0-or-later): the idea (map fresh physical pages as reservations,
//! commit a page in the access-violation handler, `ensure` before kernel
//! writes) and the shape of the page set. Reworked for this runtime:
//!
//! - Pages are sections' 64 KiB slices, and one slice may be shown at many
//!   addresses (`XMemMapPhysicalPages`, the GPU's page-mapping copies): a
//!   commit through one address commits the slice for all of them, so every
//!   lazy address carries its slice ([`Key`]) and all addresses of a slice
//!   leave the set together. Slices known committed ([`State::done`]) are
//!   committed at once when mapped again, so no committed page is ever in
//!   the set (a commit over a committed page could drop the GPU side's
//!   read-only guard on it).
//! - No handler of its own: the GPU side's write-guard handler
//!   (`crate::watch`) asks [`on_fault`] first, so the two never race.
//! - Kernel writes into title memory already pass `crate::watch::writing`
//!   (file reads, DirectStorage, the GPU side's copy-back through
//!   `NtWriteVirtualMemory`): it calls [`ensure`], as do the in-place heap
//!   openers, which need committed pages.
//!
//! What the title never touches (render targets and depth buffers, which
//! live on the card here; pools it maps and leaves zero) costs no commit.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{LazyLock, RwLock};

/// The size of a physical page and of a lazy commit.
pub const PAGE: usize = 64 << 10;

/// A section's slice: the section handle and the slice's index in it.
type Key = (usize, usize);

/// The lazy pages and the slices behind them.
#[derive(Default)]
struct State {
    /// Lazy page (64 KiB aligned address) -> its slice.
    pages: BTreeMap<usize, Key>,
    /// Slice -> its lazy addresses.
    by_key: HashMap<Key, Vec<usize>>,
    /// Slices committed through some address (or a view of the runtime's).
    done: HashSet<Key>,
    /// Addresses committed by [`State::take`] and still mapped: a thread
    /// that faulted on one while another committed it goes on.
    committed: HashSet<usize>,
}

impl State {
    /// Notes `page` lazy, showing `key`; false (nothing noted) if the slice
    /// is committed already.
    fn mark(&mut self, page: usize, key: Key) -> bool {
        if self.done.contains(&key) {
            return false;
        }
        if let Some(old) = self.pages.insert(page, key) {
            self.drop_alias(old, page);
        }
        self.committed.remove(&page);
        self.by_key.entry(key).or_default().push(page);
        true
    }

    fn drop_alias(&mut self, key: Key, page: usize) {
        if let Some(v) = self.by_key.get_mut(&key) {
            v.retain(|&p| p != page);
            if v.is_empty() {
                self.by_key.remove(&key);
            }
        }
    }

    /// Takes `page` out of the set, with every other address of its slice
    /// (committed together), and notes the slice committed.
    fn take(&mut self, page: usize) -> Option<Key> {
        let key = self.pages.remove(&page)?;
        for alias in self.by_key.remove(&key).unwrap_or_default() {
            self.pages.remove(&alias);
            self.committed.insert(alias);
        }
        self.committed.insert(page);
        self.done.insert(key);
        Some(key)
    }

    /// `[at, at + len)` is unmapped: nothing there is lazy any more.
    fn unmark(&mut self, at: usize, len: usize) {
        let end = at.saturating_add(len);
        let gone: Vec<(usize, Key)> = self.pages.range(at & !(PAGE - 1)..end).map(|(&p, &k)| (p, k)).collect();
        for (page, key) in gone {
            self.pages.remove(&page);
            self.drop_alias(key, page);
        }
        self.committed.retain(|&p| p < at & !(PAGE - 1) || p >= end);
    }

    /// The runtime committed slices `[index, index + n)` of `section`
    /// through a view of its own.
    fn section_committed(&mut self, section: usize, index: usize, n: usize) {
        for i in index..index + n {
            let key = (section, i);
            for alias in self.by_key.remove(&key).unwrap_or_default() {
                self.pages.remove(&alias);
                self.committed.insert(alias);
            }
            self.done.insert(key);
        }
    }

    /// `section` is closed; its handle value may name another later.
    fn section_gone(&mut self, section: usize) {
        self.done.retain(|k| k.0 != section);
        let gone: Vec<Key> = self.by_key.keys().filter(|k| k.0 == section).copied().collect();
        for key in gone {
            for page in self.by_key.remove(&key).unwrap_or_default() {
                self.pages.remove(&page);
            }
        }
    }

    /// The lazy pages that meet `[at, at + len)`.
    fn lazy_in(&self, at: usize, len: usize) -> Vec<usize> {
        self.pages.range(at & !(PAGE - 1)..at.saturating_add(len)).map(|(&p, _)| p).collect()
    }

    /// Bytes of `[start, end)` on lazy pages.
    fn lazy_bytes(&self, start: usize, end: usize) -> usize {
        self.pages
            .range(start & !(PAGE - 1)..end)
            .map(|(&p, _)| end.min(p + PAGE).saturating_sub(start.max(p)))
            .sum()
    }
}

static STATE: LazyLock<RwLock<State>> = LazyLock::new(Default::default);
/// Lazy pages now (the fault path's quick no).
static LAZY: AtomicUsize = AtomicUsize::new(0);
/// Bytes left reserved at map time since the start; bytes committed lazily
/// by a fault, and by `ensure`; faults taken.
static MARKED: AtomicU64 = AtomicU64::new(0);
static FAULTED: AtomicU64 = AtomicU64::new(0);
static ENSURED: AtomicU64 = AtomicU64::new(0);
static FAULTS: AtomicU64 = AtomicU64::new(0);

fn state_read() -> std::sync::RwLockReadGuard<'static, State> {
    STATE.read().unwrap_or_else(|e| e.into_inner())
}

fn state_write() -> std::sync::RwLockWriteGuard<'static, State> {
    STATE.write().unwrap_or_else(|e| e.into_inner())
}

/// Whether physical pages are mapped lazily (`VICTORY_LAZY_COMMIT=1`).
pub fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var("VICTORY_LAZY_COMMIT").is_ok_and(|v| v.trim() == "1");
        if on {
            tracing::info!("VICTORY_LAZY_COMMIT: physical pages are committed on first use (64 KiB at a time)");
        }
        on
    })
}

fn commit_page(page: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, PAGE_READWRITE, VirtualAlloc};
    // SAFETY: a 64 KiB page of a view of a reserved section, still mapped
    // (unmapping takes it out of the set first, under the same lock).
    !unsafe { VirtualAlloc(page as *const _, PAGE, MEM_COMMIT, PAGE_READWRITE) }.is_null()
}

/// Commits the lazy page at `page` (and with it its slice's other
/// addresses); false if it is not lazy or the host refuses.
fn commit_lazy(state: &mut State, page: usize) -> bool {
    let Some(key) = state.take(page) else {
        return false;
    };
    if commit_page(page) {
        return true;
    }
    // Back in the set: the slice is not committed after all.
    state.done.remove(&key);
    state.committed.remove(&page);
    state.mark(page, key);
    false
}

fn recount(state: &State) {
    LAZY.store(state.pages.len(), Relaxed);
}

/// The runtime mapped `len` bytes at `at` as a view of `section` from slice
/// `index` on (64 KiB each), not committed: each page is committed on first
/// use, except slices committed before, which are committed now. False if
/// that commit fails (the caller fails the mapping).
pub fn mark_view(at: usize, len: usize, section: usize, index: usize) -> bool {
    crate::watch::install();
    let mut eager = Vec::new();
    let marked;
    {
        let mut s = state_write();
        for k in 0..len / PAGE {
            let page = at + k * PAGE;
            if !s.mark(page, (section, index + k)) {
                eager.push(page);
            }
        }
        marked = (len / PAGE - eager.len()) * PAGE;
        recount(&s);
    }
    for run in eager.chunk_by(|a, b| a + PAGE == *b) {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, PAGE_READWRITE, VirtualAlloc};
        // SAFETY: pages of the view just mapped, whose slices are committed.
        if unsafe { VirtualAlloc(run[0] as *const _, run.len() * PAGE, MEM_COMMIT, PAGE_READWRITE) }.is_null() {
            state_write().unmark(at, len);
            return false;
        }
    }
    let before = MARKED.fetch_add(marked as u64, Relaxed);
    let after = before + marked as u64;
    // One line as the total left reserved passes each power of two from 256 MB.
    if after >> 28 != 0 && (after >> 20).ilog2() != ((before >> 20).max(1)).ilog2() {
        tracing::info!(
            "VICTORY_LAZY_COMMIT: {} MB of physical pages mapped as reservation only so far ({} MB lazy now)",
            after >> 20,
            (LAZY.load(Relaxed) * PAGE) >> 20
        );
    }
    true
}

/// `[at, at + len)` is unmapped (or mapped again): nothing there is lazy.
pub fn unmark(at: usize, len: usize) {
    if LAZY.load(Relaxed) == 0 && state_read().committed.is_empty() {
        return;
    }
    let mut s = state_write();
    s.unmark(at, len);
    recount(&s);
}

/// The runtime committed slices `[index, index + n)` of `section` through
/// a view of its own (DirectStorage writes).
pub fn section_committed(section: usize, index: usize, n: usize) {
    if !on() {
        return;
    }
    let mut s = state_write();
    s.section_committed(section, index, n);
    recount(&s);
}

/// `section`'s handle is closed.
pub fn section_gone(section: usize) {
    if !on() {
        return;
    }
    let mut s = state_write();
    s.section_gone(section);
    recount(&s);
}

/// Commits the lazy pages of `[at, at + len)`: memory the kernel or the
/// runtime writes next (a kernel write fails on a reservation; it does not
/// fault into the handler).
pub fn ensure(at: usize, len: usize) {
    if len == 0 || LAZY.load(Relaxed) == 0 {
        return;
    }
    let pages = state_read().lazy_in(at, len);
    if pages.is_empty() {
        return;
    }
    let mut s = state_write();
    let mut n = 0u64;
    for page in pages {
        if commit_lazy(&mut s, page) {
            n += 1;
        }
    }
    recount(&s);
    ENSURED.fetch_add(n * PAGE as u64, Relaxed);
}

/// From the access-violation handler (`crate::watch`): true if the access
/// at `at` met a lazy page, now committed (the access goes on).
/// `access` is the record's first parameter: 0 read, 1 write, 8 execute.
pub(crate) fn on_fault(access: usize, at: usize) -> bool {
    if access == 8 || LAZY.load(Relaxed) == 0 && !on() {
        return false;
    }
    let page = at & !(PAGE - 1);
    {
        let s = state_read();
        let lazy = s.pages.contains_key(&page);
        if !lazy {
            // Committed by another thread since this one faulted: a read
            // goes on (a write may meet a guard: the caller decides).
            return access == 0 && s.committed.contains(&page);
        }
    }
    let mut s = state_write();
    let done = if s.pages.contains_key(&page) {
        commit_lazy(&mut s, page)
    } else {
        access == 0 && s.committed.contains(&page)
    };
    recount(&s);
    drop(s);
    if done {
        FAULTS.fetch_add(1, Relaxed);
        FAULTED.fetch_add(PAGE as u64, Relaxed);
    }
    done
}

/// Whether `at` is on a lazy page (reads as zeros, committed on use).
pub fn is_lazy(at: usize) -> bool {
    LAZY.load(Relaxed) != 0 && state_read().pages.contains_key(&(at & !(PAGE - 1)))
}

/// Bytes of `[start, end)` on lazy pages.
pub(crate) fn lazy_bytes(start: usize, end: usize) -> usize {
    if LAZY.load(Relaxed) == 0 || end <= start {
        return 0;
    }
    state_read().lazy_bytes(start, end)
}

/// One line for the periodic memory report.
#[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
pub(crate) fn log_stats() {
    if !on() {
        return;
    }
    tracing::info!(
        "VICTORY_LAZY_COMMIT: {} MB lazy now (never touched); since the start {} MB left reserved at map time, {} MB committed by {} faults, {} MB before kernel/runtime writes",
        (LAZY.load(Relaxed) * PAGE) >> 20,
        MARKED.load(Relaxed) >> 20,
        FAULTED.load(Relaxed) >> 20,
        FAULTS.load(Relaxed),
        ENSURED.load(Relaxed) >> 20,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: usize = 0x1234;

    #[test]
    fn a_slice_leaves_with_all_its_addresses() {
        let mut s = State::default();
        assert!(s.mark(0x10_0000, (S, 3)));
        assert!(s.mark(0x20_0000, (S, 3)));
        assert!(s.mark(0x10_0000 + PAGE, (S, 4)));
        assert_eq!(s.take(0x20_0000), Some((S, 3)));
        assert!(!s.pages.contains_key(&0x10_0000));
        assert!(s.committed.contains(&0x10_0000) && s.committed.contains(&0x20_0000));
        assert!(s.pages.contains_key(&(0x10_0000 + PAGE)));
        // Committed now: mapped again, it is not lazy.
        assert!(!s.mark(0x30_0000, (S, 3)));
        assert_eq!(s.take(0x30_0000), None);
    }

    #[test]
    fn ranges_find_and_count_lazy_pages() {
        let mut s = State::default();
        for k in 0..4 {
            s.mark(0x40_0000 + k * PAGE, (S, k));
        }
        assert_eq!(s.lazy_in(0x40_0000 + PAGE + 5, 1), vec![0x40_0000 + PAGE]);
        assert_eq!(s.lazy_in(0x40_0000 + PAGE - 1, 2), vec![0x40_0000, 0x40_0000 + PAGE]);
        assert_eq!(s.lazy_in(0x40_0000, usize::MAX).len(), 4);
        assert_eq!(s.lazy_bytes(0x40_0000 + 16, 0x40_0000 + PAGE + 16), PAGE);
        assert_eq!(s.lazy_bytes(0x3f_0000, 0x40_0000), 0);
        s.unmark(0x40_0000 + PAGE, 2 * PAGE);
        assert_eq!(s.lazy_in(0x40_0000, 4 * PAGE), vec![0x40_0000, 0x40_0000 + 3 * PAGE]);
        assert!(!s.by_key.contains_key(&(S, 1)));
    }

    #[test]
    fn runtime_commits_and_closed_sections_are_forgotten() {
        let mut s = State::default();
        s.mark(0x50_0000, (S, 0));
        s.mark(0x50_0000 + PAGE, (S, 1));
        s.section_committed(S, 1, 1);
        assert!(!s.pages.contains_key(&(0x50_0000 + PAGE)));
        assert!(s.done.contains(&(S, 1)));
        s.section_gone(S);
        assert!(s.pages.is_empty() && s.by_key.is_empty() && s.done.is_empty());
        // The handle value names a new section: its slices are fresh.
        assert!(s.mark(0x60_0000, (S, 1)));
    }

    /// A lazy page reads as zeros on first touch and keeps what is written;
    /// `ensure` commits without a fault. Uses the real handler.
    #[test]
    fn a_lazy_view_commits_on_first_use() {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE, SEC_RESERVE, UnmapViewOfFile,
        };
        // SAFETY: a section and a view of our own, unmapped at the end.
        unsafe {
            let section = CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE | SEC_RESERVE, 0, (2 * PAGE) as u32, std::ptr::null());
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * PAGE);
            let at = view.Value as usize;
            assert_ne!(at, 0);
            assert!(mark_view(at, 2 * PAGE, section as usize, 0));
            assert!(is_lazy(at) && is_lazy(at + PAGE));
            assert_eq!(std::ptr::read_volatile((at + 8) as *const u32), 0);
            std::ptr::write_volatile((at + 8) as *mut u32, 7);
            assert_eq!(std::ptr::read_volatile((at + 8) as *const u32), 7);
            assert!(!is_lazy(at));
            ensure(at + PAGE + 4, 4);
            assert!(!is_lazy(at + PAGE));
            std::ptr::write_volatile((at + PAGE) as *mut u32, 9);
            unmark(at, 2 * PAGE);
            section_gone(section as usize);
            UnmapViewOfFile(view);
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
    }
}
