//! CTA's home (ours, PLAN.md 2.1): title memory has one copy. Under
//! `CTA_MEM=1` a title buffer or window of title memory is not copied into
//! a host buffer of the PC's memory: the card reads the title's own pages.
//!
//! How: the title's pages are a section; a view of ours of the stretch the
//! buffer lies in is mapped and opened as a host heap
//! (`OpenExistingHeapFromAddress1`), and the buffer is placed in it. Our
//! own view, not the title's: a heap holds the pages of the view it was
//! opened on, so the title's views stay free to be remapped, guarded or
//! freed. Private memory (`XMemVirtualAlloc`) committed whole is opened
//! where it is ([`crate::inplace::place_part`]).
//!
//! Regions are cut to the buffer asked for, rounded to 2 MiB, so that
//! opening a heap never pulls untouched title pages into the PC's memory.
//! Heap opening, placing and the stretch lookup are cloned from Vice-Tay's
//! renderer port 1 (`crate::port`, `crate::inplace`, TaynanGT, GPL-3.0);
//! the policy here (every address, every buffer the card does not hold,
//! tight regions) is CTA's.
//!
//! What it assumes: as port 1, CPU steps of ours that read this memory at
//! submission read what the title's CPU wrote.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows_sys::Win32::System::Memory as mem;

use crate::inplace::{Placed, stretch};

const K64: u64 = 64 << 10;
const GRAIN: u64 = 2 << 20;

/// Whether title memory goes in place by CTA's rules.
pub(crate) fn on() -> bool {
    cta_mem::enabled()
}

struct Region {
    start: u64,
    end: u64,
    heap: ID3D12Heap,
}

// SAFETY: D3D12 heaps are free-threaded.
unsafe impl Send for Region {}

static REGIONS: Mutex<Vec<Region>> = Mutex::new(Vec::new());
static BOUNDS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Since the start: regions, their bytes, buffers and windows placed, their
/// bytes; refused: not one stretch and not private, not committed, by the
/// host; regions forgotten.
/// Then card copies fed from the title's pages, their bytes; lean card
/// copies (no CPU side at all), their bytes.
static STATS: [AtomicU64; 12] = [const { AtomicU64::new(0) }; 12];
/// Why windows and buffers were refused, by how the views cover them:
/// none (and not private memory whole), holes, scattered, a run the host
/// refused; then, of "none", private memory partly committed or of
/// another protection, and memory that is a view with nothing known.
static WHY: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

fn why(start: u64, len: u64) {
    use crate::inplace::Cover;
    let i = match crate::inplace::cover(start as usize, len as usize) {
        Cover::None => {
            let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a valid out pointer of the right size.
            let n = unsafe { mem::VirtualQuery(start as *const _, &mut info, size_of_val(&info)) };
            if n != 0 && info.Type == mem::MEM_PRIVATE {
                WHY[4].fetch_add(1, Ordering::Relaxed);
            } else {
                WHY[5].fetch_add(1, Ordering::Relaxed);
            }
            0
        }
        Cover::Holes => 1,
        Cover::Scattered => 2,
        Cover::Run(..) => 3,
    };
    WHY[i].fetch_add(1, Ordering::Relaxed);
}

/// Title memory `[start, start + len)` as the title's own pages, or `None`
/// for the caller's copy.
pub(crate) fn place_for(start: u64, len: u64, window: bool) -> Option<Placed> {
    if !on() || len == 0 {
        return None;
    }
    let host = crate::host::host()?;
    let placed = match place_in(&host.device, start, len) {
        Ok(p) => Some(p),
        Err(4) => {
            // Not a view: private memory committed whole, opened where it is.
            crate::inplace::place_part(&host.device, start, len).ok()
        }
        Err(_) => None,
    };
    match &placed {
        Some(_) => {
            crate::inplace::counted(window, len);
            STATS[2].fetch_add(1, Ordering::Relaxed);
            STATS[3].fetch_add(len, Ordering::Relaxed);
        }
        None => {
            STATS[4].fetch_add(1, Ordering::Relaxed);
            why(start, len);
        }
    }
    placed
}

