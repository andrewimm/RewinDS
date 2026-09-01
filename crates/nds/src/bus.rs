//! The per-step bus adapter bridging one ARM core to the shared DS machine.
//!
//! The ARM interpreter drives memory through [`arm::cpu::Bus`]. The DS has two
//! cores over one machine, so each step builds a short-lived [`NdsCpuBus`] tagged
//! with the active [`Core`]: reads and writes dispatch through that core's map,
//! every access advances that core's local clock (the ARM9 at the master rate,
//! the ARM7 at half), and coprocessor transfers on the ARM9 route to CP15 (the
//! ARM7 has no coprocessor, so they trap as Undefined via the trait defaults).

use arm::cpu::{Bus, Timed};
use emu_core::Scheduler;

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
    /// The ARM7 (ARM7TDMI) cost in ARM7 cycles for one memory access: no cache, a
    /// base waitstate from the region table (with the ARM7's 32-bit bus, `M32 = 1`),
    /// plus a +1 nonsequential penalty. `size_bits` is the access width. Grounded in
    /// GBATEK's DS memory timings.
    fn arm7_access_cost(&self, address: u32, size_bits: u32, sequential: bool) -> u32 {
        const M32: u32 = 1;
        let m16 = if size_bits > 16 { 2 } else { 1 };
        let base = match (address >> 24) & 0x0F {
            0x0 | 0x1 => M32,       // BIOS / low
            0x2 | 0x5 | 0x6 => m16, // Main RAM, palette, VRAM (no ARM7 cache)
            0x8..=0xA => m16 * 8,   // GBA slot (slow)
            _ => M32,               // WRAM, I/O, OAM, BIOS-high, etc.
        };
        if sequential {
            base
        } else {
            base + 1
        }
    }

    /// The ARM9 (ARM946E-S) cost in master ticks for one memory access. TCM is
    /// single-cycle; cacheable Main RAM is a hit (1) or a miss (a nonsequential-read
    /// line fill = 52 for 32-bit / 42 for 16-bit, a sequential-read miss = 36/34, a
    /// write miss = 8/4 with no line fill); other regions pay a base waitstate plus a
    /// +6 nonsequential penalty. Reads allocate a cache line; writes only probe (the
    /// ARM946E-S is read-allocate). `size_bits` is the access width; `is_code` selects
    /// the i-cache, otherwise the d-cache. Grounded in GBATEK's DS memory timings and
    /// the ARM946E-S cache architecture.
    fn arm9_access_cost(
        &mut self,
        address: u32,
        size_bits: u32,
        is_code: bool,
        is_read: bool,
        sequential: bool,
    ) -> u32 {
        const MC: u32 = 1; // cached / TCM speed
        const M32: u32 = 2; // ARM9 32-bit bus cycle
        let m16 = if size_bits > 16 { M32 * 2 } else { M32 }; // 4 (32-bit) or 2 (≤16-bit)

        let cp15 = &self.machine.cp15;
        let in_itcm = cp15.itcm_enabled() && address < cp15.itcm_size();
        let in_dtcm = cp15.dtcm_enabled()
            && address >= cp15.dtcm_base()
            && address < cp15.dtcm_base().wrapping_add(cp15.dtcm_size());
        // ITCM serves code and data; DTCM serves data only.
        if in_itcm || (!is_code && in_dtcm) {
            return MC;
        }

        // Cacheable Main RAM (0x02xxxxxx).
        if address & 0x0F00_0000 == 0x0200_0000 {
            let cached = if is_code {
                self.machine.icache.access(address)
            } else if is_read {
                self.machine.dcache.access(address)
            } else {
                self.machine.dcache.contains(address) // writes probe, never allocate
            };
            if cached {
                return MC;
            }
            let mut c = if sequential && !is_code {
                m16 // sequential-data bonus (read or write)
            } else if is_read {
                m16 * 5 // nonsequential read
            } else {
                m16 * 2 // write (no line fill; write buffer unmodelled)
            };
            if is_read {
                c += 8 * M32 * 2; // 32-byte line fill = +32
            }
            return c;
        }

        // Non-cached regions: a base waitstate (the DS memory-timing region table,
        // repeating every 16 of the 256 map slots) plus the ARM9 nonsequential penalty.
        let base = match (address >> 24) & 0x0F {
            0x0 | 0x1 => MC,        // ITCM window / BIOS mirror
            0x3 | 0x4 | 0x7 => M32, // WRAM, I/O registers, OAM
            0x5 | 0x6 => m16,       // palette, VRAM
            0x8..=0xA => m16 * 8,   // GBA slot (slow)
            _ => M32,               // 0x2 handled above; 0xB..0xF incl BIOS
        };
        if sequential {
            base
        } else {
            base + 6
        }
    }

    /// Account a data access into the active core's per-instruction timing state
    /// (combined at the boundary via the pipeline model). The access is sequential
    /// iff its address is the previous data access's address plus its width.
    fn advance_data(&mut self, address: u32, width: u32, _sequential: bool, is_read: bool) {
        let c = self.core.index();
        let seq = address == self.machine.timing[c].data_last.wrapping_add(width);
        self.machine.timing[c].data_last = address;
        let cost = if self.core == Core::Arm9 {
            self.arm9_access_cost(address, width * 8, false, is_read, seq)
        } else {
            self.arm7_access_cost(address, width * 8, seq)
        };
        let t = &mut self.machine.timing[c];
        t.mem += cost;
        if is_read {
            t.did_load = true;
        } else {
            t.did_store = true;
        }
    }

    /// Account an opcode fetch as the active core's `cFetch`. The ARM9 always fetches
    /// 32 bits (even in Thumb); the ARM7 fetches at the instruction width.
    fn advance_code(&mut self, address: u32, width: u32) {
        #[cfg(feature = "cyctrace")]
        crate::system::cyctrace::record(
            self.core.index(),
            address,
            self.machine.clock[self.core.index()],
        );
        let c = self.core.index();
        // ARM9 fetches are always 32-bit and step by 4; ARM7 by its instruction width.
        let step = if self.core == Core::Arm9 {
            4
        } else {
            width / 8
        };
        // A branch target streams sequentially: the pipeline-refill penalty is already
        // in the branch's execute base, so it is not charged again as a nonsequential
        // fetch here.
        let seq = self.machine.timing[c].prev_branched
            || address == self.machine.timing[c].code_last.wrapping_add(step);
        self.machine.timing[c].code_last = address;
        self.machine.timing[c].fetch = if self.core == Core::Arm9 {
            self.arm9_access_cost(address, 32, true, true, seq)
        } else {
            self.arm7_access_cost(address, width, seq)
        };
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
        let value = match bytes {
            1 => mem.read8(self.core, address, instruction, cp15) as u32,
            2 => mem.read16(self.core, address, instruction, cp15) as u32,
            _ => mem.read32(self.core, address, instruction, cp15),
        };
        #[cfg(feature = "cyctrace")]
        if !instruction {
            let clock = self.machine.clock[self.core.index()];
            crate::system::cyctrace::watch_access(
                self.core.index() as u8,
                address,
                bytes,
                false,
                value,
                clock,
            );
        }
        value
    }

    fn write(&mut self, address: u32, value: u32, bytes: u32) {
        #[cfg(feature = "cyctrace")]
        {
            let clock = self.machine.clock[self.core.index()];
            crate::system::cyctrace::watch_access(
                self.core.index() as u8,
                address,
                bytes,
                true,
                value,
                clock,
            );
        }
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
            1 => self
                .machine
                .memory
                .write8(self.core, address, value as u8, &self.machine.cp15),
            2 => self
                .machine
                .memory
                .write16(self.core, address, value as u16, &self.machine.cp15),
            _ => self
                .machine
                .memory
                .write32(self.core, address, value, &self.machine.cp15),
        }
    }
}

