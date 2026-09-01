//! The per-step bus adapter bridging one ARM core to the shared DS machine.
//!
//! The ARM interpreter drives memory through [`arm::cpu::Bus`]. The DS has two
//! cores over one machine, so each step builds a short-lived [`NdsCpuBus`] tagged
//! with the active [`Core`]: reads and writes dispatch through that core's map,
//! every access advances that core's local clock (the ARM9 at the master rate,
//! the ARM7 at half), and coprocessor transfers on the ARM9 route to CP15 (the
//! ARM7 has no coprocessor, so they trap as Undefined via the trait defaults).

use arm::cpu::{Bus, Timed};
use emu_core::{Scheduler, Timestamp};

use crate::memory::{is_vram, Core};
use crate::system::{Machine, NdsEvent};

/// One core's view of the DS machine for a single interpreter step. The shared
/// scheduler rides along so that MMIO writes which reschedule (a timer control
/// write) can reach it.
pub(crate) struct NdsCpuBus<'a> {
    pub machine: &'a mut Machine,
    pub scheduler: &'a mut Scheduler<NdsEvent>,
    pub core: Core,
}

/// The ARM9 I/O block. Only the handful of M2.0 registers are modelled; the rest
/// reads as zero and ignores writes.
fn is_io(address: u32) -> bool {
    (0x0400_0000..0x0500_0000).contains(&address)
}

impl NdsCpuBus<'_> {
    /// Master ticks per CPU cycle: the ARM9 runs at the master rate, the ARM7 at
    /// half, so each ARM7 cycle spans two master ticks.
    fn ticks_per_cycle(&self) -> Timestamp {
        match self.core {
            Core::Arm9 => 1,
            Core::Arm7 => 2,
        }
    }

    /// Advance the active core's local clock by `cycles` CPU cycles.
    fn advance(&mut self, cycles: u32) {
        self.machine.clock[self.core.index()] += cycles as Timestamp * self.ticks_per_cycle();
    }

    /// Master ticks (66 MHz units) for one **data** access, grounded in GBATEK's
    /// "DS Memory Timings" tables (given in 33 MHz cycles; ×2 for our ticks, and the
    /// ARM9's 0.5-cycle TCM = 1 tick). The ARM9's external-memory accesses carry a
    /// heavy waitstate (I/O/WRAM = 4 cycles, VRAM = 4, Main RAM = 9), whereas the
    /// ARM7 reaches WRAM/I/O in a single cycle — the asymmetry that governs the
    /// inter-core IPC handshake phase. (Opcode fetches are charged separately in
    /// [`Self::advance_code`], which models the ARM9 instruction cache.)
    fn data_ticks(&self, address: u32, width: u32, sequential: bool) -> Timestamp {
        let is32 = width == 4;
        if self.core == Core::Arm9 {
            // ARM9 TCM (ITCM at 0, DTCM at its movable base) is single-half-cycle.
            let cp15 = &self.machine.cp15;
            if (cp15.itcm_enabled() && address < cp15.itcm_size())
                || (cp15.dtcm_enabled()
                    && address >= cp15.dtcm_base()
                    && address < cp15.dtcm_base().wrapping_add(cp15.dtcm_size()))
            {
                return 1;
            }
            match address >> 24 {
                0x02 => {
                    if is32 {
                        if sequential { 4 } else { 20 }
                    } else if sequential { 2 } else { 18 }
                } // Main RAM
                0x05 | 0x06 => {
                    if is32 {
                        if sequential { 4 } else { 10 }
                    } else if sequential { 2 } else { 8 }
                } // Palette, VRAM
                _ => {
                    if sequential { 2 } else { 8 }
                } // WRAM, BIOS, I/O, OAM, GBA slot
            }
        } else {
            // ARM7 (33 MHz): 1 native cycle = 2 ticks. WRAM/BIOS/I/O/OAM are single
            // cycle; only Main RAM carries the nonsequential penalty.
            match address >> 24 {
                0x02 => {
                    if is32 {
                        if sequential { 4 } else { 20 }
                    } else if sequential { 2 } else { 18 }
                }
                _ => 2,
            }
        }
    }

    /// Charge the active core's clock for a data access. On the ARM9, cacheable
    /// Main RAM goes through the data-cache model (hit 0.5 cyc / cold miss 23 cyc)
    /// rather than paying the full nonsequential waitstate on every access — most
    /// game-data reads are cache hits, and charging them all as misses would leave
    /// the ARM9 far too slow.
    fn advance_data(&mut self, address: u32, width: u32, sequential: bool) {
        let in_tcm = self.core == Core::Arm9 && {
            let cp15 = &self.machine.cp15;
            (cp15.itcm_enabled() && address < cp15.itcm_size())
                || (cp15.dtcm_enabled()
                    && address >= cp15.dtcm_base()
                    && address < cp15.dtcm_base().wrapping_add(cp15.dtcm_size()))
        };
        let ticks = if self.core == Core::Arm9 && address >> 24 == 0x02 && !in_tcm {
            if self.machine.dcache.access(address) { 1 } else { 46 }
        } else {
            self.data_ticks(address, width, sequential)
        };
        self.machine.clock[self.core.index()] += ticks;
    }

    /// Charge the active core's clock for an opcode fetch. On the ARM9, code in
    /// cacheable Main RAM pays a cache hit (0.5 cyc) or a cold miss (23 cyc line
    /// fill) via the instruction-cache model; TCM is single-half-cycle; other
    /// regions carry their fetch waitstate. The ARM7 keeps its simple core-speed
    /// fetch (its hot code runs from single-cycle WRAM).
    fn advance_code(&mut self, address: u32, sequential: bool) {
        if self.core == Core::Arm9 {
            let cp15 = &self.machine.cp15;
            let ticks: Timestamp = if cp15.itcm_enabled() && address < cp15.itcm_size() {
                1 // ITCM: 0.5 cycle
            } else if address >> 24 == 0x02 {
                // Cacheable Main RAM: a cold line misses; a resident line hits, but
                // the ARM9 has no fast sequential code fetch — a nonsequential fetch
                // (a taken branch's target + pipeline refill) costs more than a
                // sequential one. Matches DeSmuME's per-instruction cycle accounting.
                if !self.machine.icache.access(address) {
                    46 // cold cache miss (line fill)
                } else if sequential {
                    1
                } else {
                    2
                }
            } else {
                8 // WRAM/BIOS/I/O code fetch: N32 = 4 cycles
            };
            self.machine.clock[0] += ticks;
        } else {
            self.advance(1); // ARM7: 1 cycle = 2 ticks
        }
    }

    fn read(&mut self, address: u32, instruction: bool, bytes: u32) -> u32 {
        // I/O reads may have side effects (e.g. dequeuing an IPC FIFO).
        if is_io(address) {
            return self.machine.io_read(self.core, address, bytes);
        }
        if is_vram(address) {
            return self.machine.vram.read(address, bytes);
        }
        let (mem, cp15) = (&self.machine.memory, &self.machine.cp15);
        match bytes {
            1 => mem.read8(self.core, address, instruction, cp15) as u32,
            2 => mem.read16(self.core, address, instruction, cp15) as u32,
            _ => mem.read32(self.core, address, instruction, cp15),
        }
    }

    fn write(&mut self, address: u32, value: u32, bytes: u32) {
        if is_io(address) {
            self.machine
                .io_write(self.core, address, value, bytes, self.scheduler);
            return;
        }
        if is_vram(address) {
            self.machine.vram.write(address, value, bytes);
            return;
        }
        // `memory` and `cp15` are disjoint fields, so the mutable memory borrow
        // (the receiver) and the shared cp15 borrow (the argument) coexist.
        match bytes {
            1 => self.machine.memory.write8(self.core, address, value as u8, &self.machine.cp15),
            2 => self.machine.memory.write16(self.core, address, value as u16, &self.machine.cp15),
            _ => self.machine.memory.write32(self.core, address, value, &self.machine.cp15),
        }
    }
}