/// The CPU side of a big buffer's copy in the card's memory as the title's
/// own pages (`crate::shadow` `lookup`): a buffer over title memory
/// `[start, start + len)` from its first byte (`start` on a 64 KiB line),
/// or `None` for a copy in the PC's memory as before. Nothing goes up from
/// a RAM copy: the ranges the title wrote are copied from these pages to
/// the card's side, and what the card wrote lands in them.
///
/// Prior art: Vilocity's `compat` sparse windows (voidopcode/vilocity,
/// GPL-3.0, `hle/d3d12x/sparse.rs`) keep the CPU's view as the title's
/// memory and send only what it wrote to VRAM; this does the same for
/// Vice-Tay's card copies.
pub(crate) fn stage_for(start: u64, len: u64) -> Option<Placed> {
    if !on() || len == 0 || start % K64 != 0 {
        return None;
    }
    let host = crate::host::host()?;
    let placed = match place_in(&host.device, start, len) {
        Ok(p) => Some(p),
        Err(4) => crate::inplace::place_part(&host.device, start, len).ok(),
        Err(_) => None,
    }
    .filter(|p| p.bias == 0)?;
    STATS[8].fetch_add(1, Ordering::Relaxed);
    STATS[9].fetch_add(len, Ordering::Relaxed);
    Some(placed)
}

/// CTA's lean card, for a big title buffer [`stage_for`] found no place
/// for: whether its copy in the card's memory is to have no CPU side at
/// all. Its CPU side is then the title's own memory, whoever's pages they
/// are, read where it is (a page not there reads as zeros); what changed
/// in it is found by the hashes of its 64-byte chunks and goes up through
/// each submission's own memory (`crate::host::sends_own`). Vice-Tay's
/// card copy kept a RAM copy of the whole buffer to compare with and to
/// send from. `CTA_LEAN=0` keeps that.
pub(crate) fn lean_for(start: u64, len: u64) -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let on = *ON.get_or_init(|| on() && std::env::var_os("CTA_LEAN").is_none_or(|v| v != "0") && crate::host::sends_own());
    if !on || len == 0 || !crate::texture::committed(start as *const u8, len as usize) {
        return false;
    }
    STATS[10].fetch_add(1, Ordering::Relaxed);
    STATS[11].fetch_add(len, Ordering::Relaxed);
    true
}

/// A [`lean_for`] that found no room on the card.
pub(crate) fn unlean(len: u64) {
    STATS[10].fetch_sub(1, Ordering::Relaxed);
    STATS[11].fetch_sub(len, Ordering::Relaxed);
}

/// A [`stage_for`] that went in place whole instead (no room on the card).
pub(crate) fn unstage(len: u64) {
    STATS[8].fetch_sub(1, Ordering::Relaxed);
    STATS[9].fetch_sub(len, Ordering::Relaxed);
    STATS[2].fetch_add(1, Ordering::Relaxed);
    STATS[3].fetch_add(len, Ordering::Relaxed);
}

/// `Err(4)`: not one stretch of a section; 5 not committed; 6 refused.
fn place_in(device: &ID3D12Device, start: u64, len: u64) -> Result<Placed, usize> {
    let at = start & !(K64 - 1);
    let end = (start + len.max(1)).next_multiple_of(K64);
    let mut regions = REGIONS.lock().unwrap_or_else(|e| e.into_inner());
    let i = match regions.iter().position(|r| r.start <= at && end <= r.end) {
        Some(i) => i,
        None => {
            let region = open_region(device, at, end)?;
            STATS[0].fetch_add(1, Ordering::Relaxed);
            STATS[1].fetch_add(region.end - region.start, Ordering::Relaxed);
            regions.push(region);
            bounds(&regions);
            regions.len() - 1
        }
    };
    let region = &regions[i];
    let buffer = crate::inplace::place(device, &region.heap, at - region.start, end - at).map_err(|_| 6usize)?;
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() } + (start - at);
    Ok(Placed { buffer, heap: region.heap.clone(), gpu, bias: start - at })
}

