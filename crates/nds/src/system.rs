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
use crate::dma::Dma;
use crate::interrupt::Interrupts;
use crate::ipc::Ipc;
use crate::memory::{is_vram, Core};
use crate::ppu::{Ppu, PpuEvent};
use crate::timer::{TimerId, Timers};
use crate::vram::Vram;
use crate::{Cp15, Memory};

/// Events on the shared DS timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NdsEvent {
    /// A timer reached its overflow on `core`. `generation` distinguishes it from
    /// events left stale by a reconfiguration.
    TimerOverflow {
        core: Core,
        timer: TimerId,
        generation: u32,
    },
    /// A 2D-engine scanline boundary (line start or H-blank).
    Ppu(PpuEvent),
}

/// Everything reachable through the bus: the memory image, CP15, the per-core
/// local clocks, and (later) the devices. It is the timeline's [`EventHandler`].
pub struct Machine {
    pub(crate) memory: Memory,
    pub(crate) cp15: Cp15,
    /// Per-core interrupt controllers, indexed by [`Core::index`].
    pub(crate) interrupts: [Interrupts; 2],
    /// Per-core timer banks, indexed by [`Core::index`].
    pub(crate) timers: [Timers; 2],
    /// Per-core DMA controllers, indexed by [`Core::index`].
    pub(crate) dma: [Dma; 2],
    pub(crate) vram: Vram,
    pub(crate) ppu: Ppu,
    pub(crate) ipc: Ipc,
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
            interrupts: [Interrupts::new(), Interrupts::new()],
            timers: [Timers::new(Core::Arm9), Timers::new(Core::Arm7)],
            dma: [Dma::new(), Dma::new()],
            vram: Vram::new(),
            ppu: Ppu::new(),
            ipc: Ipc::new(),
            clock: [0; 2],
        }
    }

    /// A DMA/CPU data read through `core`'s map. VRAM routes through the bank
    /// engine; instruction-fetch TCM rules do not apply.
    pub(crate) fn data_read(&self, core: Core, addr: u32, bytes: u32) -> u32 {
        if is_vram(addr) {
            return self.vram.read(addr, bytes);
        }
        match bytes {
            1 => self.memory.read8(core, addr, false, &self.cp15) as u32,
            2 => self.memory.read16(core, addr, false, &self.cp15) as u32,
            _ => self.memory.read32(core, addr, false, &self.cp15),
        }
    }

    /// The write counterpart to [`Self::data_read`].
    pub(crate) fn data_write(&mut self, core: Core, addr: u32, value: u32, bytes: u32) {
        if is_vram(addr) {
            self.vram.write(addr, value, bytes);
            return;
        }
        match bytes {
            1 => self.memory.write8(core, addr, value as u8, &self.cp15),
            2 => self.memory.write16(core, addr, value as u16, &self.cp15),
            _ => self.memory.write32(core, addr, value, &self.cp15),
        }
    }

    /// Run a DMA channel's transfer to completion as a bus master on `core`.
    fn run_dma_channel(&mut self, core: Core, channel: usize) {
        let c = core.index();
        let ch = self.dma[c].channels[channel]; // config snapshot
        let bytes = ch.unit_bytes();
        let source_step = ch.source_step();
        let dest_step = ch.dest_step();
        let mut source = ch.internal_source();
        let mut dest = ch.internal_dest();
        for _ in 0..ch.internal_count() {
            let value = self.data_read(core, source, bytes);
            self.data_write(core, dest, value, bytes);
            source = source.wrapping_add(source_step);
            dest = dest.wrapping_add(dest_step);
        }
        self.dma[c].channels[channel].complete(source, dest);
        if ch.irq_on_end() {
            self.interrupts[c].request(crate::dma::irq_source(channel));
        }
    }

    /// Read from a core's I/O block. Registers are handled at their natural width
    /// (the CPU accesses them aligned); everything unmodelled reads as zero. Some
    /// reads have side effects (dequeuing `IPCFIFORECV`), hence `&mut self`.
    pub(crate) fn io_read(&mut self, core: Core, addr: u32, bytes: u32) -> u32 {
        let c = core.index();
        // Timers: 0x4000100..0x4000110, four bytes each (L = counter, H = control).
        if (0x0400_0100..0x0400_0110).contains(&addr) {
            let now = self.clock[c];
            let id = TimerId::from_index(((addr - 0x0400_0100) / 4) as usize);
            let counter = self.timers[c].read_counter(id, now) as u32;
            let control = self.timers[c].read_control(id) as u32;
            return match (bytes, addr & 2 != 0) {
                (4, _) => counter | (control << 16),
                (_, true) => control,
                (_, false) => counter,
            };
        }
        // DMA registers: 0x40000B0..0x40000E0 (four channels, 12 bytes each).
        if (0x0400_00B0..0x0400_00E0).contains(&addr) {
            let control = self.dma[c].read_register(addr & 0xFF) as u32;
            return if bytes == 4 { control << 16 } else { control };
        }
        match addr {
            0x0400_0000 => self.ppu.dispcnt(),
            0x0400_0004 => self.ppu.read_dispstat(c) as u32,
            0x0400_0006 => self.ppu.vcount() as u32,
            0x0400_0180 => self.ipc.read_sync(core) as u32,
            0x0400_0184 => self.ipc.read_fifocnt(core) as u32,
            0x0400_0208 => self.interrupts[c].ime() as u32,
            0x0400_0210 => self.interrupts[c].ie(),
            0x0400_0214 => self.interrupts[c].iflags(),
            0x0410_0000 => self.ipc.recv(core, &mut self.interrupts),
            0x0400_0241 if core == Core::Arm7 => self.memory.wramcnt as u32,
            _ => 0,
        }
    }

    /// Write to a core's I/O block, handled at natural width; unmodelled writes
    /// are ignored. Timer-control writes can schedule an overflow, so the shared
    /// scheduler is threaded in.
    pub(crate) fn io_write(
        &mut self,
        core: Core,
        addr: u32,
        value: u32,
        bytes: u32,
        scheduler: &mut Scheduler<NdsEvent>,
    ) {
        let c = core.index();
        if (0x0400_0100..0x0400_0110).contains(&addr) {
            let now = self.clock[c];
            let id = TimerId::from_index(((addr - 0x0400_0100) / 4) as usize);
            match (bytes, addr & 2 != 0) {
                (4, _) => {
                    self.timers[c].write_reload(id, value as u16);
                    self.timers[c].write_control(id, (value >> 16) as u16, now, scheduler);
                }
                (_, true) => self.timers[c].write_control(id, value as u16, now, scheduler),
                (_, false) => self.timers[c].write_reload(id, value as u16),
            }
            return;
        }
        // DMA registers: writing DMAxCNT_H may arm an immediate transfer, which
        // runs to completion here.
        if (0x0400_00B0..0x0400_00E0).contains(&addr) {
            let base = addr & 0xFF;
            let armed = if bytes == 4 {
                self.dma[c].write_register(base, value as u16);
                self.dma[c].write_register(base + 2, (value >> 16) as u16)
            } else {
                self.dma[c].write_register(base, value as u16)
            };
            if let Some(channel) = armed {
                self.run_dma_channel(core, channel);
            }
            return;
        }
        // The VRAMCNT_A..I block (with WRAMCNT sharing address 0x4000247), all
        // byte registers on the ARM9. Decompose to bytes so any access width works.
        if core == Core::Arm9 && (0x0400_0240..0x0400_024A).contains(&addr) {
            for i in 0..bytes {
                let a = addr + i;
                let byte = (value >> (8 * i)) as u8;
                match a {
                    0x0400_0247 => self.memory.wramcnt = byte & 3,
                    _ => {
                        if let Some(bank) = vramcnt_bank(a) {
                            self.vram.set_control(bank, byte);
                        }
                    }
                }
            }
            return;
        }
        match addr {
            0x0400_0000 if core == Core::Arm9 => self.ppu.write_dispcnt(value, bytes),
            0x0400_0004 => self.ppu.write_dispstat(c, value as u16),
            0x0400_0180 => self.ipc.write_sync(core, value as u16, &mut self.interrupts),
            0x0400_0184 => self.ipc.write_fifocnt(core, value as u16, &mut self.interrupts),
            0x0400_0188 => self.ipc.send(core, value, &mut self.interrupts),
            0x0400_0208 => self.interrupts[c].set_ime(value & 1 != 0),
            0x0400_0210 => self.interrupts[c].set_ie(value),
            0x0400_0214 => self.interrupts[c].acknowledge(value),
            _ => {}
        }
    }
}