impl Bus for NdsCpuBus<'_> {
    fn fetch32(&mut self, address: u32, _sequential: bool) -> Timed<u32> {
        let value = self.read(address, true, 4);
        self.advance_code(address, 32);
        Timed { value, cycles: 1 }
    }

    fn fetch16(&mut self, address: u32, _sequential: bool) -> Timed<u16> {
        let value = self.read(address, true, 2) as u16;
        self.advance_code(address, 16);
        Timed { value, cycles: 1 }
    }

    fn load32(&mut self, address: u32, sequential: bool) -> Timed<u32> {
        let value = self.read(address, false, 4);
        self.advance_data(address, 4, sequential, true);
        Timed { value, cycles: 1 }
    }

    fn load16(&mut self, address: u32, sequential: bool) -> Timed<u16> {
        let value = self.read(address, false, 2) as u16;
        self.advance_data(address, 2, sequential, true);
        Timed { value, cycles: 1 }
    }

    fn load8(&mut self, address: u32, sequential: bool) -> Timed<u8> {
        let value = self.read(address, false, 1) as u8;
        self.advance_data(address, 1, sequential, true);
        Timed { value, cycles: 1 }
    }

    fn store32(&mut self, address: u32, value: u32, sequential: bool) -> u32 {
        self.write(address, value, 4);
        self.advance_data(address, 4, sequential, false);
        1
    }

    fn store16(&mut self, address: u32, value: u16, sequential: bool) -> u32 {
        self.write(address, value as u32, 2);
        self.advance_data(address, 2, sequential, false);
        1
    }

    fn store8(&mut self, address: u32, value: u8, sequential: bool) -> u32 {
        self.write(address, value as u32, 1);
        self.advance_data(address, 1, sequential, false);
        1
    }

    fn internal(&mut self, cycles: u32) {
        self.machine.timing[self.core.index()].internal += cycles;
    }

    fn coprocessor_read(
        &mut self,
        cp: u8,
        opcode1: u8,
        crn: u8,
        crm: u8,
        opcode2: u8,
    ) -> Option<u32> {
        if self.core == Core::Arm9 && cp == 15 {
            // MRC executes in 2 cycles on the ARM946E-S, independent of any fetch.
            let t = &mut self.machine.timing[0];
            t.internal = t.internal.max(2);
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
            // MCR executes in 2 cycles on the ARM946E-S, independent of any fetch.
            let t = &mut self.machine.timing[0];
            t.internal = t.internal.max(2);
            self.machine.cp15.write(opcode1, crn, crm, opcode2, value)
        } else {
            false
        }
    }
}
