//! The GBA's four DMA channels.
//!
//! DMA is a bus master, not a memcpy shortcut: a transfer moves data through the
//! same bus the CPU uses, tagged with [`AccessMaster::Dma`], so region rules,
//! wait states, and access restrictions all apply. This module holds each
//! channel's register state and configuration decode; the transfer loop itself
//! lives on the [`Bus`](crate::bus::Bus), which owns both the memory and this
//! state and so can read config here while driving bus accesses.
//!
//! A transfer runs in full at the instant it is triggered (the scheduler design
//! permits that), and its read+write+internal cycle cost is charged to the CPU
//! timeline as a stall (see `dma_stall_cycles` on the bus). All four start
//! timings are handled: immediate, V-blank, H-blank, and the per-channel
//! "special" modes (DMA1/2 sound FIFO, DMA3 video capture).

/// When a channel's transfer is (re)started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaTiming {
    Immediate,
    VBlank,
    HBlank,
    /// DMA1/2 sound FIFO, or DMA3 video capture.
    Special,
}

/// One DMA channel's registers and internal transfer pointers.
#[derive(Clone, Copy, Debug)]
pub struct DmaChannel {
    pub(crate) id: u8,
    /// `DMAxSAD` source address (write-only register).
    pub(crate) source: u32,
    /// `DMAxDAD` destination address (write-only register).
    pub(crate) dest: u32,
    /// `DMAxCNT_L` word count (write-only register).
    pub(crate) count_reg: u16,
    /// `DMAxCNT_H` control.
    pub(crate) control: u16,
    // Internal pointers/counter used during a transfer; the visible registers
    // above are never modified by the hardware.
    pub(crate) internal_source: u32,
    pub(crate) internal_dest: u32,
    pub(crate) internal_count: u32,
    pub(crate) enabled: bool,
    /// Armed for an immediate-timing start by an enable edge.
    pub(crate) start_pending: bool,
}

impl DmaChannel {
    fn new(id: u8) -> Self {
        DmaChannel {
            id,
            source: 0,
            dest: 0,
            count_reg: 0,
            control: 0,
            internal_source: 0,
            internal_dest: 0,
            internal_count: 0,
            enabled: false,
            start_pending: false,
        }
    }

    pub(crate) fn timing(&self) -> DmaTiming {
        match (self.control >> 12) & 3 {
            0 => DmaTiming::Immediate,
            1 => DmaTiming::VBlank,
            2 => DmaTiming::HBlank,
            _ => DmaTiming::Special,
        }
    }

    pub(crate) fn is_32bit(&self) -> bool {
        self.control & (1 << 10) != 0
    }

    pub(crate) fn unit_bytes(&self) -> u32 {
        if self.is_32bit() {
            4
        } else {
            2
        }
    }

    pub(crate) fn repeat(&self) -> bool {
        self.control & (1 << 9) != 0
    }

    pub(crate) fn irq_on_end(&self) -> bool {
        self.control & (1 << 14) != 0
    }

    /// The signed step (as a wrapping addend) applied to the source after each
    /// unit: increment, decrement, or fixed (source control 3 is prohibited and
    /// treated as fixed).
    pub(crate) fn source_step(&self) -> u32 {
        let unit = self.unit_bytes();
        match (self.control >> 7) & 3 {
            0 => unit,
            1 => unit.wrapping_neg(),
            _ => 0,
        }
    }

    /// The destination step: increment (including increment/reload), decrement,
    /// or fixed.
    pub(crate) fn dest_step(&self) -> u32 {
        let unit = self.unit_bytes();
        match (self.control >> 5) & 3 {
            0 | 3 => unit,
            1 => unit.wrapping_neg(),
            _ => 0,
        }
    }

    /// Whether the destination reloads on repeat (destination control 3).
    pub(crate) fn dest_reloads(&self) -> bool {
        (self.control >> 5) & 3 == 3
    }

