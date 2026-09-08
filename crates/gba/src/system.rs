//! The top-level GBA system: the guest timeline and the machine that share it.
//!
//! This owns the [`Scheduler`] alongside the [`Gba`] devices — deliberately not
//! inside the machine, so a device can schedule follow-up events through the
//! event context without a self-borrow. It will grow the CPU-driven execution
//! loop once an interpreter exists; for now it implements the HALT/wake
//! progression, which needs no CPU and is an early end-to-end scheduler test.

use crate::bus::Bus;
use crate::event::EventKind;
use crate::machine::Gba;
use crate::ppu::debug::{ExplainError, PixelExplanation, ScanlineExplanation, VideoInstrumentation};
use crate::ppu::Color15;
use crate::trace::{describe_event, io_register_name, Trace};
use arm::cpu::{Bus as CpuMemory, Cpu, Timed};
use emu_core::{Access, AccessKind, AccessSequence, Scheduler, Timestamp};

/// The whole GBA: the CPU, one timeline, and the device machine.
#[derive(Clone, Debug, Default)]
pub struct System {
    pub cpu: Cpu,
    pub scheduler: Scheduler<EventKind>,
    pub gba: Gba,
    /// Optional event trace (off by default; see [`System::enable_trace`]).
    trace: Option<Trace>,
    /// Which video instrumentation a running frame collects (off by default).
    video_debug: VideoInstrumentation,
    /// Whether the continuous LCD scanline schedule has been started.
    lcd_started: bool,
    /// Whether the continuous audio sample schedule has been started.
    apu_started: bool,
}

/// Cycles in one scanline (see [`crate::ppu::timing`]).
const CYCLES_PER_LINE: Timestamp = 1232;
/// Scanlines in one frame.
const LINES_PER_FRAME: u32 = 228;

/// Adapts the GBA [`Bus`] and [`Scheduler`] to the CPU's memory interface: it
/// tags each access as a CPU instruction/data access, advances the timeline by
/// the cycles consumed, folds in any DMA stall a store triggered, and steps the
/// ROM prefetcher on internal cycles.
struct CpuBus<'a> {
    bus: &'a mut Bus,
    scheduler: &'a mut Scheduler<EventKind>,
    trace: Option<&'a mut Trace>,
}

impl CpuBus<'_> {
    fn advance(&mut self, cycles: u32) {
        let now = self.scheduler.now().saturating_add(cycles as u64);
        self.scheduler.set_now(now);
    }

    /// Advance for a data access. The GamePak prefetch unit works during any cycle
    /// the CPU is not on the cartridge bus, so a data access to *other* memory
    /// (BIOS/RAM/VRAM/I-O) lets the prefetcher buffer opcodes ahead — accesses to
    /// the cartridge itself (ROM/SRAM) do not.
    fn advance_data(&mut self, address: u32, cycles: u32) {
        if address < 0x0800_0000 {
            self.bus.step_prefetch(cycles);
        }
        self.advance(cycles);
    }

    /// Record an MMIO write (and any event it (re)scheduled) into the trace.
    fn trace_write(&mut self, address: u32, value: u32, scheduling_changed: bool) {
        // Only the I/O region is interesting, and only when tracing is on.
        if !(0x0400_0000..0x0500_0000).contains(&address) {
            return;
        }
        let now = self.scheduler.now();
        let deadline = self.scheduler.next_deadline();
        if let Some(trace) = self.trace.as_deref_mut() {
            trace.record(
                now,
                format!("CPU write {} = {value:#06x}", io_register_name(address)),
            );
            if scheduling_changed {
                if let Some(at) = deadline {
                    trace.record(now, format!("scheduled next event @ {at}"));
                }
            }
        }
    }
}

fn cpu_access(kind: AccessKind, sequential: bool) -> Access {
    let sequence = if sequential {
        AccessSequence::Sequential
    } else {
        AccessSequence::NonSequential
    };
    Access::cpu(kind, sequence)
}

impl CpuMemory for CpuBus<'_> {
    fn fetch32(&mut self, address: u32, sequential: bool) -> Timed<u32> {
        let read = self
            .bus
            .read32(address, cpu_access(AccessKind::Instruction, sequential), self.scheduler);
        self.advance_data(address, read.cycles);
        Timed { value: read.value, cycles: read.cycles }
    }
    fn fetch16(&mut self, address: u32, sequential: bool) -> Timed<u16> {
        let read = self
            .bus
            .read16(address, cpu_access(AccessKind::Instruction, sequential), self.scheduler);
        self.advance_data(address, read.cycles);
        Timed { value: read.value, cycles: read.cycles }
    }
    fn load32(&mut self, address: u32, sequential: bool) -> Timed<u32> {
        let read = self
            .bus
            .read32(address, cpu_access(AccessKind::Data, sequential), self.scheduler);
        self.advance_data(address, read.cycles);
        Timed { value: read.value, cycles: read.cycles }
    }
    fn load16(&mut self, address: u32, sequential: bool) -> Timed<u16> {
        let read = self
            .bus
            .read16(address, cpu_access(AccessKind::Data, sequential), self.scheduler);
        self.advance_data(address, read.cycles);
        Timed { value: read.value, cycles: read.cycles }
    }
    fn load8(&mut self, address: u32, sequential: bool) -> Timed<u8> {
        let read = self
            .bus
            .read8(address, cpu_access(AccessKind::Data, sequential), self.scheduler);
        self.advance_data(address, read.cycles);
        Timed { value: read.value, cycles: read.cycles }
    }
    fn store32(&mut self, address: u32, value: u32, sequential: bool) -> u32 {
        let write = self
            .bus
            .write32(address, value, cpu_access(AccessKind::Data, sequential), self.scheduler);
        self.trace_write(address, value, write.scheduling_changed);
        let cycles = write.cycles + self.bus.take_dma_stall_cycles() as u32;
        self.advance_data(address, cycles);
        cycles
    }
    fn store16(&mut self, address: u32, value: u16, sequential: bool) -> u32 {
        let write = self
            .bus
            .write16(address, value, cpu_access(AccessKind::Data, sequential), self.scheduler);
        self.trace_write(address, value as u32, write.scheduling_changed);
        let cycles = write.cycles + self.bus.take_dma_stall_cycles() as u32;
        self.advance_data(address, cycles);
        cycles
    }
    fn store8(&mut self, address: u32, value: u8, sequential: bool) -> u32 {
        let write = self
            .bus
            .write8(address, value, cpu_access(AccessKind::Data, sequential), self.scheduler);
        self.trace_write(address, value as u32, write.scheduling_changed);
        let cycles = write.cycles + self.bus.take_dma_stall_cycles() as u32;
        self.advance_data(address, cycles);
        cycles
    }
    fn internal(&mut self, cycles: u32) {
        self.bus.step_prefetch(cycles);
        self.advance(cycles);
    }
}

