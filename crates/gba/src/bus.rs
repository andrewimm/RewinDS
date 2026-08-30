//! The GBA memory bus: the address map, its backing storage, and access timing.
//!
//! Every access carries an [`Access`] describing the master, kind, and sequence,
//! and returns a [`BusResult`] with the value, the guest cycles consumed, and
//! whether it changed the scheduler's deadline. The bus decodes the address to a
//! region, applies that region's mirroring and access rules, and — for the I/O
//! region — hands off to the [`Io`] MMIO subsystem.

use crate::event::EventKind;
use crate::io::Io;
use emu_core::{Access, AccessSequence, AccessWidth, BusResult, Scheduler};

// Region sizes.
const BIOS_SIZE: usize = 0x4000; // 16 KiB
const EWRAM_SIZE: usize = 0x4_0000; // 256 KiB
const IWRAM_SIZE: usize = 0x8000; // 32 KiB
const PALETTE_SIZE: usize = 0x400; // 1 KiB
const VRAM_SIZE: usize = 0x1_8000; // 96 KiB
const OAM_SIZE: usize = 0x400; // 1 KiB
const SRAM_SIZE: usize = 0x1_0000; // 64 KiB

/// The bus's backing storage. Cartridge ROM is loaded in; the rest is RAM.
#[derive(Clone, Debug)]
pub struct Memory {
    pub bios: Box<[u8]>,
    pub ewram: Box<[u8]>,
    pub iwram: Box<[u8]>,
    pub palette: Box<[u8]>,
    pub vram: Box<[u8]>,
    pub oam: Box<[u8]>,
    pub sram: Box<[u8]>,
    pub rom: Vec<u8>,
}

impl Default for Memory {
    fn default() -> Self {
        Memory {
            bios: vec![0; BIOS_SIZE].into_boxed_slice(),
            ewram: vec![0; EWRAM_SIZE].into_boxed_slice(),
            iwram: vec![0; IWRAM_SIZE].into_boxed_slice(),
            palette: vec![0; PALETTE_SIZE].into_boxed_slice(),
            vram: vec![0; VRAM_SIZE].into_boxed_slice(),
            oam: vec![0; OAM_SIZE].into_boxed_slice(),
            sram: vec![0; SRAM_SIZE].into_boxed_slice(),
            rom: Vec::new(),
        }
    }
}

/// The GBA memory bus.
#[derive(Clone, Debug, Default)]
pub struct Bus {
    pub memory: Memory,
    pub io: Io,
}

