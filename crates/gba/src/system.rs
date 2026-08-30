//! The top-level GBA system: the guest timeline and the machine that share it.
//!
//! This owns the [`Scheduler`] alongside the [`Gba`] devices — deliberately not
//! inside the machine, so a device can schedule follow-up events through the
//! event context without a self-borrow. It will grow the CPU-driven execution
//! loop once an interpreter exists; for now it implements the HALT/wake
//! progression, which needs no CPU and is an early end-to-end scheduler test.

use crate::event::EventKind;
use crate::machine::Gba;
use emu_core::{Scheduler, Timestamp};

/// The whole GBA: one timeline, one device machine.
#[derive(Clone, Debug, Default)]
pub struct System {
    pub scheduler: Scheduler<EventKind>,
    pub gba: Gba,
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

    /// Dispatch every event due at the current time into the machine.
    pub fn run_due_events(&mut self) {
        self.scheduler.run_due_events(&mut self.gba);
    }

    /// Begin LCD timing, starting the PPU's continuous scanline schedule.
    pub fn start_lcd(&mut self) {
        let now = self.scheduler.now();
        self.gba.bus.io.video.start(now, &mut self.scheduler);
    }

    /// Advance the timeline to `target`, dispatching every event due up to it.
    ///
    /// This is the no-CPU stand-in for the execution loop: with no instructions
    /// to run, time simply advances and device events fire in order. It becomes
    /// the event-dispatch half of the real loop once the CPU can consume the
    /// cycle budget between events.
    pub fn run_until(&mut self, target: Timestamp) {
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

    #[test]
    fn ppu_scanline_timing_and_hblank_flag() {
        let mut sys = System::new();
        sys.start_lcd();

        // Line 0, before HBlank (which begins at cycle 1006).
        sys.run_until(1005);
        assert_eq!(sys.gba.bus.io.video.vcount(), 0);
        assert!(!sys.gba.bus.io.video.hblank_flag());

        // HBlank flag is raised at 1006.
        sys.run_until(1006);
        assert!(sys.gba.bus.io.video.hblank_flag());

        // The next line starts at 1232: VCOUNT advances, HBlank flag clears.
        sys.run_until(1232);
        assert_eq!(sys.gba.bus.io.video.vcount(), 1);
        assert!(!sys.gba.bus.io.video.hblank_flag());

        // VCOUNT tracks elapsed lines.
        sys.run_until(100 * 1232);
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
        sys.run_until(159 * 1232);
        assert!(!sys.gba.bus.io.video.vblank_flag());
        assert_eq!(sys.gba.bus.io.irq.iflags() & IrqSource::VBlank.mask(), 0);

        // Entering line 160 raises the flag and the interrupt.
        sys.run_until(160 * 1232);
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

        sys.run_until(99 * 1232);
        assert!(!sys.gba.bus.io.video.vcount_match());
        assert_eq!(sys.gba.bus.io.irq.iflags() & IrqSource::VCounterMatch.mask(), 0);

        sys.run_until(100 * 1232);
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
        sys.run_until(1006);
        assert_ne!(sys.gba.bus.io.irq.iflags() & IrqSource::HBlank.mask(), 0);
        sys.gba.bus.io.irq.acknowledge(IrqSource::HBlank.mask());

        // A VBlank scanline (line 200) still produces an HBlank interrupt.
        sys.run_until(200 * 1232 + 1006);
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
        sys.run_until(1006);
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
        sys.run_until(1006);
        assert_eq!(
            sys.gba.bus.read16(0x0600_0000, cpu, &mut sys.scheduler).cycles,
            1
        );

        // Back to a visible drawing phase, but force-blank the display: the PPU
        // releases video memory, so access is full speed again.
        sys.run_until(2 * 1232);
        sys.gba
            .bus
            .write16(0x0400_0000, 1 << 7, cpu, &mut sys.scheduler); // DISPCNT force blank
        assert_eq!(
            sys.gba.bus.read16(0x0600_0000, cpu, &mut sys.scheduler).cycles,
            1
        );
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
}