/// The result of running (a portion of) a video frame via [`System::run_frame_step`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameOutcome {
    /// The video frame finished; the framebuffer holds the completed image.
    Completed,
    /// A connected serial transfer is waiting for the peer's word. The host must
    /// exchange a link frame (poll_out → carrier → deliver) and call again to resume
    /// the same frame. Only occurs when a link carrier is attached.
    LinkPending,
}

/// The result of [`System::run_step`]: a bounded slice of a frame, so a linked host can
/// service its carrier between slices (sub-frame granularity) rather than once per frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepOutcome {
    /// The video frame finished.
    FrameComplete,
    /// A connected serial transfer awaits the peer's word (exchange, then call again).
    LinkPending,
    /// The slice's cycle budget ran out mid-frame with no barrier — call again to continue
    /// (the host can service its carrier first).
    Yielded,
}

/// The result of progressing a low-power machine by one event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HaltProgress {
    /// A pending interrupt woke the CPU; it is now running.
    Woke,
    /// An event was processed but nothing woke the CPU yet.
    StillHalted,
    /// No events remain, so nothing can ever wake the CPU. On real hardware
    /// this is a hang; surfaced here as a distinct outcome for debugging.
    Deadlocked,
}

impl System {
    pub fn new() -> Self {
        Self::default()
    }

    /// Turn on event tracing (see [`crate::trace::Trace`]). Off by default.
    pub fn enable_trace(&mut self) {
        self.trace = Some(Trace::new());
    }

    /// The collected trace, if tracing is enabled.
    pub fn trace(&self) -> Option<&Trace> {
        self.trace.as_ref()
    }

    /// Take ownership of the collected trace, leaving tracing enabled but empty.
    pub fn take_trace(&mut self) -> Option<Trace> {
        self.trace.as_mut().map(std::mem::take)
    }

    /// Dispatch every event due at the current time into the machine, tracing
    /// each as it fires.
    pub fn run_due_events(&mut self) {
        let trace = &mut self.trace;
        self.scheduler
            .run_due_events_traced(&mut self.gba, |now, kind| {
                if let Some(trace) = trace.as_mut() {
                    trace.record(now, describe_event(kind));
                }
            });
    }

    /// Press a button (raising the keypad interrupt if `KEYCNT` is so configured).
    pub fn press_key(&mut self, key: crate::keypad::Key) {
        self.gba.bus.io.set_key(key, true);
    }

    /// Release a button.
    pub fn release_key(&mut self, key: crate::keypad::Key) {
        self.gba.bus.io.set_key(key, false);
    }

    /// Set a button's pressed state directly.
    pub fn set_key(&mut self, key: crate::keypad::Key, pressed: bool) {
        self.gba.bus.io.set_key(key, pressed);
    }

    /// Begin LCD timing, starting the PPU's continuous scanline schedule. On real
    /// hardware the LCD runs from power-on; this is idempotent so callers can
    /// ensure it without tracking whether it has started.
    pub fn start_lcd(&mut self) {
        if self.lcd_started {
            return;
        }
        self.lcd_started = true;
        let now = self.scheduler.now();
        self.gba.bus.io.video.start(now, &mut self.scheduler);
    }

    /// Begin the continuous audio-sample schedule: one output frame every
    /// [`crate::apu::CYCLES_PER_SAMPLE`] cycles, mixing the current channel state.
    pub fn start_apu(&mut self) {
        if self.apu_started {
            return;
        }
        self.apu_started = true;
        self.scheduler
            .schedule_after(crate::apu::CYCLES_PER_SAMPLE, EventKind::Apu(crate::event::ApuEvent::Sample));
    }

    /// Drain the audio samples generated since the last call — interleaved stereo
    /// `i16` frames — for the host to play.
    pub fn take_audio(&mut self) -> Vec<i16> {
        self.gba.bus.io.apu.take_samples()
    }

    /// Audio clip metering since the last call: (samples clamped, peak pre-clamp
    /// magnitude). A peak above 32767 means the mix is clipping.
    pub fn audio_clip_stats(&mut self) -> (u64, f64) {
        self.gba.bus.io.apu.clip_stats()
    }

