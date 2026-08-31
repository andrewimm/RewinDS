//! The GamePak ROM prefetch buffer (address-aware).
//!
//! When enabled (WAITCNT bit 14), the cartridge prefetcher reads opcodes ahead
//! from ROM during any cycle the CPU isn't using the cartridge bus. It buffers up
//! to eight 16-bit halfwords, tracking *which* addresses it holds: the buffer
//! occupies `[head, head + 2*count)` and it is fetching the halfword at the far
//! end. A code fetch then resolves against those addresses:
//!
//! - The next sequential halfword (`addr == head`) is served from the buffer in a
//!   single cycle, or — if the buffer is empty — waits for the in-flight fetch.
//! - A **forward branch into the buffered region** (`head < addr < far`) is also
//!   served from the buffer: the skipped halfwords are discarded and the rest,
//!   already the sequential continuation from the target, are kept. This is why a
//!   branch to a nearby, already-prefetched address is cheap.
//! - Any other fetch flushes the buffer and pays the full non-sequential access,
//!   restarting the prefetch stream at the target.
//!
//! Prefetch benefits **instruction fetches from ROM only**. [`Prefetch::step`]
//! advances the in-flight fetch during the CPU's non-cartridge cycles;
//! [`Prefetch::access`] resolves a code halfword fetch.

/// The cartridge prefetch buffer state.
#[derive(Clone, Copy, Debug, Default)]
pub struct Prefetch {
    enabled: bool,
    /// Address of the next halfword the CPU will consume — the front of the
    /// buffered region `[head, head + 2*count)`.
    head: u32,
    /// Halfwords currently buffered ahead of the CPU (0..=8).
    count: u8,
    /// Cycles remaining on the halfword being fetched (at `head + 2*count`).
    countdown: u32,
    /// Sequential access cost of one ROM halfword (the prefetch fetch time).
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

    /// A CPU data access to the cartridge steals the ROM bus: the in-flight
    /// prefetch is dropped and the buffered halfwords discarded, so the stream
    /// restarts from the CPU's next opcode fetch.
    pub fn on_cart_data_access(&mut self) {
        self.count = 0;
        if self.halfword_cost > 0 {
            self.countdown = self.halfword_cost;
        }
    }

    /// The far end of the buffer — the address currently being fetched.
    fn far(&self) -> u32 {
        self.head.wrapping_add(2 * self.count as u32)
    }

    /// Advance the in-flight fetch during `idle` cycles in which the CPU is not
    /// using the ROM bus, completing buffered halfwords up to capacity.
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
                return;
            }
        }
    }

    /// Resolve a code halfword fetch at `addr`, returning its cost in cycles and
    /// advancing the buffer. `halfword_cost` is the sequential per-halfword cost
    /// (`1 + WSn.S`); `nonseq_cost` is the full non-sequential access (`1 + WSn.N`).
    pub fn access(&mut self, addr: u32, halfword_cost: u32, nonseq_cost: u32) -> u32 {
        self.halfword_cost = halfword_cost;

        if addr == self.head {
            if self.count > 0 {
                // Sequential hit: the halfword is buffered. The freed ROM bus lets
                // the in-flight fetch advance during this cycle.
                self.count -= 1;
                self.head = self.head.wrapping_add(2);
                return 1;
            }
            // Buffer empty: the prefetcher is mid-fetch of this very halfword; wait.
            let cost = self.countdown.max(1);
            self.head = self.head.wrapping_add(2);
            self.countdown = self.halfword_cost;
            return cost;
        }

        if addr > self.head && addr < self.far() {
            // Forward branch into the buffered region: discard the skipped
            // halfwords, keep the rest (the continuation from the target).
            let skipped = ((addr - self.head) / 2) as u8;
            self.count -= skipped + 1;
            self.head = addr.wrapping_add(2);
            return 1;
        }

        // Not buffered: flush, pay the non-sequential access, restart ahead.
        self.head = addr.wrapping_add(2);
        self.count = 0;
        self.countdown = self.halfword_cost;
        nonseq_cost
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> Prefetch {
        let mut p = Prefetch::default();
        p.set_enabled(true);
        p
    }

    #[test]
    fn first_fetch_is_nonsequential_then_buffer_fills() {
        let mut p = enabled();
        // First fetch at 0x0800_0000 is not buffered -> non-sequential cost.
        assert_eq!(p.access(0x0800_0000, 2, 4), 4);
        // Prefetcher is now fetching 0x0800_0002 (head), countdown = 2.
        // Give it idle cycles to buffer several halfwords ahead.
        p.step(8);
        // Sequential fetches are now served from the buffer at 1 cycle each.
        assert_eq!(p.access(0x0800_0002, 2, 4), 1);
        assert_eq!(p.access(0x0800_0004, 2, 4), 1);
    }

    #[test]
    fn empty_buffer_waits_for_in_flight_fetch() {
        let mut p = enabled();
        p.access(0x0800_0000, 3, 5); // flush; now fetching 0x0800_0002, countdown 3
        // No idle time: the next sequential fetch waits the full in-flight fetch.
        assert_eq!(p.access(0x0800_0002, 3, 5), 3);
    }

    #[test]
    fn forward_branch_into_buffer_is_cheap_and_keeps_the_rest() {
        let mut p = enabled();
        p.access(0x0800_0000, 2, 4); // head = 0x0800_0002
        p.step(20); // buffer fills to capacity ahead of 0x0800_0002
        // Branch forward to 0x0800_0008 (inside the buffered region).
        assert_eq!(p.access(0x0800_0008, 2, 4), 1);
        // The continuation is still buffered: the next sequential fetch is cheap.
        assert_eq!(p.access(0x0800_000A, 2, 4), 1);
    }

    #[test]
    fn branch_outside_buffer_flushes_and_pays_nonsequential() {
        let mut p = enabled();
        p.access(0x0800_0000, 2, 4);
        p.step(20);
        // Branch far away, outside the buffered window -> non-sequential.
        assert_eq!(p.access(0x08F0_0000, 2, 4), 4);
    }

    #[test]
    fn disabled_prefetch_does_not_buffer() {
        let mut p = Prefetch::default(); // disabled
        p.step(100);
        assert_eq!(p.count, 0);
    }
}