impl Bus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load cartridge ROM.
    pub fn load_rom(&mut self, rom: Vec<u8>) {
        self.memory.rom = rom;
    }

    /// Load the BIOS image (up to 16 KiB).
    pub fn load_bios(&mut self, bios: &[u8]) {
        let len = bios.len().min(BIOS_SIZE);
        self.memory.bios[..len].copy_from_slice(&bios[..len]);
    }

    pub fn read8(&mut self, addr: u32, access: Access, scheduler: &mut Scheduler<EventKind>) -> BusResult<u8> {
        let r = self.read(addr, AccessWidth::Byte, access, scheduler);
        BusResult::plain(r.value as u8, r.cycles)
    }

    pub fn read16(&mut self, addr: u32, access: Access, scheduler: &mut Scheduler<EventKind>) -> BusResult<u16> {
        let r = self.read(addr, AccessWidth::Half, access, scheduler);
        BusResult::plain(r.value as u16, r.cycles)
    }

    pub fn read32(&mut self, addr: u32, access: Access, scheduler: &mut Scheduler<EventKind>) -> BusResult<u32> {
        self.read(addr, AccessWidth::Word, access, scheduler)
    }

    pub fn write8(&mut self, addr: u32, value: u8, access: Access, scheduler: &mut Scheduler<EventKind>) -> BusResult<()> {
        self.write(addr, value as u32, AccessWidth::Byte, access, scheduler)
    }

    pub fn write16(&mut self, addr: u32, value: u16, access: Access, scheduler: &mut Scheduler<EventKind>) -> BusResult<()> {
        self.write(addr, value as u32, AccessWidth::Half, access, scheduler)
    }

    pub fn write32(&mut self, addr: u32, value: u32, access: Access, scheduler: &mut Scheduler<EventKind>) -> BusResult<()> {
        self.write(addr, value, AccessWidth::Word, access, scheduler)
    }

    fn read(&mut self, addr: u32, width: AccessWidth, access: Access, scheduler: &mut Scheduler<EventKind>) -> BusResult<u32> {
        let addr = align(addr, width);
        match addr >> 24 {
            0x00 if (addr as usize) < BIOS_SIZE => {
                BusResult::plain(read_le(&self.memory.bios, addr as usize, width), 1)
            }
            0x02 => {
                let off = addr as usize & (EWRAM_SIZE - 1);
                BusResult::plain(read_le(&self.memory.ewram, off, width), fixed_cycles(3, 6, width))
            }
            0x03 => {
                let off = addr as usize & (IWRAM_SIZE - 1);
                BusResult::plain(read_le(&self.memory.iwram, off, width), 1)
            }
            0x04 => {
                if let Some(off) = io_offset(addr) {
                    let value = self.io.read(off, width, scheduler.now());
                    BusResult::plain(value, fixed_cycles(1, 1, width))
                } else {
                    BusResult::plain(OPEN_BUS, 1)
                }
            }
            0x05 => {
                let off = addr as usize & (PALETTE_SIZE - 1);
                BusResult::plain(read_le(&self.memory.palette, off, width), fixed_cycles(1, 2, width))
            }
            0x06 => BusResult::plain(read_le(&self.memory.vram, vram_offset(addr), width), fixed_cycles(1, 2, width)),
            0x07 => {
                let off = addr as usize & (OAM_SIZE - 1);
                BusResult::plain(read_le(&self.memory.oam, off, width), 1)
            }
            0x08..=0x0D => {
                let off = (addr & 0x01FF_FFFF) as usize;
                let value = if off + width.bytes() as usize <= self.memory.rom.len() {
                    read_le(&self.memory.rom, off, width)
                } else {
                    rom_open_bus(addr, width)
                };
                BusResult::plain(value, self.rom_cycles(addr >> 24, width, access.sequence))
            }
            0x0E | 0x0F => {
                let byte = self.memory.sram[addr as usize & (SRAM_SIZE - 1)];
                BusResult::plain(sram_replicate(byte, width), self.sram_cycles())
            }
            _ => BusResult::plain(OPEN_BUS, 1),
        }
    }

    fn write(&mut self, addr: u32, value: u32, width: AccessWidth, access: Access, scheduler: &mut Scheduler<EventKind>) -> BusResult<()> {
        let addr = align(addr, width);
        match addr >> 24 {
            // BIOS and ROM are read-only; writes are dropped.
            0x00 => BusResult::plain((), 1),
            0x02 => {
                let off = addr as usize & (EWRAM_SIZE - 1);
                write_le(&mut self.memory.ewram, off, width, value);
                BusResult::plain((), fixed_cycles(3, 6, width))
            }
            0x03 => {
                let off = addr as usize & (IWRAM_SIZE - 1);
                write_le(&mut self.memory.iwram, off, width, value);
                BusResult::plain((), 1)
            }
            0x04 => {
                let changed = if let Some(off) = io_offset(addr) {
                    self.io.write(off, width, value, scheduler)
                } else {
                    false
                };
                BusResult {
                    value: (),
                    cycles: fixed_cycles(1, 1, width),
                    scheduling_changed: changed,
                }
            }
            0x05 => {
                let off = addr as usize & (PALETTE_SIZE - 1);
                // A byte write duplicates into both halves of the halfword.
                write_duplicating(&mut self.memory.palette, off, width, value);
                BusResult::plain((), fixed_cycles(1, 2, width))
            }
            0x06 => {
                let off = vram_offset(addr);
                write_duplicating(&mut self.memory.vram, off, width, value);
                BusResult::plain((), fixed_cycles(1, 2, width))
            }
            0x07 => {
                // OAM ignores byte writes entirely.
                if width != AccessWidth::Byte {
                    let off = addr as usize & (OAM_SIZE - 1);
                    write_le(&mut self.memory.oam, off, width, value);
                }
                BusResult::plain((), 1)
            }
            0x08..=0x0D => BusResult::plain((), self.rom_cycles(addr >> 24, width, access.sequence)),
            0x0E | 0x0F => {
                // SRAM is an 8-bit bus: only the low byte is written.
                self.memory.sram[addr as usize & (SRAM_SIZE - 1)] = value as u8;
                BusResult::plain((), self.sram_cycles())
            }
            _ => BusResult::plain((), 1),
        }
    }

    /// Wait-state cycles for a gamepak ROM access.
    fn rom_cycles(&self, region: u32, width: AccessWidth, sequence: AccessSequence) -> u32 {
        let waitstate = match region {
            0x08 | 0x09 => 0,
            0x0A | 0x0B => 1,
            _ => 2,
        };
        let (nonseq, seq) = self.ws_waits(waitstate);
        let first = 1 + if sequence == AccessSequence::Sequential { seq } else { nonseq };
        match width {
            // A 32-bit access is two 16-bit accesses; the second is sequential.
            AccessWidth::Word => first + (1 + seq),
            _ => first,
        }
    }

    /// The (non-sequential, sequential) wait cycles for a gamepak wait-state
    /// region, decoded from `WAITCNT`.
    fn ws_waits(&self, waitstate: u32) -> (u32, u32) {
        const NONSEQ: [u32; 4] = [4, 3, 2, 8];
        let w = self.io.control.waitcnt();
        match waitstate {
            0 => (NONSEQ[((w >> 2) & 3) as usize], if (w >> 4) & 1 == 1 { 1 } else { 2 }),
            1 => (NONSEQ[((w >> 5) & 3) as usize], if (w >> 7) & 1 == 1 { 1 } else { 4 }),
            _ => (NONSEQ[((w >> 8) & 3) as usize], if (w >> 10) & 1 == 1 { 1 } else { 8 }),
        }
    }

    fn sram_cycles(&self) -> u32 {
        const NONSEQ: [u32; 4] = [4, 3, 2, 8];
        1 + NONSEQ[(self.io.control.waitcnt() & 3) as usize]
    }
}

