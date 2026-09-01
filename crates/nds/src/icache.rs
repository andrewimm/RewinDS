//! A minimal ARM9 (ARM946E-S) instruction-cache timing model.
//!
//! The ARM9's effective speed on external memory is dominated by its cache: a hit
//! costs half a cycle, while a miss pays the full main-RAM line-fill. We do not
//! emulate cache *contents* (memory correctness is unaffected) — only which 32-byte
//! lines are currently resident, so opcode fetches can be charged as hits vs misses.
//! This is the piece that makes the ARM9's timing (and thus the inter-core phase)
//! realistic; grounded in GBATEK "DS Memory Timings" (hit = 0.5 cycle; a main-RAM
//! cache miss = 23 cycles for the line fill).
//!
//! Geometry follows the ARM946E-S 8 KB cache: 32-byte lines, here direct-mapped
//! (256 lines) as a minimal stand-in for the real 4-way set-associative array.

const LINES: usize = 256; // 256 * 32 bytes = 8 KB
const INVALID: u32 = u32::MAX;

/// Which 32-byte lines are resident, for hit/miss timing only.
pub struct ICache {
    tags: Box<[u32]>,
}

impl Default for ICache {
    fn default() -> Self {
        ICache {
            tags: vec![INVALID; LINES].into_boxed_slice(),
        }
    }
}

impl ICache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Note an instruction fetch at `address`; returns `true` on a cache hit. A miss
    /// installs the line (so the rest of that line then hits, modelling a line fill).
    pub fn access(&mut self, address: u32) -> bool {
        let block = address >> 5; // 32-byte line number
        let index = block as usize & (LINES - 1);
        let tag = block;
        if self.tags[index] == tag {
            true
        } else {
            self.tags[index] = tag;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_line_misses_then_hits_within_the_line() {
        let mut c = ICache::new();
        // First access to a line misses; the next word in the same 32-byte line hits.
        assert!(!c.access(0x0200_0000));
        assert!(c.access(0x0200_0004));
        assert!(c.access(0x0200_001C));
        // A different line misses.
        assert!(!c.access(0x0200_0020));
    }

    #[test]
    fn direct_mapped_lines_alias_and_evict() {
        let mut c = ICache::new();
        assert!(!c.access(0x0200_0000)); // line 0
        // 256 lines * 32 bytes = 0x2000 apart maps to the same index.
        assert!(!c.access(0x0200_2000)); // evicts line 0's tag
        assert!(!c.access(0x0200_0000)); // now a miss again
    }
}
