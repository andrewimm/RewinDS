//! The GBA machine: the collection of devices that share the guest timeline and
//! together handle scheduled events.
//!
//! The scheduler itself is owned outside the machine (by the top-level driver),
//! matching the execution loop where `scheduler.run_due_events(&mut machine)`
//! dispatches into the devices. Keeping the two separate avoids a self-borrow
//! when a device schedules a follow-up event through the context.

use crate::event::EventKind;
use crate::interrupt::InterruptController;
use crate::timer::Timers;
use emu_core::{EventContext, EventHandler};

/// The GBA's devices, sharing one timeline.
#[derive(Clone, Copy, Debug, Default)]
pub struct Gba {
    pub timers: Timers,
    pub irq: InterruptController,
}

impl Gba {
    pub fn new() -> Self {
        Self::default()
    }
}

impl EventHandler<EventKind> for Gba {
    fn handle(&mut self, event: EventKind, ctx: &mut EventContext<'_, EventKind>) {
        match event {
            EventKind::Timer(event) => self.timers.handle_overflow(event, &mut self.irq, ctx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interrupt::IrqSource;
    use crate::timer::TimerId;
    use emu_core::{Scheduler, Timestamp};

    /// Advance the scheduler like the machine loop, up to and including `limit`.
    fn run_until(scheduler: &mut Scheduler<EventKind>, gba: &mut Gba, limit: Timestamp) {
        while let Some(deadline) = scheduler.next_deadline() {
            if deadline > limit {
                break;
            }
            scheduler.set_now(deadline);
            scheduler.run_due_events(gba);
        }
    }

    #[test]
    fn timer_overflow_requests_irq_and_reloads() {
        let mut sched = Scheduler::new();
        let mut gba = Gba::new();
        gba.irq.set_ie(IrqSource::Timer0.mask());
        gba.irq.set_ime(true);

        // Overflow every 10 cycles (reload 0xFFF6), prescaler F/1, IRQ on.
        gba.timers.write_reload(TimerId::Timer0, 0xFFF6);
        gba.timers
            .write_control(TimerId::Timer0, (1 << 7) | (1 << 6), 0, &mut sched);
        assert_eq!(sched.next_deadline(), Some(10));

        run_until(&mut sched, &mut gba, 10);
        // The overflow fired: IRQ requested and the line is asserted.
        assert!(gba.irq.line_asserted());
        assert_eq!(gba.irq.iflags(), IrqSource::Timer0.mask());
        // Counter reloaded and a fresh overflow is queued 10 cycles later.
        assert_eq!(gba.timers.read_counter(TimerId::Timer0, 10), 0xFFF6);
        assert_eq!(sched.next_deadline(), Some(20));
    }

    #[test]
    fn disabling_invalidates_the_pending_overflow() {
        let mut sched = Scheduler::new();
        let mut gba = Gba::new();
        gba.irq.set_ie(0xFFFF);
        gba.irq.set_ime(true);

        gba.timers.write_reload(TimerId::Timer0, 0xFF00); // overflow at 256
        gba.timers
            .write_control(TimerId::Timer0, (1 << 7) | (1 << 6), 0, &mut sched);
        assert_eq!(sched.next_deadline(), Some(256));

        // Stop the timer partway. This bumps the generation, so the queued
        // overflow at 256 is now stale.
        sched.set_now(100);
        gba.timers.write_control(TimerId::Timer0, 0, 100, &mut sched);

        run_until(&mut sched, &mut gba, 1000);
        // The stale event fired but was ignored: no IRQ, no reload.
        assert!(!gba.irq.pending());
    }

    #[test]
    fn cascade_overflow_propagates_at_one_instant() {
        let mut sched = Scheduler::new();
        let mut gba = Gba::new();
        gba.irq.set_ie(0xFFFF);
        gba.irq.set_ime(true);

        // Timer0: prescaler F/1, overflow every 4 cycles.
        gba.timers.write_reload(TimerId::Timer0, 0xFFFC);
        gba.timers
            .write_control(TimerId::Timer0, 1 << 7, 0, &mut sched);
        // Timer1: cascade, IRQ on, overflows after two increments (two Timer0
        // overflows), i.e. reload 0xFFFE.
        gba.timers.write_reload(TimerId::Timer1, 0xFFFE);
        gba.timers
            .write_control(TimerId::Timer1, (1 << 7) | (1 << 2) | (1 << 6), 0, &mut sched);

        // First Timer0 overflow at t=4: Timer1 -> 0xFFFF, no IRQ yet.
        run_until(&mut sched, &mut gba, 4);
        assert_eq!(gba.timers.read_counter(TimerId::Timer1, 4), 0xFFFF);
        assert!(!gba.irq.pending());

        // Second Timer0 overflow at t=8: Timer1 wraps -> reload, Timer1 IRQ.
        run_until(&mut sched, &mut gba, 8);
        assert_eq!(gba.timers.read_counter(TimerId::Timer1, 8), 0xFFFE);
        assert_ne!(gba.irq.iflags() & IrqSource::Timer1.mask(), 0);
    }

    #[test]
    fn cascade_timer_ignores_time_and_only_counts_overflows() {
        let mut sched = Scheduler::new();
        let mut gba = Gba::new();

        gba.timers.write_reload(TimerId::Timer0, 0xFFF0); // overflow every 16
        gba.timers
            .write_control(TimerId::Timer0, 1 << 7, 0, &mut sched);
        gba.timers.write_reload(TimerId::Timer1, 0x1234);
        gba.timers
            .write_control(TimerId::Timer1, (1 << 7) | (1 << 2), 0, &mut sched);

        // Long before Timer0 overflows, the cascade timer has not moved despite
        // elapsed time.
        assert_eq!(gba.timers.read_counter(TimerId::Timer1, 15), 0x1234);
        // Only Timer0 has a scheduled event; the cascade timer schedules none.
        assert_eq!(sched.next_deadline(), Some(16));
    }
}
