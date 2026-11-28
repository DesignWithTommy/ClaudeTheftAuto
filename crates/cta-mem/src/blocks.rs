//! Per-64 KiB block placement (PLAN.md 2.1, "Tiers per 64 KiB block").
//!
//! Each block the GPU or CPU touches has a [`Block`]: a tier, an epoch that
//! moves on every CPU write, and decayed use counters. Callers feed it read
//! and write sets ([`Blocks::gpu_read`], [`Blocks::gpu_write`],
//! [`Blocks::cpu_write`]); [`Blocks::end_frame`] then moves blocks between
//! tiers under the card budget. Nothing here scans memory.

use std::collections::HashMap;

use crate::BLOCK;

/// Where the GPU reads a block from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// The home, over PCIe: no copy, no guard. Read-once and CPU-streamed data.
    InPlace,
    /// A card copy with a valid bit; re-uploaded only when the CPU wrote it.
    Cached,
    /// Written by the GPU: lives on the card; the home is brought up to date
    /// on demand.
    GpuOwned,
    /// No use for a while: no card copy.
    Cold,
}

#[derive(Clone, Copy, Debug)]
pub struct Block {
    pub tier: Tier,
    /// Moves on every CPU write; a card copy is valid while its epoch matches.
    pub epoch: u32,
    /// The epoch the card copy holds (`Cached`), if any.
    pub card_epoch: Option<u32>,
    /// GPU reads this frame and decayed over past frames (x16 fixed point).
    reads: u32,
    reads_now: u32,
    gpu_wrote_now: bool,
    cpu_wrote_now: bool,
    /// Frames since anything touched it.
    idle: u32,
}

impl Block {
    fn new() -> Self {
        Block { tier: Tier::InPlace, epoch: 0, card_epoch: None, reads: 0, reads_now: 0, gpu_wrote_now: false, cpu_wrote_now: false, idle: 0 }
    }

    /// Decayed GPU reads per frame.
    pub fn reads_per_frame(&self) -> f32 {
        self.reads as f32 / 16.0
    }

    /// Whether the GPU must be sent the home's bytes before it reads the card copy.
    pub fn stale_on_card(&self) -> bool {
        self.tier == Tier::Cached && self.card_epoch != Some(self.epoch)
    }
}

/// Placement rules; defaults from the step 0 probe and the rig's best preset.
#[derive(Clone, Copy, Debug)]
pub struct Rules {
    /// Reads a frame at or above which a block is worth a card copy. The
    /// probe: read once costs the same in place as copied; read 16 times is
    /// 10x faster from the card.
    pub promote_reads: f32,
    /// Frames untouched before a block goes cold.
    pub cold_after: u32,
    /// Card bytes this map may hold (cached plus GPU-owned). Seeded from the
    /// rig's best preset (`preset_4060`: CARD_BUFFERS_MB 1024 + CARD_WINDOWS_MB 512).
    pub card_budget: u64,
}

impl Default for Rules {
    fn default() -> Self {
        Rules { promote_reads: 2.0, cold_after: 120, card_budget: 1536 << 20 }
    }
}

/// What one frame's end changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Moves {
    pub promoted: Vec<u64>,
    pub demoted: Vec<u64>,
    pub owned: Vec<u64>,
    pub cooled: Vec<u64>,
    /// Promotions refused for want of card room (the block stays in place).
    pub refused: usize,
}

#[derive(Default)]
pub struct Blocks {
    map: HashMap<u64, Block>,
    pub rules: Rules,
    /// Card bytes held by `Cached` and `GpuOwned` blocks.
    on_card: u64,
}

fn blocks_of(at: u64, len: u64) -> impl Iterator<Item = u64> {
    let first = at / BLOCK;
    let last = if len == 0 { first } else { (at + len - 1) / BLOCK };
    (first..=last).map(|b| b * BLOCK)
}

impl Blocks {
    pub fn new(rules: Rules) -> Self {
        Blocks { rules, ..Default::default() }
    }

    pub fn get(&self, at: u64) -> Option<&Block> {
        self.map.get(&(at / BLOCK * BLOCK))
    }

