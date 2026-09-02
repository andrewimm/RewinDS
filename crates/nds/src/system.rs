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
use crate::cart::Cart;
use crate::dma::Dma;
use crate::interrupt::Interrupts;
use crate::ipc::Ipc;
use crate::memory::{is_vram, Core};
use crate::ppu::{Ppu, PpuEvent};
use crate::timer::{TimerId, Timers};
use crate::vram::Vram;
use crate::{Cp15, Memory};

/// Debug instrumentation (feature `cyctrace`) for investigating DS boot divergences:
/// an ARM9 opcode-fetch PC trace (diff against a reference to find where execution
/// splits), an ARM9 register trap, and a Main-RAM byte watch. Compiled out by
/// default; driven by `tests/cyc_capture.rs`.
#[cfg(feature = "cyctrace")]
pub mod cyctrace {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Mutex;

    /// Per-core opcode-fetch PC traces (enabled via [`enable`]), for divergence
    /// diffing against a reference. Index 0 = ARM9, index 1 = ARM7.
    pub static ENABLED: AtomicBool = AtomicBool::new(false);
    pub static TRACE: [Mutex<Vec<u32>>; 2] = [Mutex::new(Vec::new()), Mutex::new(Vec::new())];
    /// Master clock captured alongside each recorded PC (same index as [`TRACE`]),
    /// so a captured trace can be sliced by a clock window to locate a mistimed span.
    pub static CLOCKS: [Mutex<Vec<u64>>; 2] = [Mutex::new(Vec::new()), Mutex::new(Vec::new())];

    pub fn enable() {
        ENABLED.store(true, Ordering::Relaxed);
    }
    pub fn record(core: usize, pc: u32, clock: u64) {
        if ENABLED.load(Ordering::Relaxed) {
            TRACE[core].lock().unwrap().push(pc);
            CLOCKS[core].lock().unwrap().push(clock);
        }
    }
    pub fn len(core: usize) -> usize {
        TRACE[core].lock().unwrap().len()
    }
    /// A copy of a core's recorded PCs from index `from` to the end.
    pub fn slice(core: usize, from: usize) -> Vec<u32> {
        TRACE[core].lock().unwrap()[from..].to_vec()
    }
    pub fn dump(core: usize, path: &str) {
        use std::fmt::Write as _;
        let t = TRACE[core].lock().unwrap();
        let clk = CLOCKS[core].lock().unwrap();
        let mut s = String::with_capacity(t.len() * 18);
        for (pc, c) in t.iter().zip(clk.iter()) {
            let _ = writeln!(s, "{pc:08X} {c}");
        }
        std::fs::write(path, s).unwrap();
    }

    /// An ARM9 instruction address to trap on (registers snapshotted), or `u32::MAX`.
    pub static TRIGGER: AtomicU32 = AtomicU32::new(u32::MAX);
    pub static REGS: Mutex<Vec<[u32; 16]>> = Mutex::new(Vec::new());
    /// A byte address to watch, or `u32::MAX`. Reads/writes touching it are logged.
    pub static WATCH: AtomicU32 = AtomicU32::new(u32::MAX);
    pub static WATCH_LOG: Mutex<Vec<(u8, bool, u32, u64)>> = Mutex::new(Vec::new());

    pub fn trigger(addr: u32) {
        TRIGGER.store(addr, Ordering::Relaxed);
    }
    pub fn is_trigger(pc: u32) -> bool {
        pc == TRIGGER.load(Ordering::Relaxed)
    }
    pub fn snapshot(regs: [u32; 16]) {
        REGS.lock().unwrap().push(regs);
    }
    pub fn take_regs() -> Vec<[u32; 16]> {
        std::mem::take(&mut REGS.lock().unwrap())
    }
    pub fn watch(addr: u32) {
        WATCH.store(addr, Ordering::Relaxed);
    }

    /// The most recently fetched opcode PC per core (0 = ARM9, 1 = ARM7), updated on
    /// every fetch, so a bus-side hook (e.g. the AUXSPI backup) can attribute an
    /// access to the instruction that issued it without threading the CPU through.
    pub static CUR_PC: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
    /// Raw AUXSPI backup byte log: `(arm7_pc, byte_in, byte_out, still_in_transfer)`.
    pub static AUXLOG: Mutex<Vec<(u32, u8, u8, u8)>> = Mutex::new(Vec::new());
    pub fn aux_log(pc: u32, byte_in: u8, byte_out: u8, in_transfer: u8) {
        AUXLOG.lock().unwrap().push((pc, byte_in, byte_out, in_transfer));
    }
    pub fn take_aux_log() -> Vec<(u32, u8, u8, u8)> {
        std::mem::take(&mut AUXLOG.lock().unwrap())
    }
    pub fn watch_access(core: u8, address: u32, bytes: u32, is_write: bool, value: u32, clock: u64) {
        let target = WATCH.load(Ordering::Relaxed);
        if target == u32::MAX {
            return;
        }
        // Main RAM is compared modulo its 4 MB mirror; other regions exactly.
        let hit = if (address >> 24) & 0x0F == 0x02 && (target >> 24) & 0x0F == 0x02 {
            let off = address & 0x003F_FFFF;
            (off..off + bytes).contains(&(target & 0x003F_FFFF))
        } else {
            (address..address + bytes).contains(&target)
        };
        if hit {
            WATCH_LOG.lock().unwrap().push((core, is_write, value, clock));
        }
    }
    pub fn take_watch_log() -> Vec<(u8, bool, u32, u64)> {
        std::mem::take(&mut WATCH_LOG.lock().unwrap())
    }
}

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
    /// The sound mixer should emit one output sample.
    SoundSample,
}

/// Per-core per-instruction timing state for the ARM pipeline model, where an
/// instruction's cost is `max(cExecute, cFetch)` — the fetch and execute stages
/// overlap, so their throughput is the larger, not the sum. The ARM9 (5-stage
/// pipeline) also overlaps its ALU and memory stages, so `cExecute = max(aluBase,
/// mem)`; the ARM7 (3-stage) runs them in sequence, so `cExecute = aluBase + mem`.
/// The bus accumulates `fetch`/`mem`/`internal` and the load/store flags during a
/// step; the boundary combines them (with the CPU's taken-branch signal) into one
/// cost. `code_last`/`data_last` are the separate sequential-access trackers (an
/// access is sequential iff its address is the previous one plus its width).
#[derive(Default)]
pub(crate) struct CoreTiming {
    /// Code-fetch cost of this instruction (i-cache hit/miss on ARM9), the `cFetch`.
    pub fetch: u32,
    /// Sum of this instruction's data-access costs (d-cache hit/miss), the `mem` term.
    pub mem: u32,
    /// Internal (non-memory) execute cycles, e.g. multiply — combined for ops that
    /// touch no memory (a load's spurious load-use cycle is excluded via the flags).
    pub internal: u32,
    pub did_load: bool,
    pub did_store: bool,
    /// Last code-fetch / data-access addresses, for sequential-access detection.
    pub code_last: u32,
    pub data_last: u32,
    /// The previous instruction's `cExecute`, carried forward for the prefetch model:
    /// a step charges `max(cExecute(prev), cFetch(this))`, pairing this instruction's
    /// opcode fetch with the previous instruction's execute (hardware fetches this
    /// opcode while the previous one executes). Equivalent to the pipeline's
    /// `max(cExecute(I), cFetch(I+1))` re-associated across the boundary, so a taken
    /// branch's refill (its execute base) naturally absorbs the target's
    /// non-sequential fetch — no separate branch-target special case is needed.
    pub pending_execute: u32,
}

impl CoreTiming {
    /// Clear the per-instruction accumulators (keeping the sequential-access
    /// trackers, which persist across instructions).
    fn begin_step(&mut self) {
        self.fetch = 0;
        self.mem = 0;
        self.internal = 0;
        self.did_load = false;
        self.did_store = false;
    }

