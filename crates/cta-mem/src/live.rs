//! The block map for the running title: the GPU layer reports each
//! submission's read and write sets and each CPU write it learns of, and
//! every presented frame ends a frame here. Once a second a `cta-mem` line
//! goes to the log: blocks per tier, card bytes, moves, and how many blocks
//! the CPU and GPU touched (the census of PLAN.md step 2: G4 sizing and the
//! R5/R7 counters).

use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Instant;

use crate::blocks::{Blocks, Rules, Tier};
use crate::BLOCK;

struct Live {
    blocks: Blocks,
    since: Instant,
    frames: u64,
    /// Summed over the frames since the last line.
    promoted: u64,
    demoted: u64,
    owned: u64,
    cooled: u64,
    refused: u64,
    read_blocks: u64,
    stale_reads: u64,
    cpu_blocks: u64,
    gpu_blocks: u64,
}

static LIVE: LazyLock<Mutex<Live>> = LazyLock::new(|| {
    Mutex::new(Live {
        blocks: Blocks::new(Rules::default()),
        since: Instant::now(),
        frames: 0,
        promoted: 0,
        demoted: 0,
        owned: 0,
        cooled: 0,
        refused: 0,
        read_blocks: 0,
        stale_reads: 0,
        cpu_blocks: 0,
        gpu_blocks: 0,
    })
});

fn live() -> MutexGuard<'static, Live> {
    LIVE.lock().unwrap_or_else(|e| e.into_inner())
}

fn count(at: u64, len: u64) -> u64 {
    let last = if len == 0 { at } else { at + len - 1 };
    last / BLOCK - at / BLOCK + 1
}

/// A submission's shaders read `[at, at + len)` of title memory.
pub fn gpu_read(at: u64, len: u64) {
    let mut l = live();
    l.read_blocks += count(at, len);
    let stale = l.blocks.gpu_read(at, len);
    l.stale_reads += stale.len() as u64;
    // No transport yet: count the upload as done so a stale block is
    // counted once per CPU write, as a real upload would be.
    for b in stale {
        l.blocks.uploaded(b);
    }
}

/// A submission's shaders write `[at, at + len)` of title memory.
pub fn gpu_write(at: u64, len: u64) {
    let mut l = live();
    l.gpu_blocks += count(at, len);
    l.blocks.gpu_write(at, len);
}

/// The CPU wrote `[at, at + len)` (write watch or a guard).
pub fn cpu_write(at: u64, len: u64) {
    let mut l = live();
    l.cpu_blocks += count(at, len);
    l.blocks.cpu_write(at, len);
}

/// A frame was presented: tiers move, and once a second the line is logged.
pub fn frame() {
    let mut l = live();
    let m = l.blocks.end_frame();
    l.frames += 1;
    l.promoted += m.promoted.len() as u64;
    l.demoted += m.demoted.len() as u64;
    l.owned += m.owned.len() as u64;
    l.cooled += m.cooled.len() as u64;
    l.refused += m.refused as u64;
    if l.since.elapsed().as_secs_f64() < 1.0 {
        return;
    }
    let mut tiers = [0u64; 4];
    for t in l.blocks.tiers() {
        tiers[match t {
            Tier::InPlace => 0,
            Tier::Cached => 1,
            Tier::GpuOwned => 2,
            Tier::Cold => 3,
        }] += 1;
    }
    let f = l.frames.max(1) as f64;
    let mb = |blocks: u64| blocks * BLOCK >> 20;
    tracing::info!(
        target: "cta_mem",
        "cta-mem: blocks in place {} MB, cached {} MB, GPU-owned {} MB, cold {} MB; card {} MB of {} MB; \
         per frame: GPU read {:.0} blocks ({:.1} stale), GPU wrote {:.0}, CPU wrote {:.0}; \
         promoted {:.1}, demoted {:.1}, owned {:.1}, cooled {:.1}, refused {:.1}",
        mb(tiers[0]), mb(tiers[1]), mb(tiers[2]), mb(tiers[3]),
        l.blocks.on_card() >> 20, l.blocks.rules.card_budget >> 20,
        l.read_blocks as f64 / f, l.stale_reads as f64 / f, l.gpu_blocks as f64 / f, l.cpu_blocks as f64 / f,
        l.promoted as f64 / f, l.demoted as f64 / f, l.owned as f64 / f, l.cooled as f64 / f, l.refused as f64 / f,
    );
    l.since = Instant::now();
    l.frames = 0;
    l.promoted = 0;
    l.demoted = 0;
    l.owned = 0;
    l.cooled = 0;
    l.refused = 0;
    l.read_blocks = 0;
    l.stale_reads = 0;
    l.cpu_blocks = 0;
    l.gpu_blocks = 0;
}
