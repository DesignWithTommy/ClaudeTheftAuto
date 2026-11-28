//! The home: how title memory's sections are made (PLAN.md 2.1).
//!
//! - 2 MiB sections: 32x fewer views to map than 64 KiB ones, at the same
//!   cost per view (step 0 probe: 0.2 + 0.3 ms against 5.4 + 8.8 ms for
//!   512 MB), and a remap rebuilds one small section, not a giant one.
//! - `SEC_COMMIT`: only a committed section can be opened as a D3D12 heap
//!   (`OpenExistingHeapFromFileMapping` refuses `SEC_RESERVE`, step 0).
//!   Commit charge is taken at creation, but pages stay unbacked until
//!   touched, so RAM is paid only for what the title writes.

/// Bytes per home section.
pub const SECTION: u64 = 2 << 20;

/// How a physical allocation of `bytes` is split: the size of each section,
/// the last one shorter if `bytes` is not a multiple of [`SECTION`].
pub fn split(bytes: u64) -> impl Iterator<Item = u64> {
    let full = bytes / SECTION;
    let tail = bytes % SECTION;
    std::iter::repeat_n(SECTION, full as usize).chain((tail != 0).then_some(tail))
}

/// Pages of `page` bytes per section.
pub fn pages_per_section(page: u64) -> usize {
    (SECTION / page).max(1) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_into_sections() {
        assert_eq!(split(4 * SECTION).collect::<Vec<_>>(), vec![SECTION; 4]);
        assert_eq!(split(SECTION + 65536).collect::<Vec<_>>(), vec![SECTION, 65536]);
        assert_eq!(split(0).count(), 0);
        assert_eq!(pages_per_section(65536), 32);
    }
}