    /// This instruction's `cExecute` in the core's own cycles: the ALU base combined
    /// with the memory cost — the ARM9 by `max` (parallel 5-stage pipeline), the ARM7
    /// by `+` (sequential 3-stage). `branched` is the CPU's taken-branch signal
    /// (pipeline refill → execute base 3); `block` marks an `LDM`/`STM`/`PUSH`/`POP`.
    /// The opcode fetch is combined separately at the step boundary under the prefetch
    /// model (see [`Self::pending_execute`]).
    fn execute_cost(&self, branched: bool, block: bool, arm9: bool) -> u32 {
        // ALU base by instruction class. A single load and a taken branch both cost 3
        // (load base / pipeline refill); a single store costs 2; a plain ALU op 1. A
        // block transfer (LDM/STM/PUSH/POP) is cheaper regardless of register count:
        // LDM base 2 (4 when it loads PC, which also branches), STM base 1.
        let alu_base = if self.did_load {
            if block {
                // LDM: base 2, or 4 when it loads PC (which also branches).
                if branched {
                    4
                } else {
                    2
                }
            } else if branched {
                // A single load into PC (`LDR pc`, a function/exception return) refills
                // the pipeline: base 5, not the ordinary load's 3.
                5
            } else {
                3
            }
        } else if self.did_store {
            if block {
                1
            } else {
                2
            }
        } else if branched {
            3
        } else {
            1
        };
        // Multiply/other internal-cycle ops (no memory access) contribute their
        // internal cycles; a load/store's load-use cycle does not (its base already
        // reflects the load/store execute cost).
        let alu = if self.did_load || self.did_store {
            alu_base
        } else {
            alu_base.max(self.internal)
        };
        if arm9 {
            alu.max(self.mem)
        } else {
            alu + self.mem
        }
    }
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
    /// The gamecard slot: runtime ROM/filesystem streaming.
    pub(crate) cart: Cart,
    /// The ARM7 SPI bus (firmware flash — the touchscreen calibration source).
    pub(crate) spi: crate::spi::Spi,
    /// The ARM7 serial real-time clock (`0x4000138`).
    pub(crate) rtc: crate::rtc::Rtc,
    /// The 16-channel sound engine (`0x4000400`-`0x400051F`, ARM7).
    pub(crate) sound: crate::sound::Sound,
    /// The ARM9 3D graphics engine (geometry command FIFO + rasterizer).
    pub(crate) gpu3d: gpu3d::Gpu3d,
    /// Debug: total words streamed through GXFIFO DMA (confirms it is being used).
    pub(crate) gxfifo_dma_words: u64,
    /// The ARM9 hardware division and square-root units.
    pub(crate) math: crate::math::Math,
    /// ARM9 instruction- and data-cache timing models (hit/miss cycle costs only):
    /// the ARM946E-S 8 KB i-cache and 4 KB d-cache, both 4-way set-associative.
    pub(crate) icache: crate::icache::Cache,
    pub(crate) dcache: crate::icache::Cache,
    /// Per-core per-instruction timing accumulators (the `max(execute, fetch)`
    /// pipeline model), indexed by [`Core::index`]. The bus fills these during a step;
    /// [`System::step_core`] combines them into the core's clock at the boundary.
    pub(crate) timing: [CoreTiming; 2],
    /// `KEYINPUT` (`0x4000130`): the ten buttons, active-low (a set bit = released),
    /// readable by both cores.
    pub(crate) keyinput: u16,
    /// `EXTKEYIN` (`0x4000136`, ARM7): the X/Y buttons, pen-down, and hinge, all
    /// active-low with the unused bits set. Default = nothing pressed, pen up, hinge
    /// open; a booting game checks the pen bit before sampling the touchscreen.
    pub(crate) extkeyin: u16,
    /// `POSTFLG` (`0x4000300`) per core: bit 0 = boot completed. Retail games
    /// refuse to run while it reads 0; direct boot sets it (GBATEK).
    pub(crate) postflg: [u8; 2],
    /// `EXMEMCNT`/`EXMEMSTAT` (`0x4000204`): NDS/GBA-slot and main-memory arbitration.
    /// One shared value both cores read; the ARM9 writes all bits, the ARM7 only the
    /// low 7 (its GBA-slot timing). The two crt0s write it then read it back.
    pub(crate) exmemcnt: u16,
    /// `POWCNT1` (`0x4000304`, ARM9): LCD / 2D-engine / 3D power and display swap.
    /// Stored and read back (games verify it); the display-swap bit acts once a
    /// second screen exists.
    pub(crate) powcnt1: u16,
    /// Per-core local clocks in master ticks, indexed by [`Core::index`]. They run
    /// ahead of the scheduler's `now` up to the current deadline; the barrier
    /// reconciles them.
    pub(crate) clock: [Timestamp; 2],
    /// Per-core halt state: the ARM7 via `HALTCNT` (`0x4000301`), the ARM9 via the
    /// CP15 wait-for-interrupt. A halted core executes nothing until an enabled
    /// interrupt is pending (`IE & IF`, regardless of `IME`/CPSR.I).
    pub(crate) halted: [bool; 2],
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
            cart: Cart::new(),
            spi: crate::spi::Spi::new(),
            rtc: crate::rtc::Rtc::new(),
            sound: crate::sound::Sound::new(),
            gpu3d: gpu3d::Gpu3d::new(),
            gxfifo_dma_words: 0,
            math: crate::math::Math::new(),
            icache: crate::icache::Cache::instruction(),
            dcache: crate::icache::Cache::data(),
            timing: [CoreTiming::default(), CoreTiming::default()],
            keyinput: 0x03FF,   // all released
            extkeyin: 0x007F,   // X/Y released, pen up, hinge open
            postflg: [0, 0],
            halted: [false, false],
            exmemcnt: 0,
            powcnt1: 0,
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
        // A DMA into the ARM9 3D/geometry register block (GXFIFO + command ports)
        // must reach the GX engine, not the memory map — games stream display lists
        // by DMAing them to the GXFIFO at 0x4000400. The plain memory path drops
        // I/O writes (`map` returns `None`), so route this range explicitly, mirroring
        // the CPU bus's `io_write`. (The GX block needs no scheduler, unlike the rest
        // of I/O, so it is safe to handle here.)
        if core == Core::Arm9 && (addr == 0x0400_0060 || (0x0400_0320..0x0400_06A8).contains(&addr)) {
            self.gpu3d.write_register(addr - 0x0400_0000, value, bytes);
            self.gpu3d.run_pending();
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
            // A cart-mode DMA sources the gamecard data port, which is served (and
            // advanced) by the cartridge controller rather than the memory map.
            let value = if source == crate::cart::DATA_PORT {
                self.cart.read_data()
            } else {
                self.data_read(core, source, bytes)
            };
            self.data_write(core, dest, value, bytes);
            source = source.wrapping_add(source_step);
            dest = dest.wrapping_add(dest_step);
        }
        self.dma[c].channels[channel].complete(source, dest, core == Core::Arm9);
        if ch.irq_on_end() {
            self.interrupts[c].request(crate::dma::irq_source(channel));
        }
    }

    /// Fire the DMA channels waiting on a blank period, in channel order: V-blank DMAs
    /// on both cores (`hblank == false`), or H-blank DMAs on the ARM9 only. Each channel
    /// with the repeat bit set is re-armed by `complete` to fire again next period; the
    /// rest disable themselves after one transfer.
    fn run_blank_dmas(&mut self, hblank: bool) {
        for core in [Core::Arm9, Core::Arm7] {
            let arm9 = core == Core::Arm9;
            if hblank && !arm9 {
                continue; // H-blank DMA is an ARM9-only start mode
            }
            for channel in 0..4 {
                let ch = &self.dma[core.index()].channels[channel];
                let fire = if hblank { ch.is_hblank_dma(arm9) } else { ch.is_vblank_dma(arm9) };
                if fire {
                    self.run_dma_channel(core, channel);
                }
            }
        }
    }

    /// Run a **GXFIFO DMA** (ARM9 start mode 7): read each 32-bit word from the source
    /// and push it straight into the 3D engine's command FIFO (the ordinary DMA path
    /// writes to the memory map, which does not route to the GX registers). Since the
    /// FIFO drains synchronously, the whole batch transfers at once.
    fn run_gxfifo_dma(&mut self, core: Core, channel: usize) {
        let c = core.index();
        let ch = self.dma[c].channels[channel];
        let source_step = ch.source_step();
        let mut source = ch.internal_source();
        for _ in 0..ch.internal_count() {
            let word = self.data_read(core, source, 4);
            self.gpu3d.write_gxfifo(word);
            self.gpu3d.run_pending();
            source = source.wrapping_add(source_step);
        }
        self.gxfifo_dma_words += ch.internal_count() as u64;
        self.dma[c].channels[channel].complete(source, ch.internal_dest(), core == Core::Arm9);
        if ch.irq_on_end() {
            self.interrupts[c].request(crate::dma::irq_source(channel));
        }
    }

    /// A ROMCTRL block start launched a gamecard transfer on `core`: drain it via
    /// any enabled cart-mode DMA channel (games read the cart by DMA), then raise
    /// the transfer-complete IRQ if the block finished and `AUXSPICNT` enables it.
    /// A game using manual (polled) reads finishes the block in [`Self::io_read`]
    /// instead, which raises the IRQ the same way.
    fn start_cart_transfer(&mut self, core: Core) {
        let c = core.index();
        for channel in 0..4 {
            if self.dma[c].channels[channel].is_cart_dma(core == Core::Arm9) {
                self.run_dma_channel(core, channel);
            }
        }
        if self.cart.take_completion() && self.cart.transfer_irq_enabled() {
            self.interrupts[c].request(crate::interrupt::IrqSource::Gamecard);
        }
    }

    /// Route a write to the gamecard registers (`40001A0h`..`40001BFh`). ROMCTRL is
    /// read-modify-written so any access width works; a start bit launches the
    /// transfer. The command buffer takes byte writes; the KEY2 seed ports are
    /// accepted and ignored (the cartridge serves plaintext — see [`crate::cart`]).
    fn write_gamecard(&mut self, core: Core, addr: u32, value: u32, bytes: u32) {
        match addr {
            0x0400_01A0 => self.cart.write_auxspicnt(value as u16),
            0x0400_01A2 => self.cart.write_auxspidata(value as u8), // backup SPI data
            0x0400_01A4..=0x0400_01A7 => {
                // Merge into the stored config, then honour a start bit.
                let shift = (addr - 0x0400_01A4) * 8;
                let width_mask: u64 = (1u64 << (bytes * 8)) - 1;
                let mask = (width_mask << shift) as u32;
                let merged = (self.cart.romctrl_config() & !mask) | ((value << shift) & mask);
                if self.cart.write_romctrl(merged) {
                    self.start_cart_transfer(core);
                }
            }
            0x0400_01A8..=0x0400_01AF => {
                for i in 0..bytes {
                    let idx = (addr + i - 0x0400_01A8) as usize;
                    self.cart.write_command_byte(idx, (value >> (8 * i)) as u8);
                }
            }
            0x0400_01B0 => self.cart.write_seed(0, value),
            0x0400_01B4 => self.cart.write_seed(1, value),
            _ => {} // seed upper halves (B8/BA), AUXSPIDATA: ignored
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
        // DMA fill registers: 0x40000E0..0x40000EF (one word per channel).
        if (0x0400_00E0..0x0400_00F0).contains(&addr) {
            return self.dma[c].read_fill(addr - 0x0400_00E0);
        }
        // Sound registers (ARM7): 16 channels + SOUNDCNT/SOUNDBIAS.
        if core == Core::Arm7 && (0x0400_0400..0x0400_0520).contains(&addr) {
            return self.sound.read(addr - 0x0400_0000, bytes);
        }
        // 3D/geometry registers (ARM9): DISP3DCNT, the clear/fog/toon control block,
        // GXFIFO + command ports, GXSTAT, and matrix/test results. Sound occupies the
        // same 0x4000400 range on the ARM7, so this is ARM9-gated.
        if core == Core::Arm9
            && (addr == 0x0400_0060 || (0x0400_0320..0x0400_06A8).contains(&addr))
        {
            return self.gpu3d.read_register(addr - 0x0400_0000, bytes);
        }
        // 2D engine register blocks (ARM9): Engine A at 0x4000008, Engine B at
        // 0x4001008. `addr & 0xFFF` is the offset within the engine's block.
        if core == Core::Arm9
            && ((0x0400_0008..0x0400_0058).contains(&addr)
                || (0x0400_1008..0x0400_1058).contains(&addr))
        {
            let engine = ((addr >> 12) & 1) as usize;
            let base = addr & 0xFFF;
            let low = self.ppu.read_register(engine, base) as u32;
            return if bytes == 4 {
                low | (self.ppu.read_register(engine, base + 2) as u32) << 16
            } else {
                low
            };
        }
        match addr {
            0x0400_0000 => self.ppu.dispcnt(0),
            0x0400_1000 if core == Core::Arm9 => self.ppu.dispcnt(1),
            0x0400_0004 => self.ppu.read_dispstat(c) as u32,
            0x0400_0006 => self.ppu.vcount() as u32,
            0x0400_0180 => self.ipc.read_sync(core) as u32,
            0x0400_0184 => self.ipc.read_fifocnt(core) as u32,
            0x0400_0130 => self.keyinput as u32, // KEYINPUT (both cores)
            0x0400_0136 if core == Core::Arm7 => self.extkeyin as u32,
            0x0400_0138 if core == Core::Arm7 => self.rtc.read(),
            0x0400_0204 => self.exmemcnt as u32, // EXMEMCNT/EXMEMSTAT (both cores)
            0x0400_0304 if core == Core::Arm9 => self.powcnt1 as u32,
            0x0400_01A0 => self.cart.read_auxspicnt() as u32,
            0x0400_01A2 => self.cart.read_auxspidata() as u32,
            0x0400_01A4 => self.cart.read_romctrl(),
            0x0400_01C0 if core == Core::Arm7 => self.spi.read_cnt() as u32,
            0x0400_01C2 if core == Core::Arm7 => self.spi.read_data() as u32,
            0x0400_0280..=0x0400_02BF if core == Core::Arm9 => {
                self.math.read(addr & 0xFFF, bytes)
            }
            0x0400_0208 => self.interrupts[c].ime() as u32,
            0x0400_0210 => self.interrupts[c].ie(),
            0x0400_0214 => self.interrupts[c].iflags(),
            0x0400_0300 => self.postflg[c] as u32,
            0x0410_0000 => self.ipc.recv(core, &mut self.interrupts),
            0x0410_0010 => {
                // Gamecard data port: stream a word; the last word of a block
                // completes the transfer and raises the IRQ (manual-read path).
                let word = self.cart.read_data();
                if self.cart.take_completion() && self.cart.transfer_irq_enabled() {
                    self.interrupts[c].request(crate::interrupt::IrqSource::Gamecard);
                }
                word
            }
            0x0400_0240 if core == Core::Arm7 => self.vram.vramstat() as u32,
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
            let arm9 = core == Core::Arm9;
            let armed = if bytes == 4 {
                self.dma[c].write_register(base, value as u16, arm9);
                self.dma[c].write_register(base + 2, (value >> 16) as u16, arm9)
            } else {
                self.dma[c].write_register(base, value as u16, arm9)
            };
            if let Some(channel) = armed {
                if self.dma[c].channels[channel].is_gxfifo(arm9) {
                    self.run_gxfifo_dma(core, channel);
                } else {
                    self.run_dma_channel(core, channel);
                }
            }
            return;
        }
        // DMA fill registers: 0x40000E0..0x40000EF (one word per channel).
        if (0x0400_00E0..0x0400_00F0).contains(&addr) {
            self.dma[c].write_fill(addr - 0x0400_00E0, value);
            return;
        }
        // Sound registers (ARM7): 16 channels + SOUNDCNT/SOUNDBIAS.
        if core == Core::Arm7 && (0x0400_0400..0x0400_0520).contains(&addr) {
            self.sound.write(addr - 0x0400_0000, value, bytes);
            return;
        }
        // 3D/geometry registers (ARM9): DISP3DCNT, clear/fog/toon control, GXFIFO +
        // command ports, GXSTAT. ARM9-gated (sound shares 0x4000400 on the ARM7).
        if core == Core::Arm9
            && (addr == 0x0400_0060 || (0x0400_0320..0x0400_06A8).contains(&addr))
        {
            self.gpu3d.write_register(addr - 0x0400_0000, value, bytes);
            // Execute any now-complete commands (currently the matrix engine; vertex
            // and later commands are consumed but not yet acted on). Runs
            // synchronously until command timing lands, so the FIFO never stalls the
            // CPU.
            self.gpu3d.run_pending();
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
        // Gamecard slot: AUXSPICNT/ROMCTRL/command/seed ports.
        if (0x0400_01A0..0x0400_01C0).contains(&addr) {
            self.write_gamecard(core, addr, value, bytes);
            return;
        }
        // ARM7 SPI bus: SPICNT (0x40001C0) / SPIDATA (0x40001C2), the firmware flash.
        if core == Core::Arm7 && (0x0400_01C0..0x0400_01C4).contains(&addr) {
            if addr < 0x0400_01C2 {
                self.spi.write_cnt(value as u16);
            } else {
                self.spi.write_data(value as u16);
            }
            return;
        }
        // ARM9 hardware division / square-root units.
        if core == Core::Arm9 && (0x0400_0280..0x0400_02C0).contains(&addr) {
            self.math.write(addr & 0xFFF, value, bytes);
            return;
        }
        // 2D engine register blocks (BGxCNT..BLDY), ARM9 only: Engine A at 0x4000008,
        // Engine B at 0x4001008.
        if core == Core::Arm9
            && ((0x0400_0008..0x0400_0058).contains(&addr)
                || (0x0400_1008..0x0400_1058).contains(&addr))
        {
            let engine = ((addr >> 12) & 1) as usize;
            let base = addr & 0xFFF;
            if bytes == 4 {
                self.ppu.write_register(engine, base, value as u16, 0xFFFF);
                self.ppu
                    .write_register(engine, base + 2, (value >> 16) as u16, 0xFFFF);
            } else {
                self.ppu.write_register(engine, base, value as u16, 0xFFFF);
            }
            return;
        }
        match addr {
            0x0400_0000 if core == Core::Arm9 => self.ppu.write_dispcnt(0, value, bytes),
            0x0400_1000 if core == Core::Arm9 => self.ppu.write_dispcnt(1, value, bytes),
            0x0400_0004 => self.ppu.write_dispstat(c, value as u16),
            0x0400_0180 => self.ipc.write_sync(core, value as u16, &mut self.interrupts),
            0x0400_0184 => self
                .ipc
                .write_fifocnt(core, value as u16, &mut self.interrupts),
            0x0400_0188 => self.ipc.send(core, value, &mut self.interrupts),
            0x0400_0138 if core == Core::Arm7 => self.rtc.write(value),
            0x0400_0204 => {
                // EXMEMCNT: the ARM9 owns every bit; the ARM7 may only change the low
                // 7 (its own GBA-slot timing). Both cores read the same value back.
                self.exmemcnt = if core == Core::Arm9 {
                    value as u16
                } else {
                    (self.exmemcnt & !0x7F) | (value as u16 & 0x7F)
                };
            }
            0x0400_0304 if core == Core::Arm9 => self.powcnt1 = value as u16,
            0x0400_0208 => self.interrupts[c].set_ime(value & 1 != 0),
            0x0400_0210 => self.interrupts[c].set_ie(value),
            0x0400_0214 => self.interrupts[c].acknowledge(value),
            0x0400_0300 => {
                // POSTFLG: bit 0 latches set (cannot be cleared); NDS9 bit 1 is R/W.
                let mask = if core == Core::Arm9 { 0b11 } else { 0b01 };
                self.postflg[c] |= value as u8 & mask;
            }
            // HALTCNT: bits 6-7 select the power-down mode (2 = Halt, 3 = Sleep); both
            // stop the ARM7 until an enabled interrupt is pending. Bit 7 marks either,
            // which is all we model.
            0x0400_0301 if core == Core::Arm7 && value & 0x80 != 0 => {
                self.halted[c] = true;
            }
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
            NdsEvent::TimerOverflow {
                core,
                timer,
                generation,
            } => {
                let c = core.index();
                // `timers[c]` and `interrupts[c]` are disjoint fields.
                self.timers[c].handle_overflow(timer, generation, &mut self.interrupts[c], ctx);
            }
            NdsEvent::Ppu(event) => {
                // Refresh the 3D engine's texture VRAM from the banked VRAM, then
                // rasterize the sealed 3D frame (cached, so at most once per swap)
                // before the PPU composites it as Engine A's BG0.
                self.vram.assemble_texture_image(self.gpu3d.texture_image_mut());
                self.vram.assemble_texture_palette(self.gpu3d.texture_palette_mut());
                self.gpu3d.render_frame();
                // `ppu`, `interrupts`, `vram`, `memory`, and `gpu3d` are disjoint fields.
                self.ppu.handle_event(
                    event,
                    &mut self.interrupts,
                    &self.vram,
                    &self.memory.palette,
                    &self.memory.oam,
                    Some(self.gpu3d.framebuffer_3d()),
                    ctx,
                );
                // Blank-timed DMAs fire off the PPU's new position: V-blank DMAs when the
                // frame just entered V-blank, H-blank DMAs (ARM9 only) during each visible
                // line's H-blank. (Immediate/GXFIFO DMAs run when armed; cart on block start.)
                match event {
                    PpuEvent::LineStart if self.ppu.vcount() == crate::ppu::HEIGHT as u16 => {
                        self.run_blank_dmas(false);
                    }
                    PpuEvent::HBlank if (self.ppu.vcount() as usize) < crate::ppu::HEIGHT => {
                        self.run_blank_dmas(true);
                    }
                    _ => {}
                }
            }
            NdsEvent::SoundSample => {
                // `sound`, `memory`, and `cp15` are disjoint fields; the channels
                // stream their source samples through the ARM7 memory map.
                let (sound, memory, cp15) = (&mut self.sound, &self.memory, &self.cp15);
                sound.generate_sample(|addr| memory.read8(Core::Arm7, addr, false, cp15));
                ctx.scheduler
                    .schedule_after(crate::sound::CYCLES_PER_SAMPLE, NdsEvent::SoundSample);
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
    /// Whether the recurring sound-sample event has been scheduled.
    audio_started: bool,
    /// Debug: snapshot of `(total_commands, submitted, emitted, cmd_hist)` at the
    /// previous `debug_report` call, so each report can show the delta since then.
    dbg_prev: Option<(u64, u64, u64, Box<[u64; 128]>)>,
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
            audio_started: false,
            dbg_prev: None,
        }
    }

    /// The memory image, for loading code/data and inspection.
    pub fn memory(&mut self) -> &mut Memory {
        &mut self.machine.memory
    }

    /// Load the ARM9 and ARM7 BIOS images. Optional for direct boot (BIOS-free
    /// homebrew runs without them); required for ROMs that call BIOS SWIs.
    pub fn load_bios(&mut self, bios9: &[u8], bios7: &[u8]) {
        self.machine.memory.load_bios9(bios9);
        self.machine.memory.load_bios7(bios7);
    }

    /// A VRAM bank's `VRAMCNT` byte (bank 0=A … 8=I), for debug tooling.
    pub fn vram_control(&self, bank: usize) -> u8 {
        self.machine.vram.control(bank)
    }

    /// The 3D engine's `(polygons, vertices)` per-frame high-water mark, for debug
    /// tooling (confirms a game drives geometry through the pipeline).
    pub fn gpu3d_peak_geometry(&self) -> (usize, usize) {
        self.machine.gpu3d.peak_geometry()
    }

    /// The `(polygons, vertices)` in the 3D engine's sealed render list (the geometry
    /// the rasterizer would draw this frame).
    pub fn gpu3d_render_list(&self) -> (usize, usize) {
        let rl = self.machine.gpu3d.render_list();
        (rl.polygons().len(), rl.vertices().len())
    }

    /// The sealed render list's `(viewport, per-vertex clip coords)` — for debugging
    /// projection/viewport conventions.
    pub fn gpu3d_render_geometry(&self) -> (u32, Vec<[i32; 4]>) {
        let rl = self.machine.gpu3d.render_list();
        (rl.viewport, rl.vertices().iter().map(|v| v.clip).collect())
    }

    /// `DISP3DCNT` plus each sealed polygon's `(texture format, polygon alpha, blend
    /// mode, texcoord-transform mode)` — for debugging the translucency/texture path.
    pub fn gpu3d_poly_summary(&self) -> (u16, Vec<(u8, u8, u8, u8)>) {
        let rl = self.machine.gpu3d.render_list();
        let polys = rl
            .polygons()
            .iter()
            .map(|p| {
                let fmt = ((p.tex_param >> 26) & 7) as u8;
                let alpha = ((p.attr >> 16) & 0x1F) as u8;
                let mode = ((p.attr >> 4) & 3) as u8;
                let tex_mode = ((p.tex_param >> 30) & 3) as u8;
                (fmt, alpha, mode, tex_mode)
            })
            .collect();
        (self.machine.gpu3d.disp3dcnt(), polys)
    }

    /// `CLEAR_COLOR` (`0x4000350`) — the rear-plane color/alpha register, for debug.
    pub fn gpu3d_clear_color(&self) -> u32 {
        self.machine.gpu3d.clear_color()
    }

    /// Recent per-swap 3D polygon counts (oldest first), for debug.
    pub fn gpu3d_poly_log(&self) -> Vec<u16> {
        self.machine.gpu3d.poly_log()
    }

    /// Per-polygon texcoord-transform mode (`TEXIMAGE_PARAM` bits 30-31), for debug.
    pub fn gpu3d_poly_texcoord_modes(&self) -> Vec<u8> {
        self.machine
            .gpu3d
            .render_list()
            .polygons()
            .iter()
            .map(|p| ((p.tex_param >> 30) & 3) as u8)
            .collect()
    }

    /// Per-polygon texture debug: `(format, image_offset, pltt_base, centre-texel color,
    /// centre-texel alpha)` sampled from the assembled texture VRAM.
    pub fn gpu3d_poly_texture_debug(&self) -> Vec<(u8, u32, u32, [u8; 3], u8)> {
        let mut image = vec![0u8; 0x8_0000];
        let mut palette = vec![0u8; 0x1_8000];
        self.machine.vram.assemble_texture_image(&mut image);
        self.machine.vram.assemble_texture_palette(&mut palette);
        let tex = gpu3d::texture::TextureSet { image: &image, palette: &palette };
        self.machine
            .gpu3d
            .render_list()
            .polygons()
            .iter()
            .map(|p| {
                let tp = gpu3d::texture::TexParams::decode(p.tex_param, p.pltt_base);
                let t = tp.sample(&tex, tp.size_s / 2, tp.size_t / 2);
                (tp.format, tp.offset, p.pltt_base & 0x1FFF, t.color, t.alpha)
            })
            .collect()
    }

    /// Debug: the 3D render list rasterized with the depth test disabled (256×192 BGR555),
    /// to tell depth-rejected black from genuinely-uncovered black.
    pub fn gpu3d_debug_no_depth(&self) -> Vec<u16> {
        self.machine.gpu3d.debug_render_no_depth()
    }

    /// Debug: toggle 3D winding culling off/on.
    pub fn gpu3d_set_disable_cull(&mut self, on: bool) {
        self.machine.gpu3d.set_disable_cull(on);
    }

    /// Decode a render-list polygon's full texture to `(width, height, BGR555 pixels)`,
    /// for debug visualization — reveals whether the texel sampler (e.g. the 4×4-
    /// compressed decode) is producing the right image or a glitch pattern.
    pub fn gpu3d_dump_poly_texture(&self, poly: usize) -> Option<(u32, u32, Vec<u16>)> {
        let mut image = vec![0u8; 0x8_0000];
        let mut palette = vec![0u8; 0x1_8000];
        self.machine.vram.assemble_texture_image(&mut image);
        self.machine.vram.assemble_texture_palette(&mut palette);
        let tex = gpu3d::texture::TextureSet { image: &image, palette: &palette };
        let p = self.machine.gpu3d.render_list().polygons().get(poly)?;
        let tp = gpu3d::texture::TexParams::decode(p.tex_param, p.pltt_base);
        let (w, h) = (tp.size_s.max(1) as u32, tp.size_t.max(1) as u32);
        let mut out = Vec::with_capacity((w * h) as usize);
        for t in 0..h as i32 {
            for s in 0..w as i32 {
                let c = tp.sample(&tex, s, t).color; // 6-bit channels
                out.push((c[0] as u16 >> 1) | ((c[1] as u16 >> 1) << 5) | ((c[2] as u16 >> 1) << 10));
            }
        }
        Some((w, h, out))
    }

    /// Rasterize the 3D engine's sealed render list to a 256×192 RGB8 buffer (covered
    /// pixels as their color, uncovered as black), for debug visualization.
    /// Debug: render an engine's full composite WITH the current 3D framebuffer.
    pub fn debug_engine_composite_3d(&mut self, engine: usize) -> Vec<u16> {
        let m = &mut self.machine;
        let three_d = m.gpu3d.framebuffer_3d();
        m.ppu.debug_render_engine(engine, &m.vram, &m.memory.palette, &m.memory.oam, Some(three_d))
    }

    pub fn gpu3d_rasterize_rgb(&self) -> Vec<u8> {
        let mut fb = gpu3d::raster::Framebuffer3d::new();
        let mut image = vec![0u8; 0x8_0000];
        let mut palette = vec![0u8; 0x1_8000];
        self.machine.vram.assemble_texture_image(&mut image);
        self.machine.vram.assemble_texture_palette(&mut palette);
        let tex = gpu3d::texture::TextureSet { image: &image, palette: &palette };
        let cfg = self.machine.gpu3d.render_config();
        gpu3d::raster::render(self.machine.gpu3d.render_list(), &tex, &cfg, &mut fb);
        let mut rgb = Vec::with_capacity(gpu3d::raster::WIDTH * gpu3d::raster::HEIGHT * 3);
        for p in &fb.pixels {
            if p.covered {
                for &c in &p.color {
                    rgb.push((c << 2) | (c >> 4)); // 6-bit → 8-bit
                }
            } else {
                rgb.extend_from_slice(&[0, 0, 0]);
            }
        }
        rgb
    }

    /// The cartridge backup (save) bytes, for the host to persist.
    pub fn cart_backup(&self) -> &[u8] {
        self.machine.cart.backup_bytes()
    }

    /// Restore previously saved cartridge backup contents.
    pub fn load_cart_backup(&mut self, data: &[u8]) {
        self.machine.cart.load_backup(data);
    }

    /// Whether the backup changed since the last [`Self::clear_cart_backup_dirty`].
    pub fn cart_backup_dirty(&self) -> bool {
        self.machine.cart.backup_dirty()
    }
    pub fn clear_cart_backup_dirty(&mut self) {
        self.machine.cart.clear_backup_dirty();
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

            // Step the more-behind *runnable* core (ties to the ARM9) until both reach
            // the deadline. A halted core runs nothing: it wakes once an enabled
            // interrupt is pending (Halt terminates on `IE & IF` regardless of `IME`/
            // CPSR.I — the IRQ itself is taken only if allowed), otherwise it idles to
            // the deadline so the barrier can advance and events there may wake it.
            loop {
                for c in 0..2 {
                    if self.machine.halted[c] && self.machine.interrupts[c].pending() {
                        self.machine.halted[c] = false;
                    }
                    if self.machine.halted[c] && self.machine.clock[c] < deadline {
                        self.machine.clock[c] = deadline;
                    }
                }
                let (c0, c1) = (self.machine.clock[0], self.machine.clock[1]);
                let run0 = !self.machine.halted[0] && c0 < deadline;
                let run1 = !self.machine.halted[1] && c1 < deadline;
                let core = if run0 && (!run1 || c0 <= c1) {
                    Core::Arm9
                } else if run1 {
                    Core::Arm7
                } else {
                    break;
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
        #[cfg(feature = "cyctrace")]
        if core == Core::Arm9 && cyctrace::is_trigger(self.arm9.register(15)) {
            cyctrace::snapshot(std::array::from_fn(|i| self.arm9.register(i)));
        }
        // The ARM9 selects high exception vectors (0xFFFF0000) through CP15; keep
        // the core's base in sync so SWIs/IRQs reach the BIOS handlers.
        if core == Core::Arm9 {
            let base = if self.machine.cp15.high_exception_vectors() {
                0xFFFF_0000
            } else {
                0
            };
            self.arm9.set_exception_base(base);
        }
        let asserted = self.machine.interrupts[core.index()].line_asserted();
        let cpu = match core {
            Core::Arm9 => &mut self.arm9,
            Core::Arm7 => &mut self.arm7,
        };
        if asserted && cpu.irq_enabled() {
            cpu.take_irq();
        }
        // Each core charges its clock at the instruction boundary via the pipeline
        // model: the bus accumulates fetch/data/internal costs during the step, and
        // we combine them into one cost here.
        let c = core.index();
        self.machine.timing[c].begin_step();
        let mut bus = NdsCpuBus {
            machine: &mut self.machine,
            scheduler: &mut self.scheduler,
            core,
        };
        cpu.step(&mut bus);
        // The cost is in the core's own cycles; the ARM9 runs at the master rate, the
        // ARM7 at half (one ARM7 cycle = two master ticks).
        let arm9 = core == Core::Arm9;
        let (branched, block) = if arm9 {
            (self.arm9.branched(), self.arm9.block_transfer())
        } else {
            (self.arm7.branched(), self.arm7.block_transfer())
        };
        // Prefetch model: charge `max(cExecute(prev), cFetch(this))` — this opcode was
        // fetched while the previous instruction executed — then carry this
        // instruction's execute forward to pair with the next fetch.
        let execute = self.machine.timing[c].execute_cost(branched, block, arm9);
        let t = &mut self.machine.timing[c];
        let cost = t.pending_execute.max(t.fetch) as Timestamp;
        t.pending_execute = execute;
        self.machine.clock[c] += if arm9 { cost } else { cost * 2 };
    }

    /// Direct-boot a `.nds` image: copy the ARM9/ARM7 binaries to their RAM
    /// addresses, seed the entry points and stacks, and leave the cores ready to
    /// run. Bypasses the firmware/BIOS handshake (deferred). The stacks/`WRAMCNT`
    /// use the conventional direct-boot values; the game's startup code sets up
    /// the rest (CP15/TCM, banked stacks, I/O).
    pub fn direct_boot(&mut self, rom: &[u8]) -> Result<(), crate::boot::BootError> {
        let header = crate::boot::Header::parse(rom)?;

        // Insert the cartridge so the game can stream the rest of its ROM at
        // runtime (the main data area is identical before/after secure-area decrypt).
        self.machine.cart.insert(rom);

        // A commercial ROM keeps its ARM9 boot code in the secure area, whose first
        // 2 KB may be KEY1-encrypted. Decrypt into an owned copy only when the ID
        // shows work is pending (an already-boot-ready dump skips the 128 MB copy).
        let mut decrypted: Option<Vec<u8>> = None;
        if crate::key1::secure_area_present(header.arm9_rom_offset)
            && rom.len() >= crate::key1::SECURE_AREA_START + crate::key1::SECURE_AREA_ENC_LEN
            && crate::key1::secure_area_needs_work(
                &rom[crate::key1::SECURE_AREA_START..crate::key1::SECURE_AREA_START + 8],
            )
        {
            let keytable = self.machine.memory.key1_keytable().to_vec();
            let mut buf = rom.to_vec();
            let _state = crate::key1::process_secure_area(&mut buf, header.gamecode, &keytable);
            decrypted = Some(buf);
        }
        let rom: &[u8] = decrypted.as_deref().unwrap_or(rom);

        self.load_binary(
            Core::Arm9,
            rom,
            header.arm9_rom_offset,
            header.arm9_ram_address,
            header.arm9_size,
        );
        self.load_binary(
            Core::Arm7,
            rom,
            header.arm7_rom_offset,
            header.arm7_ram_address,
            header.arm7_size,
        );
        // The firmware copies the header to Main RAM at 0x027F_FE00.
        for (i, &byte) in rom.iter().take(0x170).enumerate() {
            self.machine
                .data_write(Core::Arm9, 0x027F_FE00 + i as u32, byte as u32, 1);
        }
        // Rebuild the firmware boot-info footer + user settings + POSTFLG that
        // retail games expect the firmware to have established.
        self.seed_firmware_state(rom, &header);
        // Seed CP15 to the TCM state the ARM9 BIOS establishes, which BIOS-less
        // homebrew relies on (armwrestler's stack lives in DTCM and its startup
        // never configures CP15): DTCM 16 KB at 0x0080_0000 and ITCM 32 KB at 0,
        // both enabled. Games that manage CP15 themselves overwrite this.
        self.machine.cp15.write(0, 9, 1, 0, 0x0080_000A); // DTCM base 0x0080_0000, 16 KB
        self.machine.cp15.write(0, 9, 1, 1, 0x0000_000C); // ITCM 32 KB (base fixed at 0)
        self.machine.cp15.write(0, 1, 0, 0, (1 << 16) | (1 << 18)); // enable DTCM + ITCM
                                                                    // Give the ARM7 the Shared WRAM (WRAMCNT=3): its crt0 relocates code into
                                                                    // the 32K Shared WRAM mirrored at 0x037F8000, using Shared + ARM7-WRAM as
                                                                    // one continuous 96K block (GBATEK "Shared-RAM").
        self.machine.memory.wramcnt = 3;
        // Entry points and conventional system-mode stacks (the cores boot in
        // System mode; each game's startup replaces these with its own). The ARM7
        // stack sits in ARM7-WRAM; the ARM9's is a fallback it reconfigures early.
        self.arm9.set_pc(header.arm9_entry);
        self.arm9.set_register(13, 0x0300_2F7C);
        self.arm7.set_pc(header.arm7_entry);
        self.arm7.set_register(13, 0x0380_FD80);
        Ok(())
    }

    /// Rebuild the firmware-established boot state a retail game expects (GBATEK
    /// "DS Firmware User Settings" and the `27FFxxx` footer): the gamecard chip ID
    /// and header CRCs mirrored into the footer at `0x27FF800`/`0x27FFC00`, the
    /// inter-core boot handshake words, the boot indicator, and the user's touch/
    /// language/clock settings at `0x27FFC80`. Also sets `POSTFLG` on both cores.
    fn seed_firmware_state(&mut self, rom: &[u8], header: &crate::boot::Header) {
        let chip_id = self.machine.cart.chip_id();
        let u16_at = |off: usize| u16::from_le_bytes([rom[off], rom[off + 1]]) as u32;
        let header_crc = u16_at(0x15E); // hdr[15Eh]: cart header CRC
        let secure_crc = u16_at(0x06C); // hdr[06Ch]: secure-area CRC
        const NDS7_BIOS_CRC: u32 = 0x5835; // constant (GBATEK)

        let mut w = |addr: u32, value: u32, bytes: u32| {
            self.machine.data_write(Core::Arm9, addr, value, bytes);
        };

        // Footer at 0x27FF800 ("from BIOS boot code").
        w(0x027F_F800, chip_id, 4); // Chip ID 1
        w(0x027F_F804, chip_id, 4); // Chip ID 2
        w(0x027F_F808, header_crc, 2); // Header CRC (verified)
        w(0x027F_F80A, secure_crc, 2); // Secure-area CRC
        w(0x027F_F80C, 0, 2); // Missing/bad CRC: okay
        w(0x027F_F80E, 0, 2); // Secure area bad: okay
        w(0x027F_F810, 0xFFFF, 2); // Boot handler task number (idle at cart boot)
        w(0x027F_F816, 0, 2); // RTC status: okay

        // Footer at 0x27FF850/860 ("from firmware boot code"), incl. the inter-core
        // boot handshake the two crt0s check.
        w(0x027F_F850, NDS7_BIOS_CRC, 2);
        w(0x027F_F860, header.arm7_ram_address, 4); // copy of cart[38h]
        w(0x027F_F864, 0, 4); // Wifi user settings: okay
        w(0x027F_F874, 0x359A, 2); // firmware part5 crc16 (constant)
        w(0x027F_F880, 7, 4); // Message from NDS9 to NDS7 (cart-boot value)
        w(0x027F_F884, 6, 4); // NDS7 boot task, also checked by NDS9
        w(0x027F_F890, 0xB000_2A22, 4); // boot flags (cart-boot value)

        // Footer at 0x27FFC00 (mirror of 0x27FF800 + firmware values).
        w(0x027F_FC00, chip_id, 4);
        w(0x027F_FC04, chip_id, 4);
        w(0x027F_FC08, header_crc, 2);
        w(0x027F_FC0A, secure_crc, 2);
        w(0x027F_FC0C, 0, 2);
        w(0x027F_FC0E, 0, 2);
        w(0x027F_FC10, NDS7_BIOS_CRC, 2);
        w(0x027F_FC40, 1, 2); // Boot Indicator = normal (required by some games)

        // User settings copy at 0x27FFC80 (the 0x70 data bytes).
        for (i, &byte) in crate::firmware::user_settings().iter().enumerate() {
            w(0x027F_FC80 + i as u32, byte as u32, 1);
        }

        // Boot completed: retail games refuse to run while POSTFLG reads 0.
        self.machine.postflg = [1, 1];
        // Firmware leaves `POWCNT1` with the LCDs and both 2D engines powered and the
        // display swap set so Engine A drives the top screen (bit 15). Games override
        // it, but this is the sensible post-firmware default a direct boot inherits.
        self.machine.powcnt1 = 0x820F;
    }

    /// Copy a cartridge binary into a core's RAM, byte by byte through its map.
    fn load_binary(&mut self, core: Core, rom: &[u8], rom_offset: u32, ram: u32, size: u32) {
        for i in 0..size {
            let byte = rom[(rom_offset + i) as usize];
            self.machine.data_write(core, ram + i, byte as u32, 1);
        }
    }

    /// Set the keypad state from a pressed-button bitmask whose low ten bits use
    /// the `KEYINPUT` bit order (A, B, Select, Start, Right, Left, Up, Down, R, L).
    pub fn set_keypad(&mut self, pressed: u32) {
        self.machine.keyinput = 0x03FF & !(pressed as u16);
        // EXTKEYIN carries the X (bit 10) and Y (bit 11) buttons, active-low; the
        // hinge stays "open", and the pen-down bit (6) is owned by `set_touch`, so
        // preserve it rather than clobbering an in-progress touch.
        let mut ext = 0x007F;
        if pressed & (1 << 10) != 0 {
            ext &= !0x01; // X pressed
        }
        if pressed & (1 << 11) != 0 {
            ext &= !0x02; // Y pressed
        }
        ext = (ext & !(1 << 6)) | (self.machine.extkeyin & (1 << 6));
        self.machine.extkeyin = ext;
    }

    /// Set (or lift) the touchscreen pen. `Some((x, y))` presses at screen pixel
    /// `(x, y)` on the lower screen — routed to the touchscreen ADC (converted from
    /// pixels via the firmware calibration) and reflected in `EXTKEYIN`'s pen-down
    /// bit (active-low); `None` lifts the pen.
    pub fn set_touch(&mut self, pos: Option<(i32, i32)>) {
        match pos {
            Some((x, y)) => {
                self.machine.spi.set_touch(Some(crate::firmware::touch_adc(x, y)));
                self.machine.extkeyin &= !(1 << 6); // pen down
            }
            None => {
                self.machine.spi.set_touch(None);
                self.machine.extkeyin |= 1 << 6; // pen up
            }
        }
    }

    /// Begin the PPU's continuous scanline schedule (idempotent).
    pub fn start_video(&mut self) {
        self.machine.ppu.start(&mut self.scheduler);
    }

    /// Begin the sound mixer's recurring sample event (idempotent).
    pub fn start_audio(&mut self) {
        if self.audio_started {
            return;
        }
        self.audio_started = true;
        let now = self.scheduler.now();
        self.scheduler
            .schedule_at(now + crate::sound::CYCLES_PER_SAMPLE, NdsEvent::SoundSample);
    }

    /// Drain the mixer's interleaved stereo `i16` samples produced since the last call.
    pub fn take_audio(&mut self) -> Vec<i16> {
        self.machine.sound.take_samples()
    }

    /// Debug: (sound master enabled, active-channel count, active-channel bitmask).
    pub fn sound_status(&self) -> (bool, usize, u16) {
        self.machine.sound.status()
    }

    /// Debug: a sound channel's `(sad, tmr, len, pos)`.
    pub fn sound_channel(&self, ch: usize) -> (u32, u16, u32, u32) {
        self.machine.sound.channel_debug(ch)
    }

    /// Run until the 2D engine completes one frame (into the next V-blank).
    pub fn run_frame(&mut self) {
        self.start_video();
        self.start_audio();
        let start = self.machine.ppu.frame();
        for _ in 0..(crate::ppu::HEIGHT as u64 + 100) {
            if self.machine.ppu.frame() != start {
                break;
            }
            let target = self.scheduler.now() + crate::ppu::CYCLES_PER_LINE;
            self.run_until(target);
        }
    }

    /// Engine A's current output image, in BGR555. See [`Self::screen`] for the
    /// physical-screen view (which honors the `POWCNT1` display swap).
    pub fn framebuffer(&self) -> &[u16] {
        self.machine.ppu.framebuffer(0)
    }

    /// The BGR555 image shown on physical screen `index` (0 = top, 1 = bottom),
    /// honoring `POWCNT1` bit 15 (1 = Engine A drives the top screen).
    pub fn screen(&self, index: usize) -> &[u16] {
        let a_on_top = self.machine.powcnt1 & (1 << 15) != 0;
        let engine = if a_on_top { index } else { 1 - index };
        self.machine.ppu.framebuffer(engine)
    }

    /// The completed-frame counter.
    pub fn frame(&self) -> u64 {
        self.machine.ppu.frame()
    }

    /// A snapshot of a 2D engine's register file, for debugging (0 = A, 1 = B).
    pub fn engine_registers(&self, engine: usize) -> video2d::Registers {
        self.machine.ppu.engine_registers(engine)
    }

    /// Debug: per-layer pixel coverage `[bg0, bg1, bg2, bg3, obj]` for a 2D engine —
    /// how many pixels each layer contributes in isolation.
    pub fn debug_layer_coverage(&mut self, engine: usize) -> [usize; 5] {
        let m = &mut self.machine;
        m.ppu.debug_layer_coverage(engine, &m.vram, &m.memory.palette, &m.memory.oam)
    }

    /// Debug: the isolated BGR555 framebuffer for one layer (BG 0-3, OBJ = 4).
    pub fn debug_render_layer(&mut self, engine: usize, layer: usize) -> Vec<u16> {
        let m = &mut self.machine;
        m.ppu.debug_render_layer(engine, layer, &m.vram, &m.memory.palette, &m.memory.oam)
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
    /// A one-shot human-readable diagnostic of the current 2D/3D graphics state — the
    /// screen assignment, both engines' config, and the sealed 3D render list — for a
    /// host "dump state" key while a game sits on a screen of interest.
    pub fn debug_report(&mut self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let rd = |sys: &mut Self, a: u32| sys.read(Core::Arm9, a, 4);
        let powcnt1 = self.read(Core::Arm9, 0x0400_0304, 2);
        let disp3d = self.read(Core::Arm9, 0x0400_0060, 2);
        let clear = self.gpu3d_clear_color();
        let _ = writeln!(s, "POWCNT1={powcnt1:#06x} (EngineA_on_top={})", (powcnt1 >> 15) & 1);
        let _ = writeln!(s, "DISP3DCNT={disp3d:#06x} (tex_en={} alpha_test={} alpha_blend={} rear_bitmap={})",
            disp3d & 1, (disp3d >> 2) & 1, (disp3d >> 3) & 1, (disp3d >> 14) & 1);
        let _ = writeln!(s, "CLEAR_COLOR={clear:#010x} (rgb5=[{},{},{}] alpha={})",
            clear & 0x1F, (clear >> 5) & 0x1F, (clear >> 10) & 0x1F, (clear >> 16) & 0x1F);
        for (eng, base) in [(0usize, 0x0400_0000u32), (1, 0x0400_1000)] {
            let dc = rd(self, base);
            let r = self.engine_registers(eng);
            let _ = writeln!(s, "Engine{} DISPCNT={dc:#010x} mode={} dispmode={} 3d_bg0={} ext_pal={} cov={:?}",
                if eng == 0 { "A" } else { "B" }, dc & 7, (dc >> 16) & 3, (dc >> 3) & 1, (dc >> 30) & 3,
                self.debug_layer_coverage(eng));
            for bg in 0..4 {
                let _ = writeln!(s, "  BG{bg} en={} BGCNT={:#06x} prio={} bit7={} bit2={} size={} hofs={} vofs={} affine(PA={} PD={} X={} Y={})",
                    r.dispcnt & (1 << (8 + bg)) != 0, r.bgcnt[bg], r.bgcnt[bg] & 3,
                    (r.bgcnt[bg] >> 7) & 1, (r.bgcnt[bg] >> 2) & 1, (r.bgcnt[bg] >> 14) & 3,
                    r.bg_hofs[bg], r.bg_vofs[bg],
                    if bg >= 2 { r.bg_pa[bg - 2] } else { 0 }, if bg >= 2 { r.bg_pd[bg - 2] } else { 0 },
                    if bg >= 2 { r.bg_ref_x[bg - 2] } else { 0 }, if bg >= 2 { r.bg_ref_y[bg - 2] } else { 0 });
            }
        }
        let (polys, verts) = self.gpu3d_render_list();
        let (peak_p, peak_v) = self.gpu3d_peak_geometry();
        let ram_count = self.read(Core::Arm9, 0x0400_0604, 4);
        let _ = writeln!(s, "3D render list: {polys} polys, {verts} verts | peak {peak_p}/{peak_v} | RAM_COUNT={ram_count:#010x} (build polys={} verts={}) | GXFIFO-DMA words={}",
            ram_count & 0xFFF, (ram_count >> 16) & 0x1FFF, self.machine.gxfifo_dma_words);
        // Breakdown of the sealed render list by shading path — reveals whether polys
        // are textured, and which blend mode (0=modulate 1=decal 2=toon/highlight
        // 3=shadow) and opacity they use, to diagnose miscoloured/washed-out 3D.
        let (_, summary) = self.gpu3d_poly_summary();
        let (mut modes, mut textured, mut opaque, mut translucent, mut wire) = ([0u32; 4], 0u32, 0u32, 0u32, 0u32);
        let (mut fmts, mut texmodes) = ([0u32; 8], [0u32; 4]);
        for &(fmt, alpha, mode, tex_mode) in &summary {
            modes[mode as usize & 3] += 1;
            fmts[fmt as usize & 7] += 1;
            texmodes[tex_mode as usize & 3] += 1;
            if fmt != 0 { textured += 1; }
            match alpha { 0 => wire += 1, 31 => opaque += 1, _ => translucent += 1 }
        }
        let _ = writeln!(s, "render-list shading: textured={textured}/{} untextured={} | blend[modulate={} decal={} toon={} shadow={}] | opaque={opaque} translucent={translucent} wire/opaque0={wire}",
            summary.len(), summary.len() as u32 - textured, modes[0], modes[1], modes[2], modes[3]);
        // Texture format (0=none 1=A3I5 2=pal4 3=pal16 4=pal256 5=4x4comp 6=A5I3 7=direct)
        // and texcoord-transform mode (0=none 1=texcoord/matrix 2=normal/envmap 3=vertex).
        let _ = writeln!(s, "  tex formats: none={} A3I5={} pal4={} pal16={} pal256={} 4x4={} A5I3={} direct={} | texcoord modes: none={} matrix={} normal/env={} vertex={}",
            fmts[0], fmts[1], fmts[2], fmts[3], fmts[4], fmts[5], fmts[6], fmts[7],
            texmodes[0], texmodes[1], texmodes[2], texmodes[3]);
        // Clip health: after clipping every surviving vertex must satisfy |x|,|y|,|z| <= w
        // and w > 0. Any that don't are a clipping/overflow failure — they project to
        // huge off-screen coordinates (the black wedges over the sky).
        let (_, clips) = self.gpu3d_render_geometry();
        let (mut min_w, mut max_w, mut wbad, mut outside, mut maxabs) = (i64::MAX, i64::MIN, 0u32, 0u32, 0i64);
        for c in &clips {
            let (x, y, z, w) = (c[0] as i64, c[1] as i64, c[2] as i64, c[3] as i64);
            min_w = min_w.min(w);
            max_w = max_w.max(w);
            if w <= 0 { wbad += 1; }
            if x.abs() > w || y.abs() > w || z.abs() > w { outside += 1; }
            maxabs = maxabs.max(x.abs()).max(y.abs()).max(z.abs()).max(w.abs());
        }
        let _ = writeln!(s, "  clip health: verts={} w=[{min_w}..{max_w}] w<=0={wbad} outside_frustum={outside} max_abs_coord={maxabs} (i32::MAX={})",
            clips.len(), i32::MAX);
        let total = self.machine.gpu3d.total_commands();
        let (gxw, portw) = self.machine.gpu3d.channel_writes();
        let _ = writeln!(s, "recent per-swap poly counts: {:?} | total GX commands executed={total}",
            self.gpu3d_poly_log());
        let _ = writeln!(s, "GX submission channels (lifetime): gxfifo_port_writes={gxw} command_port_writes={portw}");
        let (sub, clip, cull, emit) = self.machine.gpu3d.pipeline_stats();
        let _ = writeln!(s, "geometry pipeline (lifetime): submitted={sub} clipped_out={clip} culled={cull} emitted={emit}");
        let cp = self.machine.gpu3d.clip_plane_stats();
        let _ = writeln!(s, "  clipped-out by plane: left={} right={} bottom={} top={} near={} far={}",
            cp[0], cp[1], cp[2], cp[3], cp[4], cp[5]);
        let (bt_run, bt_pass) = self.machine.gpu3d.box_test_stats();
        let _ = writeln!(s, "box tests (lifetime): run={bt_run} passed_inside={bt_pass} gxstat=0x{:08X}",
            self.machine.gpu3d.gxstat());
        let ((sw_p, sw_v), dropped) = self.machine.gpu3d.swap_debug();
        let (bl_p, bl_v) = self.machine.gpu3d.build_len();
        let _ = writeln!(s, "swap/build: last_swap_saw_build={sw_p}p/{sw_v}v current_build={bl_p}p/{bl_v}v emit_dropped(RAM full)={dropped}");
        // ARM9 DMA channels: any channel enabled and pointed at the GXFIFO (0x4000400)
        // is streaming a display list — the geometry path this screen may depend on.
        for ch in 0..4 {
            let (ctrl, src, dst, cnt) = self.machine.dma[Core::Arm9.index()].channels[ch].debug_regs();
            let en = ctrl & (1 << 15) != 0;
            let mode = (ctrl >> 11) & 7;
            let _ = writeln!(s, "ARM9 DMA{ch}: en={en} start_mode={mode} src={src:#010x} dst={dst:#010x} count={cnt} ctrl={ctrl:#06x}");
        }
        // Per-opcode GX command histogram: which commands the game actually issues.
        let hist = self.machine.gpu3d.cmd_hist();
        let named: &[(u8, &str)] = &[
            (0x10, "MTX_MODE"), (0x11, "MTX_PUSH"), (0x12, "MTX_POP"),
            (0x13, "MTX_STORE"), (0x14, "MTX_RESTORE"), (0x15, "MTX_IDENTITY"),
            (0x16, "MTX_LOAD_4x4"), (0x17, "MTX_LOAD_4x3"), (0x18, "MTX_MULT_4x4"),
            (0x19, "MTX_MULT_4x3"), (0x1A, "MTX_MULT_3x3"), (0x1B, "MTX_SCALE"),
            (0x1C, "MTX_TRANS"), (0x20, "COLOR"), (0x21, "NORMAL"),
            (0x22, "TEXCOORD"), (0x23, "VTX_16"), (0x24, "VTX_10"),
            (0x25, "VTX_XY"), (0x26, "VTX_XZ"), (0x27, "VTX_YZ"),
            (0x28, "VTX_DIFF"), (0x29, "POLYGON_ATTR"), (0x2A, "TEXIMAGE_PARAM"),
            (0x2B, "PLTT_BASE"), (0x30, "DIF_AMB"), (0x31, "SPE_EMI"),
            (0x32, "LIGHT_VECTOR"), (0x33, "LIGHT_COLOR"), (0x34, "SHININESS"),
            (0x40, "BEGIN_VTXS"), (0x41, "END_VTXS"), (0x50, "SWAP_BUFFERS"),
            (0x60, "VIEWPORT"), (0x70, "BOX_TEST"), (0x71, "POS_TEST"),
            (0x72, "VEC_TEST"),
        ];
        let mut line = String::from("GX command histogram (lifetime):");
        for &(op, name) in named {
            let c = hist[op as usize];
            if c > 0 {
                let _ = write!(line, " {name}={c}");
            }
        }
        let _ = writeln!(s, "{line}");
        // Delta since the previous F5 dump: what actually moved in this interval.
        // This is the signal that matters — lifetime totals are dominated by earlier
        // screens, but the delta is exactly what the *current* screen is doing.
        if let Some((p_tot, p_sub, p_emit, p_hist)) = &self.dbg_prev {
            let mut d = format!(
                "DELTA since last F5: commands=+{} submitted=+{} emitted=+{} |",
                total.saturating_sub(*p_tot),
                sub.saturating_sub(*p_sub),
                emit.saturating_sub(*p_emit),
            );
            for &(op, name) in named {
                let dc = hist[op as usize].saturating_sub(p_hist[op as usize]);
                if dc > 0 {
                    let _ = write!(d, " {name}=+{dc}");
                }
            }
            let _ = writeln!(s, "{d}");
        } else {
            let _ = writeln!(s, "DELTA since last F5: (first dump — press F5 again to see per-interval movement)");
        }
        self.dbg_prev = Some((total, sub, emit, Box::new(*hist)));
        let dispcap = self.read(Core::Arm9, 0x0400_0064, 4);
        let _ = writeln!(s, "DISPCAPCNT={dispcap:#010x} (capture_enable={} src={} dst_block={})",
            (dispcap >> 31) & 1, (dispcap >> 29) & 3, (dispcap >> 16) & 3);
        let pc = self.arm9.register(15);
        let thumb = self.arm9.cpsr().thumb();
        let _ = writeln!(s, "ARM9 PC={pc:#010x} thumb={thumb} irq_en={} | ARM7 PC={:#010x}",
            self.arm9.irq_enabled(), self.arm7.register(15));
        // Disassemble a window around the ARM9 PC (to reveal a spin/wait loop).
        let step: u32 = if thumb { 2 } else { 4 };
        for i in 0..12u32 {
            let a = pc.wrapping_sub(step * 6).wrapping_add(step * i);
            let mark = if a == pc { ">>" } else { "  " };
            let text = if thumb {
                let w = self.read(Core::Arm9, a, 2) as u16;
                format!("{w:04x}     {}", arm::format_thumb(&arm::decode_thumb(w)))
            } else {
                let w = self.read(Core::Arm9, a, 4);
                format!("{w:08x} {}", arm::format_arm(&arm::decode_arm(w)))
            };
            let _ = writeln!(s, "{mark} {a:#010x}: {text}");
        }
        let modes = self.gpu3d_poly_texcoord_modes();
        for (i, (fmt, _off, pb, color, alpha)) in self.gpu3d_poly_texture_debug().iter().enumerate().take(16) {
            let _ = writeln!(s, "  poly{i}: fmt={fmt} tcmode={} pltt={pb:#x} texel={color:?} alpha={alpha}",
                modes.get(i).copied().unwrap_or(0));
        }
        s
    }

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
            0xE3A0_1402,                  // mov r1, #0x02000000
            0xE281_1C00 | (off >> 8),     // add r1, r1, #off  (off = imm8 ROR 24)
            0xE3A0_0000 | (value & 0xFF), // mov r0, #value
            0xE581_0000,                  // str r0, [r1]
            0xEAFF_FFFE,                  // b .
        ]
    }

    /// End-to-end: set up a sound channel (from the ARM7's view) over sample data in
    /// Main RAM, run the mixer's schedule, and confirm non-silent audio is produced.
    #[test]
    fn sound_channel_produces_non_silent_audio() {
        let mut system = System::new();
        // A loud constant PCM8 waveform (+100) at 0x0203_0000, one word (4 samples).
        for b in 0..4 {
            system.memory().main[0x3_0000 + b] = 100;
        }
        let w = |s: &mut System, addr: u32, value: u32, bytes: u32| {
            s.io_write(Core::Arm7, addr, value, bytes);
        };
        w(&mut system, 0x0400_0500, (1 << 15) | 127, 2); // SOUNDCNT: master enable + full
        w(&mut system, 0x0400_0404, 0x0203_0000, 4); // ch0 SAD
        w(&mut system, 0x0400_0408, 0xFF00, 2); // ch0 TMR
        w(&mut system, 0x0400_040C, 1, 4); // ch0 LEN = 1 word
        // Start: loop mode, PCM8, full channel volume, centered pan.
        w(&mut system, 0x0400_0400, (1 << 31) | (1 << 27) | (64 << 16) | 127, 4);
        system.start_audio();
        system.run_until(system.now() + crate::sound::CYCLES_PER_SAMPLE * 64);
        let audio = system.take_audio();
        assert!(!audio.is_empty(), "the mixer should emit samples");
        assert!(audio.iter().any(|&s| s != 0), "a playing channel should be audible");
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
        system.io_write(
            Core::Arm7,
            0x0400_0210,
            IrqSource::IpcRecvNotEmpty.mask(),
            4,
        ); // IE
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
        assert_eq!(
            system.interrupts(Core::Arm7).iflags(),
            IrqSource::Timer0.mask()
        );
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
                    system.memory().main[0x1000 + i as usize * 4..][..4]
                        .try_into()
                        .unwrap(),
                );
                assert_eq!(
                    word,
                    [0x1111_1111, 0x2222_2222, 0x3333_3333, 0x4444_4444][i as usize]
                );
            }
            // Completion raised the DMA0 interrupt and disabled the channel.
            assert_eq!(system.interrupts(core).iflags(), IrqSource::Dma0.mask());
            assert_eq!(system.io_read(core, 0x0400_00BA, 2) & (1 << 15), 0);
        }
    }

    #[test]
    fn vblank_dma_fires_only_when_the_frame_enters_vblank() {
        // Games refresh OAM/palette/scroll every frame via a V-blank DMA (start mode 1).
        // It must NOT run when armed (only immediate does), and then run at V-blank.
        let mut system = System::new();
        let seed = [0x1111_1111u32, 0x2222_2222, 0x3333_3333, 0x4444_4444];
        for (i, w) in seed.iter().enumerate() {
            system.memory().main[i * 4..][..4].copy_from_slice(&w.to_le_bytes());
        }
        system.io_write(Core::Arm9, 0x0400_00B0, 0x0200_0000, 4); // SAD
        system.io_write(Core::Arm9, 0x0400_00B4, 0x0200_1000, 4); // DAD
        system.io_write(Core::Arm9, 0x0400_00B8, 4, 2); // count = 4 words
        // enable | 32-bit | start mode 1 (V-blank, ARM9 bits 11-13 = 001).
        system.io_write(Core::Arm9, 0x0400_00BA, (1 << 15) | (1 << 10) | (1 << 11), 2);

        // Not yet: V-blank timing does not run at arm time.
        assert_eq!(system.memory().main[0x1000..0x1004], [0; 4]);
        // A full frame reaches V-blank and fires it.
        system.run_frame();
        for (i, w) in seed.iter().enumerate() {
            let got = u32::from_le_bytes(system.memory().main[0x1000 + i * 4..][..4].try_into().unwrap());
            assert_eq!(got, *w, "word {i}");
        }
    }

    #[test]
    fn hblank_dma_fires_during_a_visible_scanline() {
        // Per-scanline raster effects use an ARM9 H-blank DMA (start mode 2); it fires at
        // the first visible line's H-blank, well before the frame completes.
        let mut system = System::new();
        let seed = [0xAAAA_AAAAu32, 0xBBBB_BBBB];
        for (i, w) in seed.iter().enumerate() {
            system.memory().main[i * 4..][..4].copy_from_slice(&w.to_le_bytes());
        }
        system.io_write(Core::Arm9, 0x0400_00B0, 0x0200_0000, 4); // SAD
        system.io_write(Core::Arm9, 0x0400_00B4, 0x0200_1000, 4); // DAD
        system.io_write(Core::Arm9, 0x0400_00B8, 2, 2); // count = 2 words
        // enable | 32-bit | start mode 2 (H-blank, ARM9 bits 11-13 = 010).
        system.io_write(Core::Arm9, 0x0400_00BA, (1 << 15) | (1 << 10) | (1 << 12), 2);

        assert_eq!(system.memory().main[0x1000..0x1008], [0; 8], "ran before any H-blank");
        // One scanline's worth of time covers the first H-blank.
        system.start_video();
        system.run_until(system.now() + crate::ppu::CYCLES_PER_LINE + 4);
        let got = u32::from_le_bytes(system.memory().main[0x1000..0x1004].try_into().unwrap());
        assert_eq!(got, 0xAAAA_AAAA, "H-blank DMA did not fire on a visible line");
    }

    #[test]
    fn gxfifo_dma_streams_geometry_commands_to_the_3d_engine() {
        // Games submit heavy 3D scenes via GXFIFO DMA (ARM9 start-mode 7). Regression
        // for that being dropped as an unmodelled "Special" timing (→ black 3D).
        let mut system = System::new();
        let one = 0x1000u32; // matrix 4.12 unit
        // GX command stream: MTX_MODE=position(1); MTX_TRANS(2.0, 0, 0).
        let cmds = [0x10u32, 1, 0x1C, 2 * one, 0, 0];
        for (i, w) in cmds.iter().enumerate() {
            system.memory().main[i * 4..][..4].copy_from_slice(&w.to_le_bytes());
        }
        // DMA0 (ARM9): source main RAM → GXFIFO, 32-bit, dest-fixed, start mode 7.
        system.io_write(Core::Arm9, 0x0400_00B0, 0x0200_0000, 4); // SAD
        system.io_write(Core::Arm9, 0x0400_00B4, 0x0400_0400, 4); // DAD = GXFIFO
        system.io_write(Core::Arm9, 0x0400_00B8, cmds.len() as u32, 2); // word count
        let cnt = 0x8000 | 0x0400 | 0x3800 | 0x0040; // enable | 32bit | mode 7 | dest fixed
        system.io_write(Core::Arm9, 0x0400_00BA, cnt, 2); // control → runs the DMA now
        // The stream reached the geometry engine: the clip matrix's translation-x
        // (CLIPMTX element 12) is 2.0.
        assert_eq!(system.io_read(Core::Arm9, 0x0400_0640 + 12 * 4, 4), 2 * one);
    }

    #[test]
    fn immediate_dma_to_the_gxfifo_reaches_the_3d_engine() {
        // Games also stream display lists to the GXFIFO via an *immediate*-timing DMA
        // (start mode 0) with a fixed destination — not only start-mode-7. That path
        // runs through `data_write`, which used to drop I/O-region writes to the memory
        // map (→ the geometry silently vanished and the 3D screen went black).
        let mut system = System::new();
        let one = 0x1000u32; // matrix 4.12 unit
        // GX command stream: MTX_MODE=position(1); MTX_TRANS(3.0, 0, 0).
        let cmds = [0x10u32, 1, 0x1C, 3 * one, 0, 0];
        for (i, w) in cmds.iter().enumerate() {
            system.memory().main[i * 4..][..4].copy_from_slice(&w.to_le_bytes());
        }
        system.io_write(Core::Arm9, 0x0400_00B0, 0x0200_0000, 4); // SAD
        system.io_write(Core::Arm9, 0x0400_00B4, 0x0400_0400, 4); // DAD = GXFIFO
        system.io_write(Core::Arm9, 0x0400_00B8, cmds.len() as u32, 2); // word count
        let cnt = 0x8000 | 0x0400 | 0x0040; // enable | 32bit | dest fixed | mode 0 (immediate)
        system.io_write(Core::Arm9, 0x0400_00BA, cnt, 2); // control → runs the DMA now
        // The stream reached the geometry engine: CLIPMTX translation-x is 3.0.
        assert_eq!(system.io_read(Core::Arm9, 0x0400_0640 + 12 * 4, 4), 3 * one);
    }

    #[test]
    fn cart_dma_reads_streams_rom_and_raises_transfer_irq() {
        use crate::IrqSource;
        let mut system = System::new();

        // A cartridge with a recognisable pattern in the main data area (>= 0x8000).
        let mut rom = vec![0u8; 0x9000];
        for (i, b) in rom[0x8000..0x8200].iter_mut().enumerate() {
            *b = (i as u8) ^ 0x5A;
        }
        system.machine.cart.insert(&rom);

        // ARM9 arms the gamecard transfer IRQ and its interrupt controller.
        system.io_write(Core::Arm9, 0x0400_01A0, (1 << 15) | (1 << 14), 2); // AUXSPICNT: slot + IRQ
        system.io_write(Core::Arm9, 0x0400_0210, IrqSource::Gamecard.mask(), 4); // IE
        system.io_write(Core::Arm9, 0x0400_0208, 1, 4); // IME

        // A cart-mode DMA: fixed source = data port, dest = Main RAM, 0x80 words,
        // 32-bit, source-fixed, ARM9 start mode 5 (bits 11-13), enabled.
        let cnt_h: u32 = (1 << 15) | (1 << 10) | (2 << 7) | (5 << 11);
        system.io_write(Core::Arm9, 0x0400_00B0, crate::cart::DATA_PORT, 4); // SAD
        system.io_write(Core::Arm9, 0x0400_00B4, 0x0200_0000, 4); // DAD
        system.io_write(Core::Arm9, 0x0400_00B8, 0x80, 2); // count = 0x200 bytes
        system.io_write(Core::Arm9, 0x0400_00BA, cnt_h, 2); // enabled, waits for cart start
                                                            // Not auto-run: the channel is armed but idle until the block starts.
        assert_eq!(system.memory().main[0], 0);

        // Command B7, read from ROM address 0x8000 (MSB-first in the 8-byte buffer:
        // byte0=B7, byte3=0x80 -> address param 0x00008000).
        let command = 0xB7u32 | (0x80 << 24);
        system.io_write(Core::Arm9, 0x0400_01A8, command, 4);
        system.io_write(Core::Arm9, 0x0400_01AC, 0, 4);
        // ROMCTRL: 0x200-byte block (field 1), start.
        system.io_write(Core::Arm9, 0x0400_01A4, (1 << 31) | (1 << 24), 4);

        // The whole block streamed into Main RAM, byte-for-byte from ROM[0x8000..].
        assert_eq!(&system.memory().main[..0x200], &rom[0x8000..0x8200]);
        // The block completed: busy clear and the transfer-complete IRQ pending.
        assert_eq!(system.io_read(Core::Arm9, 0x0400_01A4, 4) & (1 << 31), 0);
        assert_eq!(
            system.interrupts(Core::Arm9).iflags(),
            IrqSource::Gamecard.mask()
        );
    }

    #[test]
    fn cart_manual_read_polls_and_completes() {
        let mut system = System::new();
        let mut rom = vec![0u8; 0x9000];
        rom[0x8000..0x8008].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        system.machine.cart.insert(&rom);

        system.io_write(Core::Arm9, 0x0400_01A0, 1 << 15, 2); // slot enable, no IRQ
        let command = 0xB7u32 | (0x80 << 24);
        system.io_write(Core::Arm9, 0x0400_01A8, command, 4);
        system.io_write(Core::Arm9, 0x0400_01AC, 0, 4);
        // 4-byte block (field 7), start.
        system.io_write(Core::Arm9, 0x0400_01A4, (1 << 31) | (7 << 24), 4);

        // DRQ set, one manual word available, matching ROM[0x8000..0x8004].
        assert_ne!(system.io_read(Core::Arm9, 0x0400_01A4, 4) & (1 << 23), 0);
        assert_eq!(system.io_read(Core::Arm9, 0x0410_0010, 4), 0x0403_0201);
        // Block drained: no longer busy.
        assert_eq!(system.io_read(Core::Arm9, 0x0400_01A4, 4) & (1 << 31), 0);
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
            system.write(
                Core::Arm9,
                0x0680_0000 + i as u32 * 2,
                (i & 0x7FFF) as u32,
                2,
            );
        }
        system.io_write(Core::Arm9, 0x0400_0000, 2 << 16, 4); // DISPCNT: VRAM display, block A

        system.run_frame();

        // One frame completed; VCOUNT wrapped back into the visible region.
        assert_eq!(system.frame(), 1);
        assert!(system.io_read(Core::Arm9, 0x0400_0006, 2) as u16 <= 263);
        // The V-blank interrupt reached both cores.
        assert_eq!(
            system.interrupts(Core::Arm9).iflags(),
            IrqSource::VBlank.mask()
        );
        assert_eq!(
            system.interrupts(Core::Arm7).iflags(),
            IrqSource::VBlank.mask()
        );
        // The framebuffer holds the blitted gradient.
        let fb = system.framebuffer();
        assert_eq!(fb[0], 0);
        assert_eq!(fb[100], 100);
        assert_eq!(fb[0x1234], 0x1234);
    }

    #[test]
    fn engine_a_captures_registers_and_renders_backdrop() {
        let mut system = System::new();
        // Write the Engine A 2D register block: BG0CNT (0x4000008) priority + a
        // BLDCNT (0x4000050). These land in the shared video2d::Registers.
        system.io_write(Core::Arm9, 0x0400_0008, 0x1234, 2); // BG0CNT
        system.io_write(Core::Arm9, 0x0400_0050, 0x00FF, 2); // BLDCNT
        assert_eq!(system.io_read(Core::Arm9, 0x0400_0008, 2), 0x1234);
        assert_eq!(system.io_read(Core::Arm9, 0x0400_0050, 2), 0x00FF);

        // A red backdrop in Engine A BG palette entry 0, graphics display mode.
        system.write(Core::Arm9, 0x0500_0000, 0x001F, 2); // BGR555 red
        system.io_write(Core::Arm9, 0x0400_0000, 1 << 16, 4); // DISPCNT: graphics mode
        system.run_frame();

        // The whole top screen shows the backdrop color.
        let fb = system.framebuffer();
        assert!(fb.iter().all(|&p| p == 0x001F));
    }

    #[test]
    fn keypad_reads_active_low_on_both_cores() {
        let mut system = System::new();
        assert_eq!(system.io_read(Core::Arm9, 0x0400_0130, 2), 0x03FF); // all released
        system.set_keypad((1 << 0) | (1 << 7)); // press A + Down
        let v = system.io_read(Core::Arm7, 0x0400_0130, 2); // both cores see it
        assert_eq!(v & 1, 0); // A pressed (bit cleared)
        assert_eq!(v & (1 << 7), 0); // Down pressed
        assert_eq!(v & (1 << 1), 1 << 1); // B still released
    }

    #[test]
    fn direct_boot_runs_both_cores_from_their_entry_points() {
        fn put(rom: &mut [u8], off: usize, v: u32) {
            rom[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }
        let mut rom = vec![0u8; 0x6000];
        // Header: ARM9 binary at rom 0x4000 → 0x0200_0000; ARM7 at 0x5000 → 0x0210_0000.
        put(&mut rom, 0x20, 0x4000);
        put(&mut rom, 0x24, 0x0200_0000);
        put(&mut rom, 0x28, 0x0200_0000);
        put(&mut rom, 0x2C, 16);
        put(&mut rom, 0x30, 0x5000);
        put(&mut rom, 0x34, 0x0210_0000);
        put(&mut rom, 0x38, 0x0210_0000);
        put(&mut rom, 0x3C, 16);
        // ARM9: write 0xA9 to 0x0200_0300, then park.
        for (i, w) in [0xE3A0_1402u32, 0xE3A0_00A9, 0xE581_0300, 0xEAFF_FFFE]
            .iter()
            .enumerate()
        {
            put(&mut rom, 0x4000 + i * 4, *w);
        }
        // ARM7: write 0x77 to 0x0200_0400, then park.
        for (i, w) in [0xE3A0_1402u32, 0xE3A0_0077, 0xE581_0400, 0xEAFF_FFFE]
            .iter()
            .enumerate()
        {
            put(&mut rom, 0x5000 + i * 4, *w);
        }

        let mut system = System::new();
        system.direct_boot(&rom).unwrap();
        system.run_until(400);

        // Both cores booted from their entry points and ran their code.
        assert_eq!(system.read(Core::Arm9, 0x0200_0300, 1), 0xA9);
        assert_eq!(system.read(Core::Arm7, 0x0200_0400, 1), 0x77);
        assert_eq!(system.arm9.register(15) & !3, 0x0200_000C); // parked at the B .
                                                                // Direct boot seeds the BIOS TCM state so BIOS-less homebrew's stack works.
        assert!(system.cp15().dtcm_enabled());
        assert_eq!(system.cp15().dtcm_base(), 0x0080_0000);
    }

    #[test]
    fn direct_boot_seeds_firmware_state() {
        fn put(rom: &mut [u8], off: usize, v: u32) {
            rom[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }
        let mut rom = vec![0u8; 0x6000];
        put(&mut rom, 0x20, 0x4000); // ARM9 rom offset
        put(&mut rom, 0x24, 0x0200_0000); // entry
        put(&mut rom, 0x28, 0x0200_0000); // ram
        put(&mut rom, 0x2C, 4); // size
        put(&mut rom, 0x30, 0x5000); // ARM7 rom offset
        put(&mut rom, 0x34, 0x0210_0000);
        put(&mut rom, 0x38, 0x0210_0000);
        put(&mut rom, 0x3C, 4);
        rom[0x15E..0x160].copy_from_slice(&0xABCDu16.to_le_bytes()); // header CRC
        rom[0x6C..0x6E].copy_from_slice(&0x1234u16.to_le_bytes()); // secure CRC
                                                                   // Park both cores immediately.
        put(&mut rom, 0x4000, 0xEAFF_FFFE);
        put(&mut rom, 0x5000, 0xEAFF_FFFE);

        let mut system = System::new();
        system.direct_boot(&rom).unwrap();

        // POSTFLG set on both cores (games refuse to run otherwise).
        assert_eq!(system.read(Core::Arm9, 0x0400_0300, 1) & 1, 1);
        assert_eq!(system.read(Core::Arm7, 0x0400_0300, 1) & 1, 1);
        // Boot indicator = normal; header/secure CRC mirrored into the footer.
        assert_eq!(system.read(Core::Arm9, 0x027F_FC40, 2), 1);
        assert_eq!(system.read(Core::Arm9, 0x027F_F808, 2), 0xABCD);
        assert_eq!(system.read(Core::Arm9, 0x027F_F80A, 2), 0x1234);
        // Inter-core boot handshake words.
        assert_eq!(system.read(Core::Arm9, 0x027F_F880, 4), 7);
        assert_eq!(system.read(Core::Arm9, 0x027F_F884, 4), 6);
        // User settings: version 5, English + settings-okay flags, no prompt.
        assert_eq!(system.read(Core::Arm9, 0x027F_FC80, 2), 5);
        assert_eq!(system.read(Core::Arm9, 0x027F_FC80 + 0x64, 2) & 0x7, 1); // language English
    }

    #[test]
    fn direct_boot_gives_arm7_the_shared_wram_mirror() {
        // WRAMCNT=3 hands the 32K Shared WRAM to the ARM7, mirrored at 0x037F8000
        // (GBATEK "Shared-RAM": Shared + ARM7-WRAM form one continuous 96K block).
        // A retail ARM7 relocates its code into that mirror; if it maps to ARM7-WRAM
        // instead (WRAMCNT=0) the ARM7 reads zeroes and crashes into cleared BSS.
        fn put(rom: &mut [u8], off: usize, v: u32) {
            rom[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }
        let mut rom = vec![0u8; 0x6000];
        put(&mut rom, 0x20, 0x4000);
        put(&mut rom, 0x24, 0x0200_0000);
        put(&mut rom, 0x28, 0x0200_0000);
        put(&mut rom, 0x2C, 4);
        put(&mut rom, 0x30, 0x5000);
        put(&mut rom, 0x34, 0x0210_0000);
        put(&mut rom, 0x38, 0x0210_0000);
        put(&mut rom, 0x3C, 4);
        put(&mut rom, 0x4000, 0xEAFF_FFFE);
        put(&mut rom, 0x5000, 0xEAFF_FFFE);

        let mut system = System::new();
        system.direct_boot(&rom).unwrap();
        // The ARM7 reads WRAMCNT as 3 (32K allocated to it).
        assert_eq!(system.read(Core::Arm7, 0x0400_0241, 1) & 3, 3);
        // A write the ARM7 makes to 0x037F8000 is visible via the 0x03000000 Shared
        // WRAM window (same 32K), and NOT via ARM7-WRAM at 0x03808000.
        system.write(Core::Arm7, 0x037F_8000, 0xC0DE_0007, 4);
        assert_eq!(system.read(Core::Arm7, 0x0300_0000, 4), 0xC0DE_0007);
        assert_eq!(system.read(Core::Arm7, 0x0380_8000, 4), 0);
    }

    #[test]
    fn engine_a_renders_a_text_background_through_video2d() {
        let mut system = System::new();
        // Block A → 2D Engine-A BG VRAM at 0x0600_0000.
        system.io_write(Core::Arm9, 0x0400_0240, 0x80 | 1, 1); // VRAMCNT_A: enable, MST 1
                                                               // Tile 0 (4bpp, char base 0): every pixel is palette index 1 (bytes 0x11).
        for i in 0..8u32 {
            system.write(Core::Arm9, 0x0600_0000 + i * 4, 0x1111_1111, 4);
        }
        // The tilemap at screen-base block 1 (0x800) is zero-filled, so every map
        // entry selects tile 0. BG palette entry 1 = green.
        system.write(Core::Arm9, 0x0500_0002, 0x03E0, 2);
        // BG0CNT: screen base block 1 (bits 8-12), char base 0, 4bpp, size 0.
        system.io_write(Core::Arm9, 0x0400_0008, 1 << 8, 2);
        // DISPCNT: graphics display mode (bit 16), BG mode 0, BG0 enabled (bit 8).
        system.io_write(Core::Arm9, 0x0400_0000, (1 << 16) | (1 << 8), 4);

        system.run_frame();

        // The whole screen is tile 0 → palette index 1 → green, composited by the
        // shared renderer over the assembled banked VRAM.
        let fb = system.framebuffer();
        assert_eq!(fb[0], 0x03E0);
        assert_eq!(fb[137], 0x03E0);
        assert_eq!(fb[crate::ppu::WIDTH * 100 + 200], 0x03E0);
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
