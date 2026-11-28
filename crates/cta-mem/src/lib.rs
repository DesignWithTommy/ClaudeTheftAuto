//! `cta-mem`: CTA's memory core (PLAN.md 2.1). Written for CTA, not copied.
//!
//! Title memory has exactly one home: pagefile-backed sections in RAM
//! ([`home`]). The card's memory is a cache of it with its own valid bits,
//! never a second home. Every 64 KiB block carries counters and a tier
//! ([`blocks`]); the counters come from the read and write sets of
//! translated work, never from scanning memory, so the work each frame
//! scales with what changed. Blocks that must reach the card go through a
//! fixed, reused upload ring ([`ring`]).

pub mod blocks;
pub mod home;
pub mod ring;

/// Bytes in a block: the title's page size, and the unit of placement.
pub const BLOCK: u64 = 64 << 10;

/// Whether CTA's memory core is on (`CTA_MEM=1`). Off, the cloned paths run
/// as they were.
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| matches!(std::env::var("CTA_MEM").as_deref(), Ok("1")))
}