    pub fn on_card(&self) -> u64 {
        self.on_card
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The GPU reads `[at, at + len)` in work being submitted. Returns the
    /// blocks whose card copy is stale and must be uploaded first.
    pub fn gpu_read(&mut self, at: u64, len: u64) -> Vec<u64> {
        let mut stale = Vec::new();
        for b in blocks_of(at, len) {
            let block = self.map.entry(b).or_insert_with(Block::new);
            block.reads_now += 1;
            if block.stale_on_card() {
                stale.push(b);
            }
        }
        stale
    }

    /// The card copy of block `b` now holds its current bytes (after an upload).
    pub fn uploaded(&mut self, b: u64) {
        if let Some(block) = self.map.get_mut(&b) {
            block.card_epoch = Some(block.epoch);
        }
    }

    /// The GPU writes `[at, at + len)`: those blocks become GPU-owned at the
    /// frame's end.
    pub fn gpu_write(&mut self, at: u64, len: u64) {
        for b in blocks_of(at, len) {
            self.map.entry(b).or_insert_with(Block::new).gpu_wrote_now = true;
        }
    }

    /// The CPU wrote `[at, at + len)` (a guard fault or write watch): the
    /// epoch moves, so card copies go stale.
    pub fn cpu_write(&mut self, at: u64, len: u64) {
        for b in blocks_of(at, len) {
            let block = self.map.entry(b).or_insert_with(Block::new);
            block.epoch = block.epoch.wrapping_add(1);
            block.cpu_wrote_now = true;
        }
    }

    /// Moves blocks between tiers from this frame's use. Hot blocks are
    /// promoted first, so the budget goes to the most-read ones.
    pub fn end_frame(&mut self) -> Moves {
        let mut moves = Moves::default();
        let rules = self.rules;
        let mut candidates: Vec<(u64, u32)> = Vec::new();
        for (&b, block) in self.map.iter_mut() {
            // Exponential decay with weight 1/4 on this frame, x16 fixed point.
            block.reads = (block.reads * 3 + block.reads_now * 16) / 4;
            let touched = block.reads_now > 0 || block.gpu_wrote_now || block.cpu_wrote_now;
            block.idle = if touched { 0 } else { block.idle.saturating_add(1) };
            if block.gpu_wrote_now && block.tier != Tier::GpuOwned {
                if block.tier != Tier::Cached {
                    self.on_card += BLOCK;
                }
                block.tier = Tier::GpuOwned;
                block.card_epoch = Some(block.epoch);
                moves.owned.push(b);
            } else if block.idle >= rules.cold_after && block.tier != Tier::Cold {
                if matches!(block.tier, Tier::Cached | Tier::GpuOwned) {
                    self.on_card -= BLOCK;
                }
                block.tier = Tier::Cold;
                block.card_epoch = None;
                moves.cooled.push(b);
            } else if block.tier == Tier::Cached && block.reads_per_frame() < rules.promote_reads / 2.0 {
                // Hysteresis: demote at half the promotion line.
                self.on_card -= BLOCK;
                block.tier = Tier::InPlace;
                block.card_epoch = None;
                moves.demoted.push(b);
            } else if matches!(block.tier, Tier::InPlace | Tier::Cold) && block.reads_now > 0 && block.reads_per_frame() >= rules.promote_reads {
                candidates.push((b, block.reads));
            } else if block.tier == Tier::Cold && block.reads_now > 0 {
                block.tier = Tier::InPlace;
            }
            block.reads_now = 0;
            block.gpu_wrote_now = false;
            block.cpu_wrote_now = false;
        }
        candidates.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (b, _) in candidates {
            let block = self.map.get_mut(&b).expect("candidate exists");
            if self.on_card + BLOCK > rules.card_budget {
                // Refused, not spilled: the block keeps working in place.
                block.tier = Tier::InPlace;
                moves.refused += 1;
                continue;
            }
            self.on_card += BLOCK;
            block.tier = Tier::Cached;
            block.card_epoch = None; // uploaded before its next read
            moves.promoted.push(b);
        }
        moves
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(blocks: &mut Blocks, n: usize, mut each: impl FnMut(&mut Blocks)) -> Moves {
        let mut last = Moves::default();
        for _ in 0..n {
            each(blocks);
            last = blocks.end_frame();
        }
        last
    }

    #[test]
    fn read_once_stays_in_place() {
        let mut b = Blocks::new(Rules::default());
        frames(&mut b, 10, |b| {
            b.gpu_read(0, 100);
        });
        assert_eq!(b.get(0).unwrap().tier, Tier::InPlace);
        assert_eq!(b.on_card(), 0);
    }

    #[test]
    fn hot_block_is_promoted_then_uploaded_only_when_written() {
        let mut b = Blocks::new(Rules::default());
        frames(&mut b, 6, |b| {
            for _ in 0..8 {
                b.gpu_read(BLOCK, 16);
            }
        });
        assert_eq!(b.get(BLOCK).unwrap().tier, Tier::Cached);
        assert_eq!(b.gpu_read(BLOCK, 16), vec![BLOCK]);
        b.uploaded(BLOCK);
        assert!(b.gpu_read(BLOCK, 16).is_empty());
        b.cpu_write(BLOCK + 8, 4);
        assert_eq!(b.gpu_read(BLOCK, 16), vec![BLOCK]);
    }

    #[test]
    fn gpu_writes_make_it_owned_and_cold_frees_the_card() {
        let mut b = Blocks::new(Rules { cold_after: 3, ..Rules::default() });
        b.gpu_write(0, 2 * BLOCK);
        let m = b.end_frame();
        assert_eq!(m.owned.len(), 2);
        assert_eq!(b.on_card(), 2 * BLOCK);
        let m = frames(&mut b, 3, |_| {});
        assert_eq!(m.cooled.len(), 2);
        assert_eq!(b.on_card(), 0);
    }

    #[test]
    fn budget_refuses_instead_of_spilling_and_hottest_wins() {
        let mut b = Blocks::new(Rules { card_budget: BLOCK, ..Rules::default() });
        frames(&mut b, 6, |b| {
            for _ in 0..4 {
                b.gpu_read(0, 1);
            }
            for _ in 0..12 {
                b.gpu_read(BLOCK, 1);
            }
        });
        assert_eq!(b.get(BLOCK).unwrap().tier, Tier::Cached);
        assert_eq!(b.get(0).unwrap().tier, Tier::InPlace);
        assert_eq!(b.on_card(), BLOCK);
    }

    #[test]
    fn ranges_cover_every_block_touched() {
        assert_eq!(blocks_of(BLOCK - 1, 2).collect::<Vec<_>>(), vec![0, BLOCK]);
        assert_eq!(blocks_of(0, 0).collect::<Vec<_>>(), vec![0]);
    }
}