/// Value returned for reads of unmapped addresses. Real open-bus returns the
/// last value on the bus; this is a placeholder until that is tracked.
const OPEN_BUS: u32 = 0;

/// Align an address down to its access width.
fn align(addr: u32, width: AccessWidth) -> u32 {
    addr & !(width.bytes() - 1)
}

/// The I/O offset for an address in the `04000000h` region, or `None` if it is
/// not a mapped register (the `04000800h` mirror is not yet modeled).
fn io_offset(addr: u32) -> Option<u32> {
    let offset = addr & 0x00FF_FFFF;
    (offset < 0x400).then_some(offset)
}

/// VRAM's 128 KiB window: the 96 KiB is followed by a 32 KiB mirror of its last
/// block, and the whole thing repeats every 128 KiB.
fn vram_offset(addr: u32) -> usize {
    let mut offset = addr as usize & 0x1_FFFF;
    if offset >= VRAM_SIZE {
        offset -= 0x8000;
    }
    offset
}

fn read_le(mem: &[u8], off: usize, width: AccessWidth) -> u32 {
    match width {
        AccessWidth::Byte => mem[off] as u32,
        AccessWidth::Half => u16::from_le_bytes([mem[off], mem[off + 1]]) as u32,
        AccessWidth::Word => {
            u32::from_le_bytes([mem[off], mem[off + 1], mem[off + 2], mem[off + 3]])
        }
    }
}

fn write_le(mem: &mut [u8], off: usize, width: AccessWidth, value: u32) {
    match width {
        AccessWidth::Byte => mem[off] = value as u8,
        AccessWidth::Half => mem[off..off + 2].copy_from_slice(&(value as u16).to_le_bytes()),
        AccessWidth::Word => mem[off..off + 4].copy_from_slice(&value.to_le_bytes()),
    }
}

/// Write to palette/VRAM, where an 8-bit write duplicates the byte across the
/// addressed halfword rather than writing a single byte.
fn write_duplicating(mem: &mut [u8], off: usize, width: AccessWidth, value: u32) {
    match width {
        AccessWidth::Byte => {
            let half = (value as u8 as u16) * 0x0101;
            write_le(mem, off & !1, AccessWidth::Half, half as u32);
        }
        _ => write_le(mem, off, width, value),
    }
}