    /// Run the machine until the PPU completes the current frame (its 160 visible
    /// scanlines have all been drawn, at the start of the vertical blank). Starts
    /// the LCD if it is not already running. After this returns, [`Self::framebuffer`]
    /// holds the finished frame.
    pub fn run_frame(&mut self) {
        self.start_lcd();
        self.start_apu();
        let start = self.gba.bus.io.video.frame();
        // Advance a scanline at a time until the frame counter ticks. The cap is a
        // safety net against a stalled timeline (a hung CPU with no events).
        for _ in 0..(LINES_PER_FRAME + 2) {
            if self.gba.bus.io.video.frame() != start {
                break;
            }
            let target = self.scheduler.now() + CYCLES_PER_LINE;
            self.run_until(target);
        }
    }

    /// Run (or resume) one video frame, stopping early at a serial transfer barrier.
    ///
    /// Returns [`FrameOutcome::Completed`] when the frame finishes, or
    /// [`FrameOutcome::LinkPending`] when a connected serial transfer is waiting for a
    /// peer's word: the host should exchange a link frame (poll_out → carrier →
    /// deliver) and call this again to resume the *same* frame. When no link carrier is
    /// attached this never returns `LinkPending`, so a single call runs a whole frame —
    /// identical to [`Self::run_frame`].
    pub fn run_frame_step(&mut self) -> FrameOutcome {
        self.start_lcd();
        self.start_apu();
        let start = self.gba.bus.io.video.frame();
        for _ in 0..(LINES_PER_FRAME + 2) {
            if self.gba.bus.io.video.frame() != start {
                return FrameOutcome::Completed;
            }
            let target = self.scheduler.now() + CYCLES_PER_LINE;
            self.run_until(target);
            // `run_until` side-exits the instant a transfer awaits its peer; surface that
            // so the host can exchange before we advance (and read a stale result).
            if self.gba.bus.io.serial_awaiting_peer() {
                return FrameOutcome::LinkPending;
            }
        }
        FrameOutcome::Completed
    }

    /// Run at most `max_cycles` of the current frame, stopping early at a transfer barrier
    /// or the end of the frame. Lets a linked host advance in small slices and service its
    /// carrier between them (sub-frame granularity), so a burst of transfers within one
    /// frame — Pokémon party data, Advance Wars state sync — isn't throttled to one
    /// transfer per frame. `max_cycles` bounds only the slice, not the frame.
    pub fn run_step(&mut self, max_cycles: u64) -> StepOutcome {
        self.start_lcd();
        self.start_apu();
        let start_frame = self.gba.bus.io.video.frame();
        let target = self.scheduler.now() + max_cycles;
        // `run_until` dispatches PPU events along the way (ticking the frame counter) and
        // side-exits the instant a transfer awaits its peer.
        self.run_until(target);
        if self.gba.bus.io.serial_awaiting_peer() {
            StepOutcome::LinkPending
        } else if self.gba.bus.io.video.frame() != start_frame {
            StepOutcome::FrameComplete
        } else {
            StepOutcome::Yielded
        }
    }

    /// Select which video instrumentation a running frame collects. Off by default
    /// and zero-cost when off; mirrors the event-trace opt-in.
    pub fn set_video_instrumentation(&mut self, level: VideoInstrumentation) {
        self.video_debug = level;
    }

    /// The current video instrumentation level.
    pub fn video_instrumentation(&self) -> VideoInstrumentation {
        self.video_debug
    }

    /// The current output image, in canonical BGR555.
    pub fn framebuffer(&self) -> &[Color15] {
        self.gba.bus.io.video.framebuffer()
    }

    /// Explain how the pixel at `(x, y)` in the current state came to be its color.
    pub fn explain_pixel(&mut self, x: u16, y: u16) -> Result<PixelExplanation, ExplainError> {
        self.gba.bus.explain_pixel(x, y)
    }

    /// A whole-scanline debug summary for line `y`.
    pub fn inspect_scanline(&mut self, y: u16) -> ScanlineExplanation {
        self.gba.bus.inspect_scanline(y)
    }

    /// Advance the timeline to `target`, dispatching every event due up to it.
    ///
    /// This is the no-CPU stand-in for the execution loop: with no instructions
    /// to run, time simply advances and device events fire in order. It becomes
    /// the event-dispatch half of the real loop once the CPU can consume the
    /// cycle budget between events.
    pub fn advance_time(&mut self, target: Timestamp) {
        while let Some(deadline) = self.scheduler.next_deadline() {
            if deadline > target {
                break;
            }
            self.scheduler.set_now(deadline);
            self.scheduler.run_due_events(&mut self.gba);
        }
        if self.scheduler.now() < target {
            self.scheduler.set_now(target);
        }
    }

    /// Advance the machine by one step: execute a single CPU instruction, or —
    /// when the timeline has reached the next event deadline, or the CPU is
    /// halted — dispatch the due events (waking the CPU if that made an interrupt
    /// pending). This is the single-step primitive the debugger drives.
    pub fn step(&mut self) {
        // The LCD runs from power-on; ensure its schedule exists so stepping and
        // frame-running behave identically.
        self.start_lcd();
        self.start_apu();
        let can_run_cpu = !self.gba.is_low_power()
            && self
                .scheduler
                .next_deadline()
                .is_none_or(|d| self.scheduler.now() < d);
        if can_run_cpu {
            if self.gba.bus.io.irq.line_asserted() && self.cpu.irq_enabled() {
                self.cpu.take_irq();
            }
            let mut memory = CpuBus {
                bus: &mut self.gba.bus,
                scheduler: &mut self.scheduler,
                trace: self.trace.as_mut(),
            };
            self.cpu.step(&mut memory);
        } else {
            if self.gba.is_low_power() {
                if let Some(deadline) = self.scheduler.next_deadline() {
                    self.scheduler.set_now(deadline.max(self.scheduler.now()));
                }
            }
            self.run_due_events();
            let stall = self.gba.bus.take_dma_stall_cycles();
            if stall > 0 {
                let now = self.scheduler.now().saturating_add(stall);
                self.scheduler.set_now(now);
            }
            if self.gba.is_low_power() && self.gba.should_wake() {
                self.gba.wake();
            }
        }
    }

