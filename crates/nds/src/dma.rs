//! Each core's four DMA channels.
//!
//! DMA is a bus master, not a memcpy: a transfer moves data through the same
//! memory map its core uses. This module holds each channel's register state and
//! configuration decode; the transfer loop lives on the machine, which owns both
//! the memory and this state.
//!
//! M2.2 models the immediate-timing transfer, run in full at the instant it is
//! armed (the scheduler design permits that initially). The blank timings need
//! the PPU (M2.3); the NDS9 21-bit counts and extra 3-bit mode field are later
//! refinements — counts here follow the GBA's 16-bit form.

/// When a channel's transfer starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaTiming {
    Immediate,
    VBlank,
    HBlank,
    /// NDS-specific timings (cart, geometry FIFO, …) — not yet modelled.
    Special,
}

/// One DMA channel's registers and internal transfer pointers.
#[derive(Clone, Copy, Debug)]
pub struct DmaChannel {
    source: u32,
    dest: u32,
    count_reg: u16,
    control: u16,
    internal_source: u32,
    internal_dest: u32,
    internal_count: u32,
    enabled: bool,
}

impl DmaChannel {
    fn new() -> Self {
        DmaChannel {
            source: 0,
            dest: 0,
            count_reg: 0,
            control: 0,
            internal_source: 0,
            internal_dest: 0,
            internal_count: 0,
            enabled: false,
        }
    }

    pub fn timing(&self) -> DmaTiming {
        match (self.control >> 12) & 3 {
            0 => DmaTiming::Immediate,
            1 => DmaTiming::VBlank,
            2 => DmaTiming::HBlank,
            _ => DmaTiming::Special,
        }
    }

    pub fn is_32bit(&self) -> bool {
        self.control & (1 << 10) != 0
    }

    pub fn unit_bytes(&self) -> u32 {
        if self.is_32bit() {
            4
        } else {
            2
        }
    }

    pub fn repeat(&self) -> bool {
        self.control & (1 << 9) != 0
    }

    /// Whether this channel is enabled and set to gamecard (cart-slot) DMA timing,
    /// which the cartridge controller triggers on a ROMCTRL block start. The start
    /// mode is a 3-bit field on the ARM9 (mode 5) and 2-bit on the ARM7 (mode 2)
    /// — distinct from the GBA-shaped [`DmaChannel::timing`] used by the other
    /// modes (GBATEK "DS DMA Transfers").
    pub fn is_cart_dma(&self, arm9: bool) -> bool {
        let enabled = self.control & (1 << 15) != 0;
        let cart = if arm9 {
            (self.control >> 11) & 7 == 5
        } else {
            (self.control >> 12) & 3 == 2
        };
        enabled && cart
    }

    /// Whether this is an ARM9 **GXFIFO DMA** (start mode 7): it streams packed
    /// geometry commands to the GXFIFO. Games submit heavy 3D scenes this way, so it
    /// must be handled distinctly from the GBA-shaped [`DmaChannel::timing`] modes.
    pub fn is_gxfifo(&self, arm9: bool) -> bool {
        arm9 && (self.control >> 11) & 7 == 7
    }

    pub fn irq_on_end(&self) -> bool {
        self.control & (1 << 14) != 0
    }

    /// The signed step applied to the source after each unit (increment,
    /// decrement, or fixed).
    pub fn source_step(&self) -> u32 {
        let unit = self.unit_bytes();
        match (self.control >> 7) & 3 {
            0 => unit,
            1 => unit.wrapping_neg(),
            _ => 0,
        }
    }

    /// The destination step (increment / increment-reload, decrement, or fixed).
    pub fn dest_step(&self) -> u32 {
        let unit = self.unit_bytes();
        match (self.control >> 5) & 3 {
            0 | 3 => unit,
            1 => unit.wrapping_neg(),
            _ => 0,
        }
    }

    pub fn dest_reloads(&self) -> bool {
        (self.control >> 5) & 3 == 3
    }

    fn masked_source(&self) -> u32 {
        (self.source & 0x0FFF_FFFF) & !(self.unit_bytes() - 1)
    }

    fn masked_dest(&self) -> u32 {
        (self.dest & 0x0FFF_FFFF) & !(self.unit_bytes() - 1)
    }

    /// The transfer length (a zero count means the maximum).
    pub fn latched_count(&self) -> u32 {
        let count = self.count_reg as u32;
        if count == 0 {
            0x1_0000
        } else {
            count
        }
    }