/// The GamePak ROM open-bus value: reads of the address space beyond the loaded
/// ROM return the low bits of the halfword address.
fn rom_open_bus(addr: u32, width: AccessWidth) -> u32 {
    let half = |a: u32| (a >> 1) & 0xFFFF;
    match width {
        AccessWidth::Byte => (half(addr & !1) >> (8 * (addr & 1))) & 0xFF,
        AccessWidth::Half => half(addr),
        AccessWidth::Word => half(addr) | (half(addr + 2) << 16),
    }
}

/// SRAM is an 8-bit bus; wider reads see the byte replicated across the width.
fn sram_replicate(byte: u8, width: AccessWidth) -> u32 {
    let byte = byte as u32;
    match width {
        AccessWidth::Byte => byte,
        AccessWidth::Half => byte * 0x0101,
        AccessWidth::Word => byte * 0x0101_0101,
    }
}

/// Cycles for a fixed-timing region: `narrow` for 8/16-bit, `word` for 32-bit.
fn fixed_cycles(narrow: u32, word: u32, width: AccessWidth) -> u32 {
    match width {
        AccessWidth::Word => word,
        _ => narrow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interrupt::IrqSource;
    use emu_core::{AccessKind, AccessSequence};

    fn bus() -> (Bus, Scheduler<EventKind>) {
        (Bus::new(), Scheduler::new())
    }

    const CPU: Access = Access::cpu_data();

    #[test]
    fn ram_round_trips_all_widths() {
        let (mut b, mut s) = bus();
        b.write32(0x0300_0000, 0xDEAD_BEEF, CPU, &mut s);
        assert_eq!(b.read32(0x0300_0000, CPU, &mut s).value, 0xDEAD_BEEF);
        assert_eq!(b.read16(0x0300_0000, CPU, &mut s).value, 0xBEEF);
        assert_eq!(b.read16(0x0300_0002, CPU, &mut s).value, 0xDEAD);
        assert_eq!(b.read8(0x0300_0000, CPU, &mut s).value, 0xEF);
        assert_eq!(b.read8(0x0300_0003, CPU, &mut s).value, 0xDE);

        // EWRAM too.
        b.write16(0x0200_0100, 0x1234, CPU, &mut s);
        assert_eq!(b.read16(0x0200_0100, CPU, &mut s).value, 0x1234);
    }

    #[test]
    fn regions_mirror() {
        let (mut b, mut s) = bus();
        b.write32(0x0300_0000, 0xCAFEBABE, CPU, &mut s);
        // IWRAM mirrors every 0x8000.
        assert_eq!(b.read32(0x0300_8000, CPU, &mut s).value, 0xCAFEBABE);
        assert_eq!(b.read32(0x03FF_8000, CPU, &mut s).value, 0xCAFEBABE);

        // VRAM's 0x18000..0x20000 window mirrors 0x10000..0x18000.
        b.write16(0x0601_0000, 0xABCD, CPU, &mut s);
        assert_eq!(b.read16(0x0601_8000, CPU, &mut s).value, 0xABCD);
    }

    #[test]
    fn byte_write_quirks() {
        let (mut b, mut s) = bus();
        // Palette: a byte write duplicates across the halfword.
        b.write8(0x0500_0000, 0xAB, CPU, &mut s);
        assert_eq!(b.read16(0x0500_0000, CPU, &mut s).value, 0xABAB);

        // OAM: byte writes are ignored.
        b.write16(0x0700_0000, 0x1111, CPU, &mut s);
        b.write8(0x0700_0000, 0xFF, CPU, &mut s);
        assert_eq!(b.read16(0x0700_0000, CPU, &mut s).value, 0x1111);
    }

    #[test]
    fn unmapped_and_rom_open_bus() {
        let (mut b, mut s) = bus();
        // Unmapped region reads as open bus (0 for now).
        assert_eq!(b.read32(0x1000_0000, CPU, &mut s).value, 0);
        // ROM with no cartridge returns the address-based pattern.
        assert_eq!(b.read16(0x0800_0000, CPU, &mut s).value, 0x0000);
        assert_eq!(b.read16(0x0800_0004, CPU, &mut s).value, 0x0002);
    }

    #[test]
    fn rom_reads_loaded_cartridge() {
        let (mut b, mut s) = bus();
        b.load_rom(vec![0x11, 0x22, 0x33, 0x44]);
        assert_eq!(b.read32(0x0800_0000, CPU, &mut s).value, 0x4433_2211);
        // ROM is read-only.
        b.write16(0x0800_0000, 0xFFFF, CPU, &mut s);
        assert_eq!(b.read16(0x0800_0000, CPU, &mut s).value, 0x2211);
    }

    #[test]
    fn mmio_interrupt_registers() {
        let (mut b, mut s) = bus();
        // IE / IME.
        b.write16(0x0400_0200, IrqSource::Timer0.mask(), CPU, &mut s);
        b.write16(0x0400_0208, 1, CPU, &mut s);
        assert_eq!(b.io.irq.ie(), IrqSource::Timer0.mask());
        assert!(b.io.irq.ime());
        assert_eq!(b.read16(0x0400_0200, CPU, &mut s).value, IrqSource::Timer0.mask());

        // IF is write-1-to-clear.
        b.io.irq.request(IrqSource::Timer0);
        b.io.irq.request(IrqSource::VBlank);
        b.write16(0x0400_0202, IrqSource::Timer0.mask(), CPU, &mut s);
        assert_eq!(b.io.irq.iflags(), IrqSource::VBlank.mask());
    }

    #[test]
    fn mmio_byte_write_only_touches_its_byte() {
        let (mut b, mut s) = bus();
        // Write IE=0xFFFF, then clear only the high byte with a byte write.
        b.write16(0x0400_0200, 0xFFFF, CPU, &mut s);
        b.write8(0x0400_0201, 0x00, CPU, &mut s);
        assert_eq!(b.io.irq.ie(), 0x00FF);
    }

    #[test]
    fn mmio_vcount_is_read_only() {
        let (mut b, mut s) = bus();
        b.write16(0x0400_0006, 0x00FF, CPU, &mut s); // VCOUNT
        assert_eq!(b.read16(0x0400_0006, CPU, &mut s).value, 0);
    }

    #[test]
    fn mmio_timer_control_write_schedules_and_reports_change() {
        let (mut b, mut s) = bus();
        b.write16(0x0400_0100, 0xFF00, CPU, &mut s); // TM0 reload -> overflow at 256
        let result = b.write16(0x0400_0102, (1 << 7) | (1 << 6), CPU, &mut s); // start + IRQ
        assert!(result.scheduling_changed);
        assert_eq!(s.next_deadline(), Some(256));
        // Reading TM0CNT_L returns the live counter, not the reload.
        s.set_now(100);
        assert_eq!(b.read16(0x0400_0100, CPU, &mut s).value, 0xFF00 + 100);
        // Reading control returns the parsed register.
        assert_eq!(b.read16(0x0400_0102, CPU, &mut s).value, (1 << 7) | (1 << 6));
    }

    #[test]
    fn timing_reflects_regions_and_waitstates() {
        let (mut b, mut s) = bus();
        // IWRAM: 1/1/1.
        assert_eq!(b.read32(0x0300_0000, CPU, &mut s).cycles, 1);
        // EWRAM: 3/3/6.
        assert_eq!(b.read8(0x0200_0000, CPU, &mut s).cycles, 3);
        assert_eq!(b.read32(0x0200_0000, CPU, &mut s).cycles, 6);
        // ROM WS0 default: 16-bit nonseq = 5, 32-bit nonseq = 8.
        let nonseq = Access::cpu(AccessKind::Data, AccessSequence::NonSequential);
        let seq = Access::cpu(AccessKind::Data, AccessSequence::Sequential);
        assert_eq!(b.read16(0x0800_0000, nonseq, &mut s).cycles, 5);
        assert_eq!(b.read16(0x0800_0000, seq, &mut s).cycles, 3);
        assert_eq!(b.read32(0x0800_0000, nonseq, &mut s).cycles, 8);
    }

    #[test]
    fn thirty_two_bit_mmio_spans_two_registers() {
        let (mut b, mut s) = bus();
        // A 32-bit write to 0x4000200 sets IE (low) and IF-ack (high, W1C).
        b.io.irq.request(IrqSource::VBlank); // IF bit 0 set
        b.write32(0x0400_0200, IrqSource::Timer0.mask() as u32 | (1 << 16), CPU, &mut s);
        assert_eq!(b.io.irq.ie(), IrqSource::Timer0.mask());
        // The high halfword wrote IF with bit 0 set -> acknowledged VBlank.
        assert_eq!(b.io.irq.iflags() & IrqSource::VBlank.mask(), 0);
    }
}
