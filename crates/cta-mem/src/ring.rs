//! The upload ring (PLAN.md 2.1, "two transports"): one fixed upload heap,
//! reused every frame. Blocks a batch reads are copied from the home into
//! it, then the copy queue moves them to the card. It is bounded, so it is
//! never a second home: the RAM saving holds without opened heaps.
//!
//! This is the allocator only. Space is handed out in order and given back
//! when the GPU's fence passes the value it was handed out under.

use std::collections::VecDeque;

/// A span of the ring: `offset` bytes in, `len` long.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub offset: u64,
    pub len: u64,
}

pub struct Ring {
    size: u64,
    align: u64,
    /// Next free byte (monotonic; the offset is `head % size`).
    head: u64,
    /// Oldest byte still in use.
    tail: u64,
    /// In-flight batches: the fence value and the head when it was closed.
    flight: VecDeque<(u64, u64)>,
}

impl Ring {
    /// A ring of `size` bytes, spans aligned to `align` (a power of two).
    pub fn new(size: u64, align: u64) -> Self {
        assert!(align.is_power_of_two() && size % align == 0 && size > 0);
        Ring { size, align, head: 0, tail: 0, flight: VecDeque::new() }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Bytes in use (handed out and not yet given back).
    pub fn used(&self) -> u64 {
        self.head - self.tail
    }

    /// Room for `len` bytes, contiguous. `None` if the ring is full until the
    /// GPU catches up (the caller then waits on the oldest fence, or splits).
    pub fn alloc(&mut self, len: u64) -> Option<Span> {
        let len = len.next_multiple_of(self.align);
        if len == 0 || len > self.size {
            return None;
        }
        let mut start = self.head;
        let offset = start % self.size;
        if offset + len > self.size {
            // Skip the tail end so the span is contiguous.
            start += self.size - offset;
        }
        if start + len - self.tail > self.size {
            return None;
        }
        self.head = start + len;
        Some(Span { offset: start % self.size, len })
    }

    /// Everything handed out so far belongs to the batch signalled at `fence`.
    pub fn close(&mut self, fence: u64) {
        if self.flight.back().map(|&(_, h)| h) != Some(self.head) {
            self.flight.push_back((fence, self.head));
        }
    }

    /// The GPU's fence reached `done`: batches at or below it give their space back.
    pub fn retire(&mut self, done: u64) {
        while let Some(&(fence, head)) = self.flight.front() {
            if fence > done {
                break;
            }
            self.tail = head;
            self.flight.pop_front();
        }
    }

    /// The fence to wait on to free the oldest batch, if any.
    pub fn oldest(&self) -> Option<u64> {
        self.flight.front().map(|&(f, _)| f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hands_out_in_order_and_wraps_after_retire() {
        let mut r = Ring::new(1024, 256);
        assert_eq!(r.alloc(100), Some(Span { offset: 0, len: 256 }));
        assert_eq!(r.alloc(512), Some(Span { offset: 256, len: 512 }));
        r.close(1);
        // 256 left at the end; 512 does not fit until fence 1 passes.
        assert_eq!(r.alloc(512), None);
        assert_eq!(r.oldest(), Some(1));
        r.retire(1);
        assert_eq!(r.used(), 0);
        // Skips the 256 tail end to stay contiguous.
        assert_eq!(r.alloc(512), Some(Span { offset: 0, len: 512 }));
    }

    #[test]
    fn refuses_more_than_the_ring() {
        let mut r = Ring::new(1024, 256);
        assert_eq!(r.alloc(2048), None);
        assert_eq!(r.alloc(0), None);
    }

    #[test]
    fn older_batches_retire_first() {
        let mut r = Ring::new(1024, 256);
        r.alloc(256);
        r.close(5);
        r.alloc(256);
        r.close(6);
        r.retire(5);
        assert_eq!(r.used(), 256);
        r.retire(6);
        assert_eq!(r.used(), 0);
    }
}
