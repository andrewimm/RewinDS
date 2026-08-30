//! The GBA machine: the memory bus (storage plus MMIO devices) that shares the
//! guest timeline.
//!
//! The scheduler is owned outside the machine (by [`crate::system::System`]) so
//! that a device can schedule follow-up events through the event context, and so
//! the CPU can thread it into bus writes, without a self-borrow.

use crate::bus::Bus;
use crate::dma::DmaTiming;
use crate::event::{EventKind, PpuEvent};
use crate::io::PowerState;
use emu_core::{EventContext, EventHandler};

/// The first scanline of the vertical blank.
const VBLANK_LINE: u16 = 160;

/// The GBA machine state: everything reachable through the bus.
#[derive(Clone, Debug, Default)]
pub struct Gba {
    pub bus: Bus,
}

impl Gba {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn power_state(&self) -> PowerState {
        self.bus.io.power_state()
    }

    pub fn is_running(&self) -> bool {
        self.bus.io.power_state() == PowerState::Running
    }

    pub fn is_low_power(&self) -> bool {
        self.bus.io.is_low_power()
    }

    pub fn should_wake(&self) -> bool {
        self.bus.io.should_wake()
    }

    pub fn wake(&mut self) {
        self.bus.io.wake();
    }

    /// Write `HALTCNT` directly (a convenience mirror of the MMIO path).
    pub fn write_haltcnt(&mut self, value: u8) {
        self.bus.io.control.write_haltcnt(value);
    }
}

impl EventHandler<EventKind> for Gba {
    fn handle(&mut self, event: EventKind, ctx: &mut EventContext<'_, EventKind>) {
        match event {
            EventKind::Timer(event) => {
                let io = &mut self.bus.io;
                io.timers.handle_overflow(event, &mut io.irq, ctx);
            }
            EventKind::Ppu(event) => {
                {
                    let io = &mut self.bus.io;
                    io.video.handle_event(event, &mut io.irq, ctx);
                }
                // A blank transition can trigger DMA. HBlank DMA fires only on
                // visible scanlines; VBlank DMA fires as line 160 begins.
                match event {
                    PpuEvent::HBlank if self.bus.io.video.vcount() < VBLANK_LINE => {
                        self.bus.trigger_dma(DmaTiming::HBlank, ctx.scheduler);
                    }
                    PpuEvent::LineStart if self.bus.io.video.vcount() == VBLANK_LINE => {
                        self.bus.trigger_dma(DmaTiming::VBlank, ctx.scheduler);
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interrupt::IrqSource;
    use crate::timer::TimerId;
    use emu_core::{Scheduler, Timestamp};

    /// Drive the scheduler like the machine loop, up to and including `limit`.
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
        gba.bus.io.irq.set_ie(IrqSource::Timer0.mask());
        gba.bus.io.irq.set_ime(true);

        // Overflow every 10 cycles (reload 0xFFF6), prescaler F/1, IRQ on.
        gba.bus.io.timers.write_reload(TimerId::Timer0, 0xFFF6);
        gba.bus
            .io
            .timers
            .write_control(TimerId::Timer0, (1 << 7) | (1 << 6), 0, &mut sched);
        assert_eq!(sched.next_deadline(), Some(10));

        run_until(&mut sched, &mut gba, 10);
        assert!(gba.bus.io.irq.line_asserted());
        assert_eq!(gba.bus.io.irq.iflags(), IrqSource::Timer0.mask());
        assert_eq!(gba.bus.io.timers.read_counter(TimerId::Timer0, 10), 0xFFF6);
        assert_eq!(sched.next_deadline(), Some(20));
    }

    #[test]
    fn disabling_invalidates_the_pending_overflow() {
        let mut sched = Scheduler::new();
        let mut gba = Gba::new();
        gba.bus.io.irq.set_ie(0xFFFF);
        gba.bus.io.irq.set_ime(true);

        gba.bus.io.timers.write_reload(TimerId::Timer0, 0xFF00); // overflow at 256
        gba.bus
            .io
            .timers
            .write_control(TimerId::Timer0, (1 << 7) | (1 << 6), 0, &mut sched);

        // Stop the timer partway; its generation bumps, so the queued overflow
        // at 256 is now stale.
        sched.set_now(100);
        gba.bus
            .io
            .timers
            .write_control(TimerId::Timer0, 0, 100, &mut sched);

        run_until(&mut sched, &mut gba, 1000);
        assert!(!gba.bus.io.irq.pending());
    }

    #[test]
    fn cascade_overflow_propagates_at_one_instant() {
        let mut sched = Scheduler::new();
        let mut gba = Gba::new();
        gba.bus.io.irq.set_ie(0xFFFF);
        gba.bus.io.irq.set_ime(true);

        gba.bus.io.timers.write_reload(TimerId::Timer0, 0xFFFC); // overflow every 4
        gba.bus
            .io
            .timers
            .write_control(TimerId::Timer0, 1 << 7, 0, &mut sched);
        gba.bus.io.timers.write_reload(TimerId::Timer1, 0xFFFE);
        gba.bus.io.timers.write_control(
            TimerId::Timer1,
            (1 << 7) | (1 << 2) | (1 << 6),
            0,
            &mut sched,
        );

        run_until(&mut sched, &mut gba, 4);
        assert_eq!(gba.bus.io.timers.read_counter(TimerId::Timer1, 4), 0xFFFF);
        assert!(!gba.bus.io.irq.pending());

        run_until(&mut sched, &mut gba, 8);
        assert_eq!(gba.bus.io.timers.read_counter(TimerId::Timer1, 8), 0xFFFE);
        assert_ne!(gba.bus.io.irq.iflags() & IrqSource::Timer1.mask(), 0);
    }
}
