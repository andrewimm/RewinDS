//! The GamePak ROM prefetch buffer.
//!
//! When enabled (WAITCNT bit 14), the cartridge prefetcher reads opcodes ahead
//! from ROM during cycles the CPU isn't using the cartridge bus. A sequential
//! opcode fetch that finds its halfword already buffered costs a single cycle
//! instead of the full wait state; a non-sequential fetch (a branch) flushes the
//! buffer and pays the non-sequential access in full. The buffer holds up to
//! eight 16-bit halfwords.
//!
//! Prefetch benefits **instruction fetches from ROM only** — data reads from
//! ROM, and code running in RAM or BIOS, are unaffected.
//!
//! The model is driven by the CPU's access stream: [`Prefetch::fetch_sequential`]
//! consumes a buffered halfword, [`Prefetch::restart`] handles a branch, and
//! [`Prefetch::step`] advances the buffer during the CPU's idle cycles. The
//! interface is exercised directly in tests until the CPU drives it.

/// The cartridge prefetch buffer state.
#[derive(Clone, Copy, Debug, Default)]
pub struct Prefetch {
    enabled: bool,
    /// Halfwords currently buffered ahead of the CPU (0..=8).
    count: u8,
    /// Cycles remaining on the halfword currently being fetched.
    countdown: u32,
    /// Sequential access cycles for one ROM halfword (set on restart).
    halfword_cost: u32,
}

/// The buffer capacity, in halfwords.
const CAPACITY: u8 = 8;

impl Prefetch {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Enable or disable prefetch (WAITCNT bit 14). Disabling flushes the buffer.
    pub fn set_enabled(&mut self, enabled: bool) {
        if !enabled {
            self.count = 0;
            self.countdown = 0;
        }
        self.enabled = enabled;
    }

    /// Restart the prefetch stream after a non-sequential fetch: the buffer is
    /// flushed and the prefetcher begins fetching the next halfword, each taking
    /// `halfword_cost` cycles.
    pub fn restart(&mut self, halfword_cost: u32) {
        self.count = 0;
        self.halfword_cost = halfword_cost;
        self.countdown = halfword_cost;
    }

    /// Consume one sequential opcode halfword, returning its cost: one cycle if
    /// it was already buffered, otherwise the wait for the in-flight fetch to
    /// finish.
    pub fn fetch_sequential(&mut self) -> u32 {
        if self.count > 0 {
            self.count -= 1;
            1
        } else {
            let cost = self.countdown.max(1);
            self.countdown = self.halfword_cost;
            cost
        }
    }

    /// Advance the prefetcher during `idle` cycles in which the CPU is not using
    /// the ROM bus, buffering completed halfwords up to capacity.
    pub fn step(&mut self, idle: u32) {
        if !self.enabled || self.halfword_cost == 0 {
            return;
        }
        let mut remaining = idle;
        while self.count < CAPACITY {
            if remaining >= self.countdown {
                remaining -= self.countdown;
                self.count += 1;
                self.countdown = self.halfword_cost;
            } else {
                self.countdown -= remaining;
                break;
            }
        }
    }
}