fn open_region(device: &ID3D12Device, at: u64, end: u64) -> Result<Region, usize> {
    let (first, bytes, section, offset) = stretch(at as usize, (end - at) as usize).ok_or(4usize)?;
    let (first, bytes) = (first as u64, bytes as u64);
    // Tight: the buffer rounded out to 2 MiB, kept inside the stretch.
    let lo = (at & !(GRAIN - 1)).max(first);
    let hi = end.next_multiple_of(GRAIN).min(first + bytes);
    let offset = offset + (lo - first);
    if offset % K64 != 0 {
        return Err(4);
    }
    let len = hi - lo;
    // SAFETY: the section of a view the title has, kept open while a view
    // of it is there; the range is inside it.
    let view = unsafe { mem::MapViewOfFile(section as _, mem::FILE_MAP_WRITE, (offset >> 32) as u32, offset as u32, len as usize) }.Value as usize;
    if view == 0 {
        STATS[6].fetch_add(1, Ordering::Relaxed);
        return Err(6);
    }
    let unmap = || {
        // SAFETY: our own view, nothing opened on it.
        unsafe { mem::UnmapViewOfFile(mem::MEMORY_MAPPED_VIEW_ADDRESS { Value: view as *mut _ }) };
    };
    if !committed(view, len as usize) {
        unmap();
        STATS[5].fetch_add(1, Ordering::Relaxed);
        return Err(5);
    }
    match crate::inplace::open(device, crate::inplace::From::Part(view, len as usize)) {
        Ok(heap) => Ok(Region { start: lo, end: hi, heap }),
        Err(_) => {
            unmap();
            STATS[6].fetch_add(1, Ordering::Relaxed);
            Err(6)
        }
    }
}

fn committed(at: usize, len: usize) -> bool {
    let mut here = at;
    while here < at + len {
        // SAFETY: plain data filled in by the query.
        let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        if unsafe { mem::VirtualQuery(here as *const _, &mut info, size_of_val(&info)) } == 0 || info.State != mem::MEM_COMMIT {
            return false;
        }
        here = (info.BaseAddress as usize).saturating_add(info.RegionSize);
    }
    true
}

fn bounds(regions: &[Region]) {
    BOUNDS[0].store(regions.iter().map(|r| r.start).min().unwrap_or(0), Ordering::Relaxed);
    BOUNDS[1].store(regions.iter().map(|r| r.end).max().unwrap_or(0), Ordering::Relaxed);
}

/// The title maps other pages at `[at, at + len)`: regions over it go.
pub(crate) fn forget(at: usize, len: usize) {
    let (at, end) = (at as u64, (at as u64).saturating_add(len as u64));
    if BOUNDS[1].load(Ordering::Relaxed) <= at || BOUNDS[0].load(Ordering::Relaxed) >= end {
        return;
    }
    let mut regions = REGIONS.lock().unwrap_or_else(|e| e.into_inner());
    let before = regions.len();
    regions.retain(|r| r.end <= at || r.start >= end);
    STATS[7].fetch_add((before - regions.len()) as u64, Ordering::Relaxed);
    bounds(&regions);
}

/// For the frame-rate line.
pub(crate) fn log() {
    if !on() {
        return;
    }
    let s: Vec<u64> = STATS.iter().map(|s| s.load(Ordering::Relaxed)).collect();
    tracing::info!(
        target: "cta_mem",
        "cta home: {} buffers and windows in place ({} MB, no host copy) over {} regions ({} MB opened); refused {}: {} not committed, {} by the host; {} regions forgotten; {} card copies fed from the title's pages ({} MB, no RAM copy); {} lean card copies ({} MB, no CPU side)",
        s[2], s[3] >> 20, s[0], s[1] >> 20, s[4], s[5], s[6], s[7], s[8], s[9] >> 20, s[10], s[11] >> 20,
    );
    let w: Vec<u64> = WHY.iter().map(|s| s.load(Ordering::Relaxed)).collect();
    tracing::info!(
        target: "cta_mem",
        "cta home refusals by cover: {} no view ({} private memory not whole or not plain, {} other), {} with holes, {} scattered over views, {} one run refused",
        w[0], w[4], w[5], w[1], w[2], w[3],
    );
}