/// The VRAM block a `VRAMCNT` address configures (A–G at `0x4000240`-`246`, then
/// `0x4000247` is WRAMCNT, and H/I at `0x4000248`/`249`).
fn vramcnt_bank(addr: u32) -> Option<usize> {
    match addr {
        0x0400_0240..=0x0400_0246 => Some((addr - 0x0400_0240) as usize), // A–G
        0x0400_0248 => Some(7),                                           // H
        0x0400_0249 => Some(8),                                           // I
        _ => None,
    }
}

impl EventHandler<NdsEvent> for Machine {
    fn handle(&mut self, event: NdsEvent, ctx: &mut EventContext<'_, NdsEvent>) {
        match event {
            NdsEvent::TimerOverflow { core, timer, generation } => {
                let c = core.index();
                // `timers[c]` and `interrupts[c]` are disjoint fields.
                self.timers[c].handle_overflow(timer, generation, &mut self.interrupts[c], ctx);
            }
            NdsEvent::Ppu(event) => {
                // `ppu`, `interrupts`, and `vram` are disjoint fields.
                self.ppu.handle_event(event, &mut self.interrupts, &self.vram, ctx);
            }
        }
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

    /// Execute one instruction on `core` against its view of the machine, first
    /// accepting a pending interrupt at the boundary if the core allows it.
    fn step_core(&mut self, core: Core) {
        let asserted = self.machine.interrupts[core.index()].line_asserted();
        let cpu = match core {
            Core::Arm9 => &mut self.arm9,
            Core::Arm7 => &mut self.arm7,
        };
        if asserted && cpu.irq_enabled() {
            cpu.take_irq();
        }
        let mut bus = NdsCpuBus {
            machine: &mut self.machine,
            scheduler: &mut self.scheduler,
            core,
        };
        cpu.step(&mut bus);
    }

    /// Begin the PPU's continuous scanline schedule (idempotent).
    pub fn start_video(&mut self) {
        self.machine.ppu.start(&mut self.scheduler);
    }

    /// Run until the 2D engine completes one frame (into the next V-blank).
    pub fn run_frame(&mut self) {
        self.start_video();
        let start = self.machine.ppu.frame();
        for _ in 0..(crate::ppu::HEIGHT as u64 + 100) {
            if self.machine.ppu.frame() != start {
                break;
            }
            let target = self.scheduler.now() + crate::ppu::CYCLES_PER_LINE;
            self.run_until(target);
        }
    }

    /// Engine A's current output image, in BGR555.
    pub fn framebuffer(&self) -> &[u16] {
        self.machine.ppu.framebuffer()
    }

    /// The completed-frame counter.
    pub fn frame(&self) -> u64 {
        self.machine.ppu.frame()
    }

    /// A core's interrupt controller, for inspection and test setup.
    pub fn interrupts(&self, core: Core) -> &Interrupts {
        &self.machine.interrupts[core.index()]
    }

    /// Perform an I/O write as `core` would (for driving devices in tests).
    pub fn io_write(&mut self, core: Core, addr: u32, value: u32, bytes: u32) {
        self.machine
            .io_write(core, addr, value, bytes, &mut self.scheduler);
    }

    /// Perform an I/O read as `core` would.
    pub fn io_read(&mut self, core: Core, addr: u32, bytes: u32) -> u32 {
        self.machine.io_read(core, addr, bytes)
    }

    /// Read memory/VRAM as `core`'s CPU would (I/O routes to [`Self::io_read`]).
    pub fn read(&mut self, core: Core, addr: u32, bytes: u32) -> u32 {
        if (0x0400_0000..0x0500_0000).contains(&addr) {
            self.machine.io_read(core, addr, bytes)
        } else {
            self.machine.data_read(core, addr, bytes)
        }
    }

    /// Write memory/VRAM as `core`'s CPU would (I/O routes to [`Self::io_write`]).
    pub fn write(&mut self, core: Core, addr: u32, value: u32, bytes: u32) {
        if (0x0400_0000..0x0500_0000).contains(&addr) {
            self.io_write(core, addr, value, bytes);
        } else {
            self.machine.data_write(core, addr, value, bytes);
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
    fn ipc_fifo_handshake_round_trips_through_the_timeline() {
        use crate::IrqSource;
        const FIFOCNT: u32 = 0x0400_0184;
        const SEND: u32 = 0x0400_0188;
        const RECV: u32 = 0x0410_0000;

        let mut system = System::new();
        // Both cores park in a self-branch and keep the timeline moving.
        load_main(&mut system, 0x0000, &[0xEAFF_FFFE]); // arm9: b .
        load_main(&mut system, 0x0040, &[0xEAFF_FFFE]); // arm7: b .
        system.arm9.set_pc(0x0200_0000);
        system.arm7.set_pc(0x0200_0040);

        // Enable both FIFOs; the ARM7 also enables its receive-not-empty IRQ and
        // arms its interrupt controller.
        system.io_write(Core::Arm9, FIFOCNT, 1 << 15, 2);
        system.io_write(Core::Arm7, FIFOCNT, (1 << 15) | (1 << 10), 2);
        system.io_write(Core::Arm7, 0x0400_0210, IrqSource::IpcRecvNotEmpty.mask(), 4); // IE
        system.io_write(Core::Arm7, 0x0400_0208, 1, 4); // IME

        // ARM9 sends a request word; run the interleaved timeline forward.
        system.io_write(Core::Arm9, SEND, 0x1234, 4);
        system.run_until(200);

        // The send raised the ARM7's receive interrupt, and it vectored to handle
        // it (the boundary acceptance fired mid-timeline).
        assert_eq!(system.arm7.mode(), Some(arm::cpu::Mode::Irq));
        // The ARM7 reads the request and replies with value + 1.
        assert_eq!(system.io_read(Core::Arm7, RECV, 4), 0x1234);
        system.io_write(Core::Arm7, SEND, 0x1235, 4);
        system.run_until(400);

        // The ARM9 reads the reply back — a full round-trip.
        assert_eq!(system.io_read(Core::Arm9, RECV, 4), 0x1235);
    }

    #[test]
    fn ipc_sync_irq_crosses_from_arm9_to_arm7() {
        use crate::IrqSource;
        const SYNC: u32 = 0x0400_0180;

        let mut system = System::new();
        // The ARM7 enables the sync IRQ (IPCSYNC bit 14) and arms its controller.
        system.io_write(Core::Arm7, SYNC, 1 << 14, 2);
        system.io_write(Core::Arm7, 0x0400_0210, IrqSource::IpcSync.mask(), 4);
        system.io_write(Core::Arm7, 0x0400_0208, 1, 4);
        assert!(!system.interrupts(Core::Arm7).line_asserted());

        // The ARM9 triggers a sync IRQ (bit 13) toward the ARM7.
        system.io_write(Core::Arm9, SYNC, 1 << 13, 2);
        assert!(system.interrupts(Core::Arm7).line_asserted());
    }

    #[test]
    fn timer_overflow_reloads_and_raises_a_per_core_irq() {
        use crate::IrqSource;
        // Timer 0 registers on each core: TM0CNT_L = 0x4000100, TM0CNT_H = 0x4000102.
        let mut system = System::new();
        // The ARM7 arms its controller for the Timer 0 interrupt.
        system.io_write(Core::Arm7, 0x0400_0210, IrqSource::Timer0.mask(), 4); // IE
        system.io_write(Core::Arm7, 0x0400_0208, 1, 4); // IME
        // Reload 0xFFFF (overflows after one increment = 2 master ticks), F/1,
        // IRQ enabled, start.
        system.io_write(Core::Arm7, 0x0400_0100, 0xFFFF, 2);
        system.io_write(Core::Arm7, 0x0400_0102, (1 << 7) | (1 << 6), 2);
        assert_eq!(system.interrupts(Core::Arm7).iflags(), 0);

        system.run_until(64);
        // The overflow fired on the ARM7 (and only there) and reloaded.
        assert_eq!(system.interrupts(Core::Arm7).iflags(), IrqSource::Timer0.mask());
        assert_eq!(system.interrupts(Core::Arm9).iflags(), 0);
        assert_eq!(system.io_read(Core::Arm7, 0x0400_0100, 2), 0xFFFF);
    }

    #[test]
    fn cascade_chains_timer0_into_timer1() {
        // Timer 0: reload 0xFFFF, F/1, running -> overflows every 2 master ticks.
        // Timer 1: count-up (cascade) -> increments once per Timer 0 overflow.
        let mut system = System::new();
        system.io_write(Core::Arm9, 0x0400_0100, 0xFFFF, 2); // TM0 reload
        system.io_write(Core::Arm9, 0x0400_0102, 1 << 7, 2); // TM0 start, F/1
        system.io_write(Core::Arm9, 0x0400_0104, 0, 2); // TM1 reload 0
        system.io_write(Core::Arm9, 0x0400_0106, (1 << 7) | (1 << 2), 2); // TM1 start + cascade

        // After ~5 Timer 0 overflows (10 master ticks) Timer 1 should read ~5.
        system.run_until(64);
        let timer1 = system.io_read(Core::Arm9, 0x0400_0104, 2);
        assert!(timer1 >= 4, "timer1 cascaded to {timer1}");
    }

    #[test]
    fn immediate_dma_copies_main_ram_on_both_cores() {
        use crate::IrqSource;
        // DMA0 registers: SAD 0x40000B0, DAD 0x40000B4, CNT_L 0x40000B8, CNT_H 0x40000BA.
        // Control for a 32-bit, immediate, IRQ-on-end, enabled transfer:
        const CNT_H: u32 = (1 << 15) | (1 << 14) | (1 << 10);

        let run_on = |core: Core, src: u32, dst: u32| {
            let mut system = System::new();
            // Seed four words at the source.
            let seed = [0x1111_1111u32, 0x2222_2222, 0x3333_3333, 0x4444_4444];
            for (i, w) in seed.iter().enumerate() {
                system.io_write(core, 0x0400_0210, IrqSource::Dma0.mask(), 4); // IE (idempotent)
                system.memory().main[(src as usize & 0x3F_FFFF) + i * 4..][..4]
                    .copy_from_slice(&w.to_le_bytes());
            }
            system.io_write(core, 0x0400_0208, 1, 4); // IME
            system.io_write(core, 0x0400_00B0, src, 4); // SAD
            system.io_write(core, 0x0400_00B4, dst, 4); // DAD
            system.io_write(core, 0x0400_00B8, 4, 2); // count = 4 words
            system.io_write(core, 0x0400_00BA, CNT_H, 2); // control -> runs now
            system
        };

        // Copy 0x0200_0000 -> 0x0200_1000 on each core independently.
        for core in [Core::Arm9, Core::Arm7] {
            let mut system = run_on(core, 0x0200_0000, 0x0200_1000);
            for i in 0..4u32 {
                let word = u32::from_le_bytes(
                    system.memory().main[0x1000 + i as usize * 4..][..4].try_into().unwrap(),
                );
                assert_eq!(word, [0x1111_1111, 0x2222_2222, 0x3333_3333, 0x4444_4444][i as usize]);
            }
            // Completion raised the DMA0 interrupt and disabled the channel.
            assert_eq!(system.interrupts(core).iflags(), IrqSource::Dma0.mask());
            assert_eq!(system.io_read(core, 0x0400_00BA, 2) & (1 << 15), 0);
        }
    }

    #[test]
    fn dma_transfers_main_ram_into_engine_a_bg_vram() {
        // Configure VRAM block A as 2D Engine-A BG VRAM at 0x0600_0000, then DMA a
        // small tile pattern from Main RAM into it — the point where DMA and the
        // VRAM bank engine meet.
        let mut system = System::new();
        system.io_write(Core::Arm9, 0x0400_0240, 0x80 | 1, 1); // VRAMCNT_A: enable, MST 1

        let pattern = [0x0A0A_0A0Au32, 0x0B0B_0B0B, 0x0C0C_0C0C, 0x0D0D_0D0D];
        for (i, w) in pattern.iter().enumerate() {
            system.memory().main[0x2000 + i * 4..][..4].copy_from_slice(&w.to_le_bytes());
        }
        system.io_write(Core::Arm9, 0x0400_00B0, 0x0200_2000, 4); // SAD
        system.io_write(Core::Arm9, 0x0400_00B4, 0x0600_0000, 4); // DAD -> Engine-A BG VRAM
        system.io_write(Core::Arm9, 0x0400_00B8, 4, 2); // 4 words
        system.io_write(Core::Arm9, 0x0400_00BA, (1 << 15) | (1 << 10), 2); // enable, 32-bit, immediate

        // The words landed in VRAM (via block A) and read back through the map.
        for (i, w) in pattern.iter().enumerate() {
            assert_eq!(system.read(Core::Arm9, 0x0600_0000 + i as u32 * 4, 4), *w);
        }
        // Re-routing block A to LCDC exposes the same bytes at its LCDC address.
        system.io_write(Core::Arm9, 0x0400_0240, 0x80, 1); // MST 0
        assert_eq!(system.read(Core::Arm9, 0x0680_0000, 4), pattern[0]);
    }

    #[test]
    fn vblank_fires_on_both_cores_and_vram_display_renders() {
        use crate::IrqSource;
        let mut system = System::new();
        // Both cores enable the V-blank interrupt (DISPSTAT bit 3) and arm.
        for core in [Core::Arm9, Core::Arm7] {
            system.io_write(core, 0x0400_0004, 1 << 3, 2); // DISPSTAT: VBlank IRQ enable
            system.io_write(core, 0x0400_0210, IrqSource::VBlank.mask(), 4); // IE
            system.io_write(core, 0x0400_0208, 1, 4); // IME
        }
        // Engine A in VRAM-display mode (DISPCNT bits 16-17 = 2) showing block A;
        // block A mapped as LCDC and seeded with a gradient.
        system.io_write(Core::Arm9, 0x0400_0240, 0x80, 1); // VRAMCNT_A: enable, MST 0 (LCDC)
        for i in 0..(crate::ppu::WIDTH * crate::ppu::HEIGHT) {
            system.write(Core::Arm9, 0x0680_0000 + i as u32 * 2, (i & 0x7FFF) as u32, 2);
        }
        system.io_write(Core::Arm9, 0x0400_0000, 2 << 16, 4); // DISPCNT: VRAM display, block A

        system.run_frame();

        // One frame completed; VCOUNT wrapped back into the visible region.
        assert_eq!(system.frame(), 1);
        assert!(system.io_read(Core::Arm9, 0x0400_0006, 2) as u16 <= 263);
        // The V-blank interrupt reached both cores.
        assert_eq!(system.interrupts(Core::Arm9).iflags(), IrqSource::VBlank.mask());
        assert_eq!(system.interrupts(Core::Arm7).iflags(), IrqSource::VBlank.mask());
        // The framebuffer holds the blitted gradient.
        let fb = system.framebuffer();
        assert_eq!(fb[0], 0);
        assert_eq!(fb[100], 100);
        assert_eq!(fb[0x1234], 0x1234);
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