    /// Run the machine until the guest timeline reaches `target`.
    ///
    /// Each iteration runs the CPU (or, when halted, idles) up to the next event
    /// deadline, then dispatches the events due there. This is the canonical
    /// execution loop: the CPU consumes the cycle budget between events, MMIO it
    /// writes can reschedule, and interrupts are accepted at instruction
    /// boundaries.
    pub fn run_until(&mut self, target: Timestamp) {
        while self.scheduler.now() < target {
            // Transfer barrier: a connected serial transfer that is still waiting for
            // the peer's word suspends execution here, so the host can exchange a link
            // frame before the guest reads the result — keeping the two linked machines
            // in lockstep. Unlinked play never sets this, so it is a no-op then.
            if self.gba.bus.io.serial_awaiting_peer() {
                return;
            }
            // Recompute the deadline every instruction: an MMIO write can
            // schedule an earlier event, and the CPU must side-exit to it.
            let deadline = self
                .scheduler
                .next_deadline()
                .map_or(target, |d| d.min(target));

            if self.scheduler.now() < deadline && !self.gba.is_low_power() {
                // Accept a pending interrupt at this instruction boundary, then
                // execute one instruction.
                if self.gba.bus.io.irq.line_asserted() && self.cpu.irq_enabled() {
                    if let Some(trace) = self.trace.as_mut() {
                        trace.record(self.scheduler.now(), "CPU accepts IRQ");
                    }
                    self.cpu.take_irq();
                }
                let mut memory = CpuBus {
                    bus: &mut self.gba.bus,
                    scheduler: &mut self.scheduler,
                    trace: self.trace.as_mut(),
                };
                self.cpu.step(&mut memory);
            } else {
                // Reached the deadline (or halted): a halted CPU jumps straight
                // to it, then the events due there are dispatched. Never move
                // backward — the previous instruction may have overshot the
                // deadline by a cycle before halting, in which case the event is
                // already due and is simply dispatched at the current time.
                if self.gba.is_low_power() {
                    let now = self.scheduler.now();
                    self.scheduler.set_now(deadline.max(now));
                }
                self.run_due_events();

                // A blank-triggered DMA during dispatch also stalls the CPU.
                let stall = self.gba.bus.take_dma_stall_cycles();
                if stall > 0 {
                    let now = self.scheduler.now().saturating_add(stall);
                    self.scheduler.set_now(now);
                }

                if self.gba.is_low_power() && self.gba.should_wake() {
                    self.gba.wake();
                }
            }
        }
    }

    /// Progress a halted CPU by exactly one event: jump the timeline directly to
    /// the next event (the CPU executes no instructions), dispatch it, and wake
    /// the CPU if that made an interrupt pending.
    pub fn step_halted(&mut self) -> HaltProgress {
        debug_assert!(
            self.gba.is_low_power(),
            "step_halted called while the CPU is running"
        );
        match self.scheduler.advance_to_next_event() {
            None => HaltProgress::Deadlocked,
            Some(_) => {
                self.scheduler.run_due_events(&mut self.gba);
                if self.gba.should_wake() {
                    self.gba.wake();
                    HaltProgress::Woke
                } else {
                    HaltProgress::StillHalted
                }
            }
        }
    }

