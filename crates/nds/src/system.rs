//! The dual-core DS machine and its deterministic interleave.
//!
//! Two cores — the ARM9 (ARMv5TE) and ARM7 (ARMv4T) — share one [`Scheduler`]
//! timeline measured in master ticks (one ARM9 cycle each; the ARM7 advances two
//! per cycle). Between events the [`System`] runs the two cores **instruction by
//! instruction**, always stepping the one whose local clock is furthest behind
//! (ties to the ARM9), until both reach the next event deadline; then it settles
//! the master clock there and dispatches. Fine-grained and deterministic — the
//! shape a debugging-first emulator wants, and the interleave IPC will rely on.
//!
//! M2.0 is the harness: the cores, the shared bus and memory maps, CP15 + TCM.
//! Interrupts, timers, IPC, DMA, and video arrive in later milestones, at which
//! point [`NdsEvent`] and [`Machine`] grow the devices that schedule and service
//! events.

use arm::cpu::{ArmVersion, Cpu};
use emu_core::{EventContext, EventHandler, Scheduler, Timestamp};

use crate::bus::NdsCpuBus;
use crate::memory::Core;
use crate::{Cp15, Memory};

/// Events on the shared DS timeline. No device schedules events yet, so this is
/// uninhabited; milestones that add timers/PPU/IPC give it variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NdsEvent {}

/// Everything reachable through the bus: the memory image, CP15, the per-core
/// local clocks, and (later) the devices. It is the timeline's [`EventHandler`].
pub struct Machine {
    pub(crate) memory: Memory,
    pub(crate) cp15: Cp15,
    /// Per-core local clocks in master ticks, indexed by [`Core::index`]. They run
    /// ahead of the scheduler's `now` up to the current deadline; the barrier
    /// reconciles them.
    pub(crate) clock: [Timestamp; 2],
}

impl Machine {
    fn new() -> Self {
        Machine {
            memory: Memory::new(),
            cp15: Cp15::new(),
            clock: [0; 2],
        }
    }

    /// Read from the ARM9/ARM7 I/O block. Only the M2.0 registers exist; the rest
    /// reads as zero.
    pub(crate) fn io_read(&self, core: Core, addr: u32, bytes: u32) -> u32 {
        let mut value = 0;
        for i in 0..bytes {
            let a = addr + i;
            // WRAMSTAT (ARM7, 4000241h) reflects the Shared WRAM split.
            let byte = if core == Core::Arm7 && a == 0x0400_0241 {
                self.memory.wramcnt
            } else {
                0
            };
            value |= (byte as u32) << (8 * i);
        }
        value
    }

    /// Write to the I/O block. Only `WRAMCNT` (ARM9, 4000247h) is modelled.
    pub(crate) fn io_write(&mut self, core: Core, addr: u32, value: u32, bytes: u32) {
        for i in 0..bytes {
            let a = addr + i;
            let byte = (value >> (8 * i)) as u8;
            if core == Core::Arm9 && a == 0x0400_0247 {
                self.memory.wramcnt = byte & 3;
            }
        }
    }
}

impl EventHandler<NdsEvent> for Machine {
    fn handle(&mut self, event: NdsEvent, _ctx: &mut EventContext<'_, NdsEvent>) {
        // Uninhabited today; `match` proves there is nothing to service yet.
        match event {}
    }
}

/// The Nintendo DS: two cores, one shared timeline, one machine.
pub struct System {
    pub arm9: Cpu,
    pub arm7: Cpu,
    scheduler: Scheduler<NdsEvent>,
    machine: Machine,
}

impl Default for System {
    fn default() -> Self {
        System::new()
    }
}

impl System {
    /// A fresh DS: an ARMv5TE ARM9 and an ARMv4T ARM7 on a zeroed machine.
    pub fn new() -> Self {
        System {
            arm9: Cpu::with_version(ArmVersion::Armv5TE),
            arm7: Cpu::new(),
            scheduler: Scheduler::new(),
            machine: Machine::new(),
        }
    }

    /// The memory image, for loading code/data and inspection.
    pub fn memory(&mut self) -> &mut Memory {
        &mut self.machine.memory
    }

    /// The ARM9's CP15 coprocessor state.
    pub fn cp15(&self) -> &Cp15 {
        &self.machine.cp15
    }

    /// A core's local clock (master ticks).
    pub fn clock(&self, core: Core) -> Timestamp {
        self.machine.clock[core.index()]
    }

    /// The master timeline's current time (the last settled barrier).
    pub fn now(&self) -> Timestamp {
        self.scheduler.now()
    }