    fn source_mask(&self) -> u32 {
        // DMA0 source is internal memory only; others reach any memory.
        if self.id == 0 {
            0x07FF_FFFF
        } else {
            0x0FFF_FFFF
        }
    }

    fn dest_mask(&self) -> u32 {
        // Only DMA3's destination reaches any memory.
        if self.id == 3 {
            0x0FFF_FFFF
        } else {
            0x07FF_FFFF
        }
    }

    pub(crate) fn masked_source(&self) -> u32 {
        (self.source & self.source_mask()) & !(self.unit_bytes() - 1)
    }

    pub(crate) fn masked_dest(&self) -> u32 {
        (self.dest & self.dest_mask()) & !(self.unit_bytes() - 1)
    }

    /// The transfer length, applying the per-channel maximum (a zero count means
    /// the maximum).
    pub(crate) fn latched_count(&self) -> u32 {
        if self.id == 3 {
            if self.count_reg == 0 {
                0x1_0000
            } else {
                self.count_reg as u32
            }
        } else {
            let count = (self.count_reg & 0x3FFF) as u32;
            if count == 0 {
                0x4000
            } else {
                count
            }
        }
    }

    /// Apply a write to `DMAxCNT_H`. An enable edge (0→1) latches the internal
    /// pointers and, for immediate timing, arms the transfer.
    fn write_control(&mut self, new_control: u16) {
        let was_enabled = self.enabled;
        self.control = new_control;
        let now_enabled = new_control & (1 << 15) != 0;
        if now_enabled && !was_enabled {
            self.internal_source = self.masked_source();
            self.internal_dest = self.masked_dest();
            self.internal_count = self.latched_count();
            if self.timing() == DmaTiming::Immediate {
                self.start_pending = true;
            }
        }
        self.enabled = now_enabled;
    }
}

/// The four DMA channels.
#[derive(Clone, Copy, Debug)]
pub struct Dma {
    pub(crate) channels: [DmaChannel; 4],
}

impl Default for Dma {
    fn default() -> Self {
        Dma {
            channels: [
                DmaChannel::new(0),
                DmaChannel::new(1),
                DmaChannel::new(2),
                DmaChannel::new(3),
            ],
        }
    }
}

impl Dma {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read a DMA register halfword. Only `DMAxCNT_H` is readable; the address
    /// and count registers are write-only.
    pub fn read_register(&self, offset: u32) -> u16 {
        let (channel, sub) = decode(offset);
        if channel >= 4 {
            return 0;
        }
        match sub {
            10 => self.channels[channel].control,
            _ => 0,
        }
    }

    /// Apply a masked halfword write to a DMA register.
    pub fn write_register(&mut self, offset: u32, value: u16, mask: u16) {
        let (channel, sub) = decode(offset);
        if channel >= 4 {
            return;
        }
        let ch = &mut self.channels[channel];
        match sub {
            0 => ch.source = merge32(ch.source, value, mask, false),
            2 => ch.source = merge32(ch.source, value, mask, true),
            4 => ch.dest = merge32(ch.dest, value, mask, false),
            6 => ch.dest = merge32(ch.dest, value, mask, true),
            8 => ch.count_reg = merge16(ch.count_reg, value, mask),
            10 => ch.write_control(merge16(ch.control, value, mask)),
            _ => {}
        }
    }
}

/// Decode a DMA I/O offset into `(channel, sub-offset)`. Channel blocks are 12
/// bytes apart starting at `0xB0`; the sub-offset selects SAD/DAD/CNT_L/CNT_H.
fn decode(offset: u32) -> (usize, u32) {
    let rel = offset.wrapping_sub(0xB0);
    ((rel / 0x0C) as usize, rel % 0x0C)
}

fn merge16(current: u16, value: u16, mask: u16) -> u16 {
    (current & !mask) | (value & mask)
}

fn merge32(current: u32, value: u16, mask: u16, high: bool) -> u32 {
    let shift = if high { 16 } else { 0 };
    let m = (mask as u32) << shift;
    (current & !m) | (((value as u32) << shift) & m)
}
