//! A minimal ARM946E-S cache timing model (instruction and data caches).
//!
//! The ARM9's effective speed on external memory is dominated by its caches: a hit
//! costs half a cycle, while a miss pays the full main-RAM line fill. We do not
//! emulate cache *contents* (memory correctness is unaffected) — only which 32-byte
//! lines are currently resident, so accesses can be charged as hits vs misses. This
//! is the piece that makes the ARM9's timing (and thus the inter-core phase)
//! realistic; grounded in GBATEK "DS Memory Timings" (hit = 0.5 cycle; a main-RAM
//! cache miss ≈ line fill).
//!
//! Geometry follows the real ARM946E-S: **4-way set-associative**, 32-byte lines,
//! with the instruction cache 8 KB (64 sets) and the data cache 4 KB (32 sets).
//! Replacement is round-robin (the ARM946E-S reset default), modelled with a
//! per-set cyclic victim counter. A direct-mapped stand-in aliased and evicted
//! lines the real 4-way array keeps resident, so its hit/miss decisions drifted
//! from hardware; the set-associative array fixes that.

const LINE_BYTES: u32 = 32;
const LINE_SHIFT: u32 = 5; // log2(LINE_BYTES)
const INVALID: u32 = u32::MAX;

/// A timing-only N-way set-associative cache: tracks which 32-byte lines are
/// resident (by line number = `address >> 5`) so accesses can be charged hit/miss.
pub struct Cache {
    sets: usize,
    ways: usize,
    /// `sets * ways` line tags (line numbers); `INVALID` marks an empty way.
    tags: Box<[u32]>,
    /// Per-set round-robin replacement pointer (next way to evict on a miss).
    victim: Box<[u8]>,
}

impl Cache {
    /// Build a cache from its byte size and associativity. `size_bytes` must be a
    /// multiple of `ways * 32`, and `ways` and the resulting set count powers of two
    /// (true for every ARM946E-S configuration).
    pub fn new(size_bytes: usize, ways: usize) -> Self {
        let sets = size_bytes / (ways * LINE_BYTES as usize);
        debug_assert!(sets.is_power_of_two(), "set count must be a power of two");
        Cache {
            sets,
            ways,
            tags: vec![INVALID; sets * ways].into_boxed_slice(),
            victim: vec![0u8; sets].into_boxed_slice(),
        }
    }

    /// The ARM946E-S 8 KB instruction cache (4-way, 64 sets).
    pub fn instruction() -> Self {
        Self::new(8 * 1024, 4)
    }

    /// The ARM946E-S 4 KB data cache (4-way, 32 sets).
    pub fn data() -> Self {
        Self::new(4 * 1024, 4)
    }

    /// Note an access at `address`; returns `true` on a cache hit. A miss installs
    /// the line into the set's round-robin victim way (so the rest of that line then
    /// hits, modelling a line fill), evicting whatever it held.
    pub fn access(&mut self, address: u32) -> bool {
        let line = address >> LINE_SHIFT; // 32-byte line number
        let set = line as usize & (self.sets - 1);
        let base = set * self.ways;
        let ways = &mut self.tags[base..base + self.ways];
        if ways.contains(&line) {
            return true;
        }
        // Miss: install into the round-robin victim way and advance the pointer.
        let v = &mut self.victim[set];
        ways[*v as usize] = line;
        *v = (*v + 1) % self.ways as u8;
        false
    }

    /// Whether `address`'s line is resident, WITHOUT installing it on a miss. The
    /// ARM946E-S allocates cache lines on reads only; a write probes for a resident
    /// line (a hit is fast) but never fills one, so stores use this instead of
    /// [`Self::access`].
    pub fn contains(&self, address: u32) -> bool {
        let line = address >> LINE_SHIFT;
        let set = line as usize & (self.sets - 1);
        let base = set * self.ways;
        self.tags[base..base + self.ways].contains(&line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_line_misses_then_hits_within_the_line() {
        let mut c = Cache::instruction();
        // First access to a line misses; the next word in the same 32-byte line hits.
        assert!(!c.access(0x0200_0000));
        assert!(c.access(0x0200_0004));
        assert!(c.access(0x0200_001C));
        // A different line misses.
        assert!(!c.access(0x0200_0020));
    }

    #[test]
    fn four_ways_coexist_without_evicting() {
        // Four lines that map to the same set (8 KB / 4 ways = 64 sets => set stride
        // is 64*32 = 0x800) all stay resident in a 4-way array.
        let mut c = Cache::instruction();
        let set_stride = 64 * 32;
        for k in 0..4 {
            assert!(!c.access(0x0200_0000 + k * set_stride)); // cold miss, fills a way
        }
        // All four still hit — a direct-mapped cache would have evicted the first three.
        for k in 0..4 {
            assert!(c.access(0x0200_0000 + k * set_stride));
        }
    }

    #[test]
    fn fifth_line_evicts_the_round_robin_victim() {
        let mut c = Cache::instruction();
        let set_stride = 64 * 32;
        for k in 0..4 {
            assert!(!c.access(0x0200_0000 + k * set_stride));
        }
        // A 5th conflicting line evicts way 0 (the round-robin victim = line 0).
        assert!(!c.access(0x0200_0000 + 4 * set_stride));
        // Line 1 was NOT the victim, so it still hits (check it before re-inserting
        // line 0, since a miss on line 0 would itself evict the next victim way).
        assert!(c.access(0x0200_0000 + set_stride));
        // Line 0 was evicted, so it misses again.
        assert!(!c.access(0x0200_0000));
    }
}