    /// Run the machine until the master clock reaches `target`, interleaving the
    /// two cores instruction-by-instruction between event deadlines.
    pub fn run_until(&mut self, target: Timestamp) {
        while self.scheduler.now() < target {
            let deadline = self
                .scheduler
                .next_deadline()
                .map_or(target, |d| d.min(target));

            // Step the more-behind core (ties to the ARM9) until both reach the
            // deadline. Every step advances the stepped core's clock, so this
            // terminates.
            while self.machine.clock[0].min(self.machine.clock[1]) < deadline {
                let core = if self.machine.clock[0] <= self.machine.clock[1] {
                    Core::Arm9
                } else {
                    Core::Arm7
                };
                self.step_core(core);
            }

            // Settle the master clock at the deadline and dispatch what is due.
            self.scheduler.set_now(deadline);
            self.scheduler.run_due_events(&mut self.machine);
        }
    }

    /// Execute one instruction on `core` against its view of the machine.
    fn step_core(&mut self, core: Core) {
        let mut bus = NdsCpuBus {
            machine: &mut self.machine,
            core,
        };
        match core {
            Core::Arm9 => self.arm9.step(&mut bus),
            Core::Arm7 => self.arm7.step(&mut bus),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load little-endian ARM words into Main RAM at `offset`.
    fn load_main(system: &mut System, offset: usize, program: &[u32]) {
        for (i, word) in program.iter().enumerate() {
            system.memory().main[offset + i * 4..offset + i * 4 + 4]
                .copy_from_slice(&word.to_le_bytes());
        }
    }

    /// A program that writes `value` to Main RAM at `0x0200_0000 + off`, then
    /// parks in a self-branch. `off` and `value` must be 8-bit immediates.
    fn store_program(off: u32, value: u32) -> [u32; 5] {
        [
            0xE3A0_1402,                 // mov r1, #0x02000000
            0xE281_1C00 | (off >> 8),    // add r1, r1, #off  (off = imm8 ROR 24)
            0xE3A0_0000 | (value & 0xFF), // mov r0, #value
            0xE581_0000,                 // str r0, [r1]
            0xEAFF_FFFE,                 // b .
        ]
    }

    #[test]
    fn dual_core_interleaves_and_shares_main_ram() {
        let run = || {
            let mut system = System::new();
            // ARM9 writes 0x91 -> 0x0200_0100; ARM7 writes 0x77 -> 0x0200_0200.
            load_main(&mut system, 0x0000, &store_program(0x0100, 0x91));
            load_main(&mut system, 0x0040, &store_program(0x0200, 0x77));
            system.arm9.set_pc(0x0200_0000);
            system.arm7.set_pc(0x0200_0040);
            system.run_until(400);
            system
        };

        let mut system = run();
        // Both cores executed and wrote into the shared Main RAM.
        assert_eq!(system.memory().main[0x0100], 0x91);
        assert_eq!(system.memory().main[0x0200], 0x77);
        assert_eq!(system.arm9.register(0), 0x91);
        assert_eq!(system.arm7.register(0), 0x77);
        // Both cores advanced; the master clock settled at the target.
        assert!(system.clock(Core::Arm9) >= 400);
        assert!(system.clock(Core::Arm7) >= 400);
        assert_eq!(system.now(), 400);

        // Deterministic: a second identical run yields identical state.
        let mut again = run();
        assert_eq!(system.arm9.register(1), again.arm9.register(1));
        assert_eq!(system.memory().main[0x0100], again.memory().main[0x0100]);
        assert_eq!(system.memory().main[0x0200], again.memory().main[0x0200]);
        assert_eq!(system.clock(Core::Arm7), again.clock(Core::Arm7));
    }

    #[test]
    fn arm9_relocates_dtcm_via_cp15_at_runtime() {
        let mut system = System::new();
        // The ARM9 configures DTCM at 0x0300_0000 (16 KB), enables it, then stores
        // to and reads back from that address — proving the relocation took.
        load_main(
            &mut system,
            0,
            &[
                0xE3A0_0403, // mov r0, #0x03000000
                0xE380_000A, // orr r0, r0, #0x0A        (base | size N=5)
                0xEE09_0F11, // mcr p15, 0, r0, c9, c1, 0 (DTCM base/size)
                0xE3A0_2801, // mov r2, #0x10000
                0xEE01_2F10, // mcr p15, 0, r2, c1, c0, 0 (control: DTCM enable, bit16)
                0xE3A0_1403, // mov r1, #0x03000000
                0xE3A0_005A, // mov r0, #0x5A
                0xE581_0000, // str r0, [r1]
                0xE591_3000, // ldr r3, [r1]
                0xEAFF_FFFE, // b .
            ],
        );
        system.arm9.set_pc(0x0200_0000);
        system.run_until(200);

        // CP15 took the configuration, and the value round-tripped through DTCM.
        assert!(system.cp15().dtcm_enabled());
        assert_eq!(system.cp15().dtcm_base(), 0x0300_0000);
        assert_eq!(system.arm9.register(3), 0x5A);
        // The store landed in DTCM, not the Shared WRAM sitting under that address.
        assert_eq!(system.memory().dtcm[0], 0x5A);
        assert_eq!(system.memory().shared_wram[0], 0);
    }
}
