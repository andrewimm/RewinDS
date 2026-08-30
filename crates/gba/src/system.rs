//! The top-level GBA system: the guest timeline and the machine that share it.
//!
//! This owns the [`Scheduler`] alongside the [`Gba`] devices — deliberately not
//! inside the machine, so a device can schedule follow-up events through the
//! event context without a self-borrow. It will grow the CPU-driven execution
//! loop once an interpreter exists; for now it implements the HALT/wake
//! progression, which needs no CPU and is an early end-to-end scheduler test.

use crate::event::EventKind;
use crate::machine::Gba;
use emu_core::Scheduler;

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
        sys.gba.timers.write_reload(TimerId::Timer0, 0xFF00);
        sys.gba
            .timers
            .write_control(TimerId::Timer0, START | IRQ, now, &mut sys.scheduler);
    }

    #[test]
    fn halt_wakes_on_timer_irq_without_cpu_work() {
        let mut sys = System::new();
        sys.gba.irq.set_ie(IrqSource::Timer0.mask());
        sys.gba.irq.set_ime(true);
        arm_timer0(&mut sys);

        sys.gba.write_haltcnt(0x00); // Halt
        assert!(sys.gba.is_low_power());

        assert_eq!(sys.run_while_halted(), HaltProgress::Woke);
        // The timeline reached the overflow with no instruction execution.
        assert_eq!(sys.scheduler.now(), 256);
        assert!(sys.gba.is_running());
        assert!(sys.gba.irq.line_asserted());
    }

    #[test]
    fn halt_wakes_even_when_ime_is_off() {
        // Halt wakes on IE & IF regardless of IME; IME only gates acceptance.
        let mut sys = System::new();
        sys.gba.irq.set_ie(IrqSource::Timer0.mask());
        sys.gba.irq.set_ime(false);
        arm_timer0(&mut sys);
        sys.gba.write_haltcnt(0x00);

        assert_eq!(sys.run_while_halted(), HaltProgress::Woke);
        assert_eq!(sys.scheduler.now(), 256);
        assert!(sys.gba.is_running());
        // Woke, but the CPU would not accept the IRQ with IME clear.
        assert!(sys.gba.irq.pending());
        assert!(!sys.gba.irq.line_asserted());
    }

    #[test]
    fn step_reports_still_halted_when_event_does_not_wake() {
        let mut sys = System::new();
        // The overflow requests a Timer0 IRQ, but IE has it disabled, so the
        // machine stays halted.
        sys.gba.irq.set_ie(0);
        arm_timer0(&mut sys);
        sys.gba.write_haltcnt(0x00);

        assert_eq!(sys.step_halted(), HaltProgress::StillHalted);
        assert_eq!(sys.scheduler.now(), 256);
        assert!(sys.gba.is_low_power());
        // IF records the request even though it did not wake the CPU.
        assert_ne!(sys.gba.irq.iflags() & IrqSource::Timer0.mask(), 0);
        assert!(!sys.gba.irq.pending());
    }

    #[test]
    fn halt_with_no_events_deadlocks() {
        let mut sys = System::new();
        sys.gba.write_haltcnt(0x00);
        assert_eq!(sys.run_while_halted(), HaltProgress::Deadlocked);
    }

    #[test]
    fn stop_wakes_only_on_stop_sources() {
        let mut sys = System::new();
        sys.gba.irq.set_ie(0xFFFF);
        sys.gba.write_haltcnt(0x80); // Stop
        assert_eq!(sys.gba.power_state(), crate::machine::PowerState::Stopped);

        // A timer interrupt does not terminate Stop mode.
        sys.gba.irq.request(IrqSource::Timer0);
        assert!(!sys.gba.should_wake());

        // A keypad interrupt does.
        sys.gba.irq.request(IrqSource::Keypad);
        assert!(sys.gba.should_wake());
    }
}