    /// Progress a halted CPU until it wakes or deadlocks.
    ///
    /// This does not return if the machine can never wake yet keeps generating
    /// events (e.g. a periodic timer whose interrupt is not enabled) — the real
    /// hardware would hang identically. Use [`System::step_halted`] with a
    /// budget when a bounded, inspectable progression is wanted.
    pub fn run_while_halted(&mut self) -> HaltProgress {
        loop {
            match self.step_halted() {
                HaltProgress::StillHalted => continue,
                terminal => return terminal,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interrupt::IrqSource;
    use crate::timer::TimerId;

    const START: u16 = 1 << 7;
    const IRQ: u16 = 1 << 6;

    /// Configure Timer0 to overflow at `256` with its overflow IRQ enabled.
    fn arm_timer0(sys: &mut System) {
        let now = sys.scheduler.now();
        sys.gba.bus.io.timers.write_reload(TimerId::Timer0, 0xFF00);
        sys.gba
            .bus
            .io
            .timers
            .write_control(TimerId::Timer0, START | IRQ, now, &mut sys.scheduler);
    }

    #[test]
    fn halt_wakes_on_timer_irq_without_cpu_work() {
        let mut sys = System::new();
        sys.gba.bus.io.irq.set_ie(IrqSource::Timer0.mask());
        sys.gba.bus.io.irq.set_ime(true);
        arm_timer0(&mut sys);

        sys.gba.write_haltcnt(0x00); // Halt
        assert!(sys.gba.is_low_power());

        assert_eq!(sys.run_while_halted(), HaltProgress::Woke);
        // The timeline reached the overflow with no instruction execution.
        assert_eq!(sys.scheduler.now(), 256);
        assert!(sys.gba.is_running());
        assert!(sys.gba.bus.io.irq.line_asserted());
    }

    #[test]
    fn halt_wakes_even_when_ime_is_off() {
        // Halt wakes on IE & IF regardless of IME; IME only gates acceptance.
        let mut sys = System::new();
        sys.gba.bus.io.irq.set_ie(IrqSource::Timer0.mask());
        sys.gba.bus.io.irq.set_ime(false);
        arm_timer0(&mut sys);
        sys.gba.write_haltcnt(0x00);

        assert_eq!(sys.run_while_halted(), HaltProgress::Woke);
        assert_eq!(sys.scheduler.now(), 256);
        assert!(sys.gba.is_running());
        // Woke, but the CPU would not accept the IRQ with IME clear.
        assert!(sys.gba.bus.io.irq.pending());
        assert!(!sys.gba.bus.io.irq.line_asserted());
    }

    #[test]
    fn step_reports_still_halted_when_event_does_not_wake() {
        let mut sys = System::new();
        // The overflow requests a Timer0 IRQ, but IE has it disabled, so the
        // machine stays halted.
        sys.gba.bus.io.irq.set_ie(0);
        arm_timer0(&mut sys);
        sys.gba.write_haltcnt(0x00);

        assert_eq!(sys.step_halted(), HaltProgress::StillHalted);
        assert_eq!(sys.scheduler.now(), 256);
        assert!(sys.gba.is_low_power());
        // IF records the request even though it did not wake the CPU.
        assert_ne!(sys.gba.bus.io.irq.iflags() & IrqSource::Timer0.mask(), 0);
        assert!(!sys.gba.bus.io.irq.pending());
    }

    #[test]
    fn halt_with_no_events_deadlocks() {
        let mut sys = System::new();
        sys.gba.write_haltcnt(0x00);
        assert_eq!(sys.run_while_halted(), HaltProgress::Deadlocked);
    }

    /// Write little-endian ARM words into IWRAM starting at its base.
    fn load_iwram(sys: &mut System, program: &[u32]) {
        for (i, word) in program.iter().enumerate() {
            sys.gba.bus.memory.iwram[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
    }

    #[test]
    fn cpu_executes_a_program_from_iwram() {
        let mut sys = System::new();
        // mov r0, #5 ; add r0, r0, #3 ; b . (spin)
        load_iwram(&mut sys, &[0xE3A0_0005, 0xE280_0003, 0xEAFF_FFFE]);
        sys.cpu.set_pc(0x0300_0000);
        sys.run_until(200);
        assert_eq!(sys.cpu.register(0), 8);
        assert!(sys.scheduler.now() >= 200);
    }

    #[test]
    fn cpu_services_a_timer_interrupt() {
        use arm::cpu::Mode;
        let mut sys = System::new();
        // A spin loop the CPU runs until the interrupt arrives.
        load_iwram(&mut sys, &[0xEAFF_FFFE]); // b .
        sys.cpu.set_pc(0x0300_0000);

        // Enable Timer0's interrupt, and arm it to overflow at cycle 50.
        sys.gba.bus.io.irq.set_ie(IrqSource::Timer0.mask());
        sys.gba.bus.io.irq.set_ime(true);
        let now = sys.scheduler.now();
        sys.gba.bus.io.timers.write_reload(TimerId::Timer0, 0xFFCE); // 0x10000 - 50
        sys.gba
            .bus
            .io
            .timers
            .write_control(TimerId::Timer0, START | IRQ, now, &mut sys.scheduler);

        sys.run_until(100);
        // The overflow requested the IRQ, and the CPU accepted it: it is now in
        // IRQ mode, executing from the exception vector.
        assert_eq!(sys.cpu.mode(), Some(Mode::Irq));
        assert_ne!(sys.gba.bus.io.irq.iflags() & IrqSource::Timer0.mask(), 0);
    }

    #[test]
    fn trace_captures_the_timer_interrupt_causal_chain() {
        let mut sys = System::new();
        sys.enable_trace();
        // A program that configures Timer0's interrupt entirely through MMIO,
        // then spins: enable IE/IME, then write reload+control in one 32-bit
        // store to the timer register block.
        load_iwram(
            &mut sys,
            &[
                0xE3A0_1301, // mov r1, #0x04000000   (I/O base)
                0xE3A0_0008, // mov r0, #8            (Timer0 IRQ bit)
                0xE581_0200, // str r0, [r1, #0x200]  ; IE
                0xE3A0_0001, // mov r0, #1
                0xE581_0208, // str r0, [r1, #0x208]  ; IME
                0xE3A0_0CFF, // mov r0, #0xFF00       (reload -> overflow in 256)
                0xE380_08C0, // orr r0, r0, #0xC00000 (control = start|IRQ, in the high half)
                0xE581_0100, // str r0, [r1, #0x100]  ; TM0CNT_L + TM0CNT_H
                0xEAFF_FFFE, // b .
            ],
        );
        sys.cpu.set_pc(0x0300_0000);
        sys.run_until(1000);

        let text = sys.trace().unwrap().to_text();
        for expected in [
            "CPU write IE",
            "CPU write IME",
            "CPU write TM0CNT_L",
            "scheduled next event",
            "Timer0 overflow",
            "CPU accepts IRQ",
        ] {
            assert!(text.contains(expected), "trace missing {expected:?}:\n{text}");
        }
    }

    #[test]
    fn ppu_scanline_timing_and_hblank_flag() {
        let mut sys = System::new();
        sys.start_lcd();

        // Line 0, before HBlank (which begins at cycle 1006).
        sys.advance_time(1005);
        assert_eq!(sys.gba.bus.io.video.vcount(), 0);
        assert!(!sys.gba.bus.io.video.hblank_flag());

        // HBlank flag is raised at 1006.
        sys.advance_time(1006);
        assert!(sys.gba.bus.io.video.hblank_flag());

        // The next line starts at 1232: VCOUNT advances, HBlank flag clears.
        sys.advance_time(1232);
        assert_eq!(sys.gba.bus.io.video.vcount(), 1);
        assert!(!sys.gba.bus.io.video.hblank_flag());

        // VCOUNT tracks elapsed lines.
        sys.advance_time(100 * 1232);
        assert_eq!(sys.gba.bus.io.video.vcount(), 100);
    }

    #[test]
    fn vblank_sets_flag_and_requests_irq() {
        let mut sys = System::new();
        sys.gba.bus.io.irq.set_ie(IrqSource::VBlank.mask());
        sys.gba.bus.io.irq.set_ime(true);
        sys.gba.bus.io.video.write_dispstat(1 << 3); // VBlank IRQ enable
        sys.start_lcd();

        // Just before VBlank (line 160 starts at 160 * 1232).
        sys.advance_time(159 * 1232);
        assert!(!sys.gba.bus.io.video.vblank_flag());
        assert_eq!(sys.gba.bus.io.irq.iflags() & IrqSource::VBlank.mask(), 0);

        // Entering line 160 raises the flag and the interrupt.
        sys.advance_time(160 * 1232);
        assert_eq!(sys.gba.bus.io.video.vcount(), 160);
        assert!(sys.gba.bus.io.video.vblank_flag());
        assert!(sys.gba.bus.io.irq.line_asserted());
    }

    #[test]
    fn vcount_match_requests_irq_on_the_selected_line() {
        let mut sys = System::new();
        sys.gba.bus.io.irq.set_ie(IrqSource::VCounterMatch.mask());
        sys.gba.bus.io.irq.set_ime(true);
        // LYC = 100, V-counter IRQ enabled.
        sys.gba.bus.io.video.write_dispstat((100 << 8) | (1 << 5));
        sys.start_lcd();

        sys.advance_time(99 * 1232);
        assert!(!sys.gba.bus.io.video.vcount_match());
        assert_eq!(sys.gba.bus.io.irq.iflags() & IrqSource::VCounterMatch.mask(), 0);

        sys.advance_time(100 * 1232);
        assert!(sys.gba.bus.io.video.vcount_match());
        assert_ne!(sys.gba.bus.io.irq.iflags() & IrqSource::VCounterMatch.mask(), 0);
    }

    #[test]
    fn hblank_irq_fires_every_scanline_including_vblank() {
        let mut sys = System::new();
        sys.gba.bus.io.irq.set_ie(IrqSource::HBlank.mask());
        sys.gba.bus.io.video.write_dispstat(1 << 4); // HBlank IRQ enable
        sys.start_lcd();

        // Line 0 HBlank.
        sys.advance_time(1006);
        assert_ne!(sys.gba.bus.io.irq.iflags() & IrqSource::HBlank.mask(), 0);
        sys.gba.bus.io.irq.acknowledge(IrqSource::HBlank.mask());

        // A VBlank scanline (line 200) still produces an HBlank interrupt.
        sys.advance_time(200 * 1232 + 1006);
        assert_eq!(sys.gba.bus.io.video.vcount(), 200);
        assert_ne!(sys.gba.bus.io.irq.iflags() & IrqSource::HBlank.mask(), 0);
    }

    #[test]
    fn hblank_dma_fires_on_a_visible_scanline() {
        use emu_core::Access;
        let mut sys = System::new();
        let cpu = Access::cpu_data();
        let bus = &mut sys.gba.bus;
        let sched = &mut sys.scheduler;

        bus.write16(0x0200_0000, 0xABCD, cpu, sched); // source in EWRAM
        bus.write32(0x0400_00B0, 0x0200_0000, cpu, sched); // SAD
        bus.write32(0x0400_00B4, 0x0300_0000, cpu, sched); // DAD -> IWRAM
        bus.write16(0x0400_00B8, 1, cpu, sched); // one unit
        // Enable, 16-bit, HBlank timing (2<<12), repeat so it stays armed.
        bus.write16(0x0400_00BA, (1 << 15) | (1 << 9) | (2 << 12), cpu, sched);

        sys.start_lcd();
        // Nothing transferred yet (HBlank timing, not immediate).
        assert_eq!(
            sys.gba.bus.read16(0x0300_0000, cpu, &mut sys.scheduler).value,
            0
        );

        // Line 0's HBlank at cycle 1006 triggers the transfer.
        sys.advance_time(1006);
        assert_eq!(
            sys.gba.bus.read16(0x0300_0000, cpu, &mut sys.scheduler).value,
            0xABCD
        );
    }

    #[test]
    fn video_memory_contention_tracks_the_ppu() {
        use emu_core::Access;
        let mut sys = System::new();
        let cpu = Access::cpu_data();
        sys.start_lcd();

        // Line 0, actively drawing: a VRAM halfword read pays +1 contention.
        assert_eq!(
            sys.gba.bus.read16(0x0600_0000, cpu, &mut sys.scheduler).cycles,
            2
        );

        // During HBlank the PPU isn't fetching pixels: no penalty.
        sys.advance_time(1006);
        assert_eq!(
            sys.gba.bus.read16(0x0600_0000, cpu, &mut sys.scheduler).cycles,
            1
        );

        // Back to a visible drawing phase, but force-blank the display: the PPU
        // releases video memory, so access is full speed again.
        sys.advance_time(2 * 1232);
        sys.gba
            .bus
            .write16(0x0400_0000, 1 << 7, cpu, &mut sys.scheduler); // DISPCNT force blank
        assert_eq!(
            sys.gba.bus.read16(0x0600_0000, cpu, &mut sys.scheduler).cycles,
            1
        );
    }

    #[test]
    fn frame_renders_mode3_through_scanline_timing() {
        use crate::ppu::Color15;
        use emu_core::Access;
        let mut sys = System::new();
        let cpu = Access::cpu_data();

        // Mode 3 with BG2 enabled, via the real MMIO path.
        sys.gba
            .bus
            .write16(0x0400_0000, 0x0003 | (1 << 10), cpu, &mut sys.scheduler);
        // Pixel (0, 2) = blue (0x7C00): byte offset (2*240 + 0) * 2 = 960.
        let off = (2 * 240) * 2;
        sys.gba.bus.memory.vram[off..off + 2].copy_from_slice(&0x7C00u16.to_le_bytes());

        sys.start_lcd();
        // Advance through line 2's HBlank (2*1232 + 1006), where line 2 is drawn.
        sys.advance_time(2 * 1232 + 1006);

        // The timing-driven render wrote the pixel into the framebuffer.
        assert_eq!(sys.framebuffer()[2 * 240], Color15(0x7C00));
        // And the debug API explains it back to its VRAM source.
        let explanation = sys.explain_pixel(0, 2).unwrap();
        assert_eq!(explanation.final_color, Color15(0x7C00));
        assert_eq!(explanation.video_mode, 3);
    }

    /// Replays a real BIOS well past the intro (the point where an overshot-then-
    /// halted deadline used to move time backward). Gated on `REWINDS_BIOS`, so it
    /// is skipped unless a BIOS path is supplied.
    #[test]
    fn bios_runs_many_frames_without_moving_time_backward() {
        let Ok(path) = std::env::var("REWINDS_BIOS") else {
            return;
        };
        let bios = std::fs::read(path).expect("read BIOS");
        let mut sys = System::new();
        sys.gba.bus.load_bios(&bios);
        sys.cpu.set_pc(0);
        // Well past the intro handoff (~172 frames); this used to panic.
        for _ in 0..300 {
            sys.run_frame();
        }
    }

    /// Diagnostic: boot a BIOS + ROM and report boot progress — handoff to ROM,
    /// the first frame the game enables a background, and whether the framebuffer
    /// gets content. Gated on `REWINDS_BIOS` + `REWINDS_ROM`; run with `--nocapture`.
    #[test]
    fn diagnose_rom_boot() {
        let (Ok(bios), Ok(rom)) = (std::env::var("REWINDS_BIOS"), std::env::var("REWINDS_ROM"))
        else {
            return;
        };
        let mut sys = System::new();
        sys.gba.bus.load_bios(&std::fs::read(bios).expect("bios"));
        sys.gba.bus.load_rom(std::fs::read(rom).expect("rom"));
        sys.cpu.set_pc(0);

        let mut handoff = None;
        let mut display_on = None;
        let mut fb_content = None;
        for frame in 0..800u32 {
            sys.run_frame();
            if handoff.is_none() && sys.cpu.register(15) >> 24 >= 0x08 {
                handoff = Some(frame);
            }
            // Only look for game output *after* the BIOS hands off (the BIOS itself
            // draws the logo, which we don't want to count as the game booting).
            if handoff.is_none() {
                continue;
            }
            let dispcnt = sys.gba.bus.io.video.read_dispcnt();
            if display_on.is_none() && dispcnt & 0x0F00 != 0 && dispcnt & 0x80 == 0 {
                display_on = Some((frame, dispcnt));
            }
            if fb_content.is_none() {
                let fb = sys.framebuffer();
                if fb.iter().any(|&c| c != fb[0]) {
                    fb_content = Some(frame);
                }
            }
        }
        eprintln!("handoff to ROM at frame: {handoff:?}");
        eprintln!("first background enabled at frame: {display_on:?}");
        eprintln!("framebuffer gained content at frame: {fb_content:?}");
        eprintln!(
            "final pc={:#010x} (was stuck at 0x080008c0 before SIO)",
            sys.cpu.register(15)
        );
    }

    #[test]
    fn run_frame_advances_one_frame_and_draws() {
        use crate::ppu::Color15;
        use emu_core::Access;
        let mut sys = System::new();
        // Mode 3, BG2, a green pixel. No program: the CPU streams NOPs (opcode 0
        // is a condition-failed ANDEQ), which is enough to advance the timeline.
        sys.gba
            .bus
            .write16(0x0400_0000, 0x0003 | (1 << 10), Access::cpu_data(), &mut sys.scheduler);
        sys.gba.bus.memory.vram[0..2].copy_from_slice(&0x03E0u16.to_le_bytes());

        let before = sys.gba.bus.io.video.frame();
        sys.run_frame();
        assert_eq!(sys.gba.bus.io.video.frame(), before + 1);
        assert_eq!(sys.framebuffer()[0], Color15(0x03E0));
    }

    #[test]
    fn midscanline_register_write_splits_the_line() {
        use crate::ppu::Color15;
        use emu_core::Access;
        let mut sys = System::new();
        let cpu = Access::cpu_data();

        // Mode 3, BG2 on; fill line 0 with green and set a blue backdrop.
        sys.gba
            .bus
            .write16(0x0400_0000, 0x0003 | (1 << 10), cpu, &mut sys.scheduler);
        for x in 0..240 {
            let off = x * 2;
            sys.gba.bus.memory.vram[off..off + 2].copy_from_slice(&0x03E0u16.to_le_bytes());
        }
        sys.gba.bus.memory.palette[0..2].copy_from_slice(&0x7C00u16.to_le_bytes());
        sys.start_lcd();

        // Partway through line 0's draw (pixel 120 = cycle 480), force-blank the
        // display via a DISPCNT write.
        sys.advance_time(480);
        sys.gba.bus.write16(
            0x0400_0000,
            0x0003 | (1 << 10) | (1 << 7),
            cpu,
            &mut sys.scheduler,
        );
        // Render line 0 at its HBlank.
        sys.advance_time(1006);

        // Left of the split still shows BG2; right of it is force-blanked, which the
        // hardware drives white (0x7FFF) — not the backdrop.
        assert_eq!(sys.framebuffer()[0], Color15(0x03E0));
        assert_eq!(sys.framebuffer()[119], Color15(0x03E0));
        assert_eq!(sys.framebuffer()[120], Color15(0x7FFF));
        assert_eq!(sys.framebuffer()[239], Color15(0x7FFF));
    }

    #[test]
    fn stop_wakes_only_on_stop_sources() {
        let mut sys = System::new();
        sys.gba.bus.io.irq.set_ie(0xFFFF);
        sys.gba.write_haltcnt(0x80); // Stop
        assert_eq!(sys.gba.power_state(), crate::io::PowerState::Stopped);

        // A timer interrupt does not terminate Stop mode.
        sys.gba.bus.io.irq.request(IrqSource::Timer0);
        assert!(!sys.gba.should_wake());

        // A keypad interrupt does.
        sys.gba.bus.io.irq.request(IrqSource::Keypad);
        assert!(sys.gba.should_wake());
    }

    /// Throughput of the unlinked frame path (the single-player common case), to check the
    /// transfer-barrier check added to `run_until` doesn't regress it. Ignored by default;
    /// run with `cargo test -p gba --release bench_unlinked -- --ignored --nocapture`.
    #[test]
    #[ignore = "perf benchmark"]
    fn bench_unlinked_run_frame_throughput() {
        let mut sys = System::new();
        // add r0,r0,#1 ; add r1,r1,r0 ; b . — a representative ALU + branch mix.
        load_iwram(&mut sys, &[0xE280_0001, 0xE081_1000, 0xEAFF_FFFC]);
        sys.cpu.set_pc(0x0300_0000);
        for _ in 0..120 {
            sys.run_frame_step(); // warm up
        }
        let n = 1200;
        let start = std::time::Instant::now();
        for _ in 0..n {
            sys.run_frame_step();
        }
        let elapsed = start.elapsed();
        eprintln!(
            "unlinked run_frame_step: {n} frames in {elapsed:?} = {:.2} us/frame, {:.0} frames/s",
            elapsed.as_micros() as f64 / n as f64,
            n as f64 / elapsed.as_secs_f64()
        );
    }

    /// The transfer barrier: a connected multiplayer transfer suspends `run_frame_step`
    /// (`LinkPending`) until the peer's word is delivered, then the frame resumes and
    /// completes — the synchronous-lockstep primitive Phase A adds. Two in-process
    /// machines relay each other's frames directly (a no-op "carrier").
    #[test]
    fn link_transfer_barrier_suspends_and_resumes_the_frame() {
        use emu_core::{Access, AccessKind, AccessSequence};
        const CPU: Access = Access::cpu(AccessKind::Data, AccessSequence::NonSequential);
        const MULTI: u16 = (0b10 << 12) | (1 << 14); // multiplayer mode + transfer IRQ
        const START: u16 = 1 << 7;

        // Two machines each spinning on `b .`, wired as a two-unit multiplayer link.
        let mut parent = System::new();
        let mut child = System::new();
        for sys in [&mut parent, &mut child] {
            load_iwram(sys, &[0xEAFF_FFFE]); // b .
            sys.cpu.set_pc(0x0300_0000);
        }
        parent.gba.bus.io.serial_set_link(true, 0, 2);
        child.gba.bus.io.serial_set_link(true, 1, 2);

        // Serial/multiplayer mode and each unit's send word, via MMIO.
        parent.gba.bus.write16(0x0400_0134, 0, CPU, &mut parent.scheduler);
        child.gba.bus.write16(0x0400_0134, 0, CPU, &mut child.scheduler);
        parent.gba.bus.write16(0x0400_012A, 0xAAAA, CPU, &mut parent.scheduler);
        child.gba.bus.write16(0x0400_012A, 0x5555, CPU, &mut child.scheduler);
        child.gba.bus.write16(0x0400_0128, MULTI, CPU, &mut child.scheduler); // child ready

        // The parent clocks a transfer; the frame suspends awaiting the peer, and stays
        // suspended across calls until something is delivered.
        parent.gba.bus.write16(0x0400_0128, MULTI | START, CPU, &mut parent.scheduler);
        assert_eq!(parent.run_frame_step(), FrameOutcome::LinkPending);
        assert_eq!(parent.run_frame_step(), FrameOutcome::LinkPending);

        // Relay the barrier frames between the two machines (the in-process carrier).
        let pf = parent.gba.bus.io.serial_poll_out().expect("parent queued an outbound frame");
        child.gba.bus.io.serial_deliver(&pf);
        let cf = child.gba.bus.io.serial_poll_out().expect("child replied");
        parent.gba.bus.io.serial_deliver(&cf);

        // The peer's word is in, so the barrier lifts and the frame runs to completion.
        assert_eq!(parent.run_frame_step(), FrameOutcome::Completed);

        // Both machines hold the exchanged words, cleared busy, and took the serial IRQ.
        for sys in [&mut parent, &mut child] {
            assert_eq!(sys.gba.bus.read16(0x0400_0120, CPU, &mut sys.scheduler).value, 0xAAAA);
            assert_eq!(sys.gba.bus.read16(0x0400_0122, CPU, &mut sys.scheduler).value, 0x5555);
            assert_eq!(sys.gba.bus.read16(0x0400_0128, CPU, &mut sys.scheduler).value & START, 0);
            assert_ne!(sys.gba.bus.io.irq.iflags() & IrqSource::Serial.mask(), 0);
        }
    }
}