impl Bus for NdsCpuBus<'_> {
    fn fetch32(&mut self, address: u32, sequential: bool) -> Timed<u32> {
        let value = self.read(address, true, 4);
        self.advance_code(address, sequential);
        Timed { value, cycles: 1 }
    }

    fn fetch16(&mut self, address: u32, sequential: bool) -> Timed<u16> {
        let value = self.read(address, true, 2) as u16;
        self.advance_code(address, sequential);
        Timed { value, cycles: 1 }
    }

    fn load32(&mut self, address: u32, sequential: bool) -> Timed<u32> {
        let value = self.read(address, false, 4);
        self.advance_data(address, 4, sequential);
        Timed { value, cycles: 1 }
    }

    fn load16(&mut self, address: u32, sequential: bool) -> Timed<u16> {
        let value = self.read(address, false, 2) as u16;
        self.advance_data(address, 2, sequential);
        Timed { value, cycles: 1 }
    }

    fn load8(&mut self, address: u32, sequential: bool) -> Timed<u8> {
        let value = self.read(address, false, 1) as u8;
        self.advance_data(address, 1, sequential);
        Timed { value, cycles: 1 }
    }

    fn store32(&mut self, address: u32, value: u32, sequential: bool) -> u32 {
        self.write(address, value, 4);
        self.advance_data(address, 4, sequential);
        1
    }

    fn store16(&mut self, address: u32, value: u16, sequential: bool) -> u32 {
        self.write(address, value as u32, 2);
        self.advance_data(address, 2, sequential);
        1
    }

    fn store8(&mut self, address: u32, value: u8, sequential: bool) -> u32 {
        self.write(address, value as u32, 1);
        self.advance_data(address, 1, sequential);
        1
    }

    fn internal(&mut self, cycles: u32) {
        self.advance(cycles);
    }

    fn coprocessor_read(&mut self, cp: u8, opcode1: u8, crn: u8, crm: u8, opcode2: u8) -> Option<u32> {
        if self.core == Core::Arm9 && cp == 15 {
            self.machine.cp15.read(opcode1, crn, crm, opcode2)
        } else {
            None
        }
    }

    fn coprocessor_write(
        &mut self,
        cp: u8,
        opcode1: u8,
        crn: u8,
        crm: u8,
        opcode2: u8,
        value: u32,
    ) -> bool {
        if self.core == Core::Arm9 && cp == 15 {
            self.machine.cp15.write(opcode1, crn, crm, opcode2, value)
        } else {
            false
        }
    }
}
