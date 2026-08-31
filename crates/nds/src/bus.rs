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
    fn fetch32(&mut self, address: u32, _sequential: bool) -> Timed<u32> {
        let value = self.read(address, true, 4);
        self.advance(1);
        Timed { value, cycles: 1 }
    }

    fn fetch16(&mut self, address: u32, _sequential: bool) -> Timed<u16> {
        let value = self.read(address, true, 2) as u16;
        self.advance(1);
        Timed { value, cycles: 1 }
    }

    fn load32(&mut self, address: u32, _sequential: bool) -> Timed<u32> {
        let value = self.read(address, false, 4);
        self.advance(1);
        Timed { value, cycles: 1 }
    }

    fn load16(&mut self, address: u32, _sequential: bool) -> Timed<u16> {
        let value = self.read(address, false, 2) as u16;
        self.advance(1);
        Timed { value, cycles: 1 }
    }

    fn load8(&mut self, address: u32, _sequential: bool) -> Timed<u8> {
        let value = self.read(address, false, 1) as u8;
        self.advance(1);
        Timed { value, cycles: 1 }
    }

    fn store32(&mut self, address: u32, value: u32, _sequential: bool) -> u32 {
        self.write(address, value, 4);
        self.advance(1);
        1
    }

    fn store16(&mut self, address: u32, value: u16, _sequential: bool) -> u32 {
        self.write(address, value as u32, 2);
        self.advance(1);
        1
    }

    fn store8(&mut self, address: u32, value: u8, _sequential: bool) -> u32 {
        self.write(address, value as u32, 1);
        self.advance(1);
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
