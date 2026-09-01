//! The geometry command FIFO — real hardware, not a function call.
//!
//! The DS buffers geometry commands in a 256-entry ring the CPU (or a GXFIFO DMA)
//! fills and the geometry engine drains. It can fill and **stall the CPU** (a write
//! to a full FIFO back-pressures the ARM9 until the engine drains one), it exposes
//! its fill level through `GXSTAT`, it can raise an IRQ when it empties or falls
//! below half, and a DMA fires to refill it below half. Each stored entry is one
//! `(command, parameter)` pair — a multi-parameter command occupies several entries.

use crate::command;

/// FIFO depth in entries (GBATEK: 256, plus a small pipe we fold in).
pub const CAPACITY: usize = 256;
/// The half-full threshold that drives the "less than half" status, IRQ mode 1, and
/// GXFIFO DMA.
pub const HALF: usize = CAPACITY / 2;

/// One buffered command word: an opcode and a single parameter word. Zero-parameter
/// commands store a dummy `param` of 0.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub command: u8,
    pub param: u32,
}

/// FIFO IRQ mode (`GXSTAT` bits 30-31).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum IrqMode {
    #[default]
    Never,
    LessThanHalf,
    Empty,
}

impl IrqMode {
    fn from_bits(bits: u32) -> IrqMode {
        match bits & 3 {
            1 => IrqMode::LessThanHalf,
            2 => IrqMode::Empty,
            _ => IrqMode::Never,
        }
    }
    fn to_bits(self) -> u32 {
        match self {
            IrqMode::Never => 0,
            IrqMode::LessThanHalf => 1,
            IrqMode::Empty => 2,
        }
    }
}

/// The command ring buffer plus its IRQ mode.
pub struct Fifo {
    buf: Box<[Entry]>,
    /// Index of the next entry to pop.
    head: usize,
    /// Number of entries currently buffered.
    len: usize,
    irq_mode: IrqMode,
}

impl Default for Fifo {
    fn default() -> Self {
        Fifo::new()
    }
}

impl Fifo {
    pub fn new() -> Self {
        Fifo {
            buf: vec![Entry::default(); CAPACITY].into_boxed_slice(),
            head: 0,
            len: 0,
            irq_mode: IrqMode::Never,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn is_full(&self) -> bool {
        self.len >= CAPACITY
    }
    /// Whether the fill level is below half — the condition for GXFIFO DMA and IRQ
    /// mode 1. (`< HALF`, so exactly-half does not qualify.)
    pub fn less_than_half(&self) -> bool {
        self.len < HALF
    }

    /// Push one entry. Returns `false` if the FIFO is full (the caller must stall the
    /// CPU rather than drop the write — real hardware back-pressures).
    pub fn push(&mut self, entry: Entry) -> bool {
        if self.is_full() {
            return false;
        }
        let tail = (self.head + self.len) % CAPACITY;
        self.buf[tail] = entry;
        self.len += 1;
        true
    }

    /// Pop the oldest entry, or `None` when empty.
    pub fn pop(&mut self) -> Option<Entry> {
        if self.len == 0 {
            return None;
        }
        let entry = self.buf[self.head];
        self.head = (self.head + 1) % CAPACITY;
        self.len -= 1;
        Some(entry)
    }

    /// Peek the oldest entry without removing it.
    pub fn front(&self) -> Option<Entry> {
        (self.len > 0).then(|| self.buf[self.head])
    }

    pub fn set_irq_mode_bits(&mut self, bits: u32) {
        self.irq_mode = IrqMode::from_bits(bits);
    }

    /// Whether the FIFO currently satisfies its IRQ condition (level-triggered; the
    /// interrupt controller samples this).
    pub fn irq_asserted(&self) -> bool {
        match self.irq_mode {
            IrqMode::Never => false,
            IrqMode::LessThanHalf => self.less_than_half(),
            IrqMode::Empty => self.is_empty(),
        }
    }

    /// A GXFIFO DMA refills the FIFO whenever it is less than half full.
    pub fn wants_dma(&self) -> bool {
        self.less_than_half()
    }

    /// The `GXSTAT` (`0x4000600`) bits this FIFO owns: count (16-24), less-than-half
    /// (25), empty (26), and IRQ mode (30-31). Matrix-stack and geometry-busy bits
    /// are contributed by other subsystems.
    pub fn gxstat_bits(&self) -> u32 {
        let mut v = (self.len as u32 & 0x1FF) << 16;
        if self.less_than_half() {
            v |= 1 << 25;
        }
        if self.is_empty() {
            v |= 1 << 26;
        }
        v |= self.irq_mode.to_bits() << 30;
        v
    }
}

/// Push the entries a single GXFIFO word decodes to, stopping (returning the number
/// of entries that did **not** fit) if the FIFO fills mid-word. Used by the packed
/// path; the direct-port path pushes a single entry.
pub fn push_decoded(fifo: &mut Fifo, decoder: &mut command::Decoder, word: u32) -> usize {
    let mut overflow = 0;
    decoder.feed(word, |command, param| {
        if !fifo.push(Entry { command, param }) {
            overflow += 1;
        }
    });
    overflow
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_wraps_and_preserves_order() {
        let mut f = Fifo::new();
        // Fill, drain half, refill past the wrap point, and confirm FIFO order.
        for i in 0..CAPACITY as u32 {
            assert!(f.push(Entry { command: 1, param: i }));
        }
        assert!(f.is_full());
        assert!(!f.push(Entry { command: 1, param: 999 })); // full → rejected
        for i in 0..100 {
            assert_eq!(f.pop().unwrap().param, i);
        }
        for i in 0..100u32 {
            assert!(f.push(Entry { command: 2, param: 1000 + i }));
        }
        assert_eq!(f.len(), CAPACITY - 100 + 100);
        // Next pops continue the original sequence (100..256) before the new ones.
        assert_eq!(f.pop().unwrap().param, 100);
    }

    #[test]
    fn status_flags_track_fill_level() {
        let mut f = Fifo::new();
        assert!(f.is_empty() && f.less_than_half());
        for _ in 0..HALF - 1 {
            f.push(Entry::default());
        }
        assert!(f.less_than_half()); // 127 < 128
        f.push(Entry::default());
        assert!(!f.less_than_half()); // 128 is not < 128
        assert!(!f.wants_dma());
    }

    #[test]
    fn irq_and_dma_conditions() {
        let mut f = Fifo::new();
        f.set_irq_mode_bits(2); // Empty
        assert!(f.irq_asserted());
        f.push(Entry::default());
        assert!(!f.irq_asserted());

        f.set_irq_mode_bits(1); // LessThanHalf
        assert!(f.irq_asserted() && f.wants_dma());
        for _ in 1..HALF {
            f.push(Entry::default());
        }
        assert_eq!(f.len(), HALF);
        assert!(!f.irq_asserted() && !f.wants_dma());
    }
}