    // Live accessors used by the transfer loop.
    /// Debug: `(control, source, dest, count)` register snapshot for reporting.
    pub fn debug_regs(&self) -> (u16, u32, u32, u16) {
        (self.control, self.source, self.dest, self.count_reg)
    }
    pub fn internal_source(&self) -> u32 {
        self.internal_source
    }
    pub fn internal_dest(&self) -> u32 {
        self.internal_dest
    }
    pub fn internal_count(&self) -> u32 {
        self.internal_count
    }

    /// Apply a write to `DMAxCNT_H`. An enable edge latches the internal pointers;
    /// returns `true` if this armed an immediate-timing transfer.
    fn write_control(&mut self, new_control: u16, arm9: bool) -> bool {
        let was_enabled = self.enabled;
        self.control = new_control;
        let now_enabled = new_control & (1 << 15) != 0;
        let mut armed = false;
        if now_enabled && !was_enabled {
            self.internal_source = self.masked_source();
            self.internal_dest = self.masked_dest();
            self.internal_count = self.latched_count();
            // Immediate and (ARM9) GXFIFO DMAs run now — our FIFO drains synchronously,
            // so a GXFIFO DMA never back-pressures and can transfer in full at once.
            armed = self.timing() == DmaTiming::Immediate || self.is_gxfifo(arm9);
        }
        self.enabled = now_enabled;
        armed
    }

    /// Advance the internal pointers after a completed transfer and either re-arm
    /// (repeat, non-immediate) or disable the channel.
    pub fn complete(&mut self, source: u32, dest: u32) {
        self.internal_source = source;
        if self.repeat() && self.timing() != DmaTiming::Immediate {
            self.internal_count = self.latched_count();
            self.internal_dest = if self.dest_reloads() {
                self.masked_dest()
            } else {
                dest
            };
        } else {
            self.internal_dest = dest;
            self.enabled = false;
            self.control &= !(1 << 15);
        }
    }
}

/// One core's four DMA channels.
#[derive(Clone, Copy, Debug)]
pub struct Dma {
    pub(crate) channels: [DmaChannel; 4],
    /// `DMA_FILL0..3` (`0x40000E0`): per-channel fill words a DMA can use as its
    /// source. Stored and read back; used for fast zero/value fills.
    fill: [u32; 4],
}

impl Default for Dma {
    fn default() -> Self {
        Dma {
            channels: [DmaChannel::new(); 4],
            fill: [0; 4],
        }
    }
}

impl Dma {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read a `DMA_FILL` word (`offset` from `0x40000E0`).
    pub fn read_fill(&self, offset: u32) -> u32 {
        self.fill.get((offset / 4) as usize).copied().unwrap_or(0)
    }

    /// Write a `DMA_FILL` word (`offset` from `0x40000E0`).
    pub fn write_fill(&mut self, offset: u32, value: u32) {
        if let Some(slot) = self.fill.get_mut((offset / 4) as usize) {
            *slot = value;
        }
    }

    /// Read a DMA register halfword. Only `DMAxCNT_H` is readable.
    pub fn read_register(&self, offset: u32) -> u16 {
        let (channel, sub) = decode(offset);
        match (channel < 4).then_some(sub) {
            Some(10) => self.channels[channel].control,
            _ => 0,
        }
    }

    /// Apply a halfword write to a DMA register. Returns `Some(channel)` if the
    /// write armed an immediate transfer on that channel.
    pub fn write_register(&mut self, offset: u32, value: u16, arm9: bool) -> Option<usize> {
        let (channel, sub) = decode(offset);
        if channel >= 4 {
            return None;
        }
        let ch = &mut self.channels[channel];
        match sub {
            0 => ch.source = (ch.source & 0xFFFF_0000) | value as u32,
            2 => ch.source = (ch.source & 0x0000_FFFF) | (value as u32) << 16,
            4 => ch.dest = (ch.dest & 0xFFFF_0000) | value as u32,
            6 => ch.dest = (ch.dest & 0x0000_FFFF) | (value as u32) << 16,
            8 => ch.count_reg = value,
            10 if ch.write_control(value, arm9) => return Some(channel),
            _ => {}
        }
        None
    }
}

/// Decode a DMA I/O offset into `(channel, sub-offset)`. Channel blocks are 12
/// bytes apart starting at `0xB0`.
fn decode(offset: u32) -> (usize, u32) {
    let rel = offset.wrapping_sub(0xB0);
    ((rel / 0x0C) as usize, rel % 0x0C)
}

/// The interrupt source for a DMA channel's completion.
pub fn irq_source(channel: usize) -> crate::interrupt::IrqSource {
    use crate::interrupt::IrqSource;
    // DMA0..3 occupy IE/IF bits 8..11.
    match channel {
        0 => IrqSource::Dma0,
        1 => IrqSource::Dma1,
        2 => IrqSource::Dma2,
        _ => IrqSource::Dma3,
    }
}
