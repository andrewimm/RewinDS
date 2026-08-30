//! The GBA's four 16-bit timers.
//!
//! Timers are modeled lazily, as the scheduler design requires: a running timer
//! does not tick every cycle. It stores its counter at the last synchronization
//! point and the prescaler, and derives the current count on demand from the
//! elapsed guest time. Because an overflow is externally observable (it can
//! reload, raise an IRQ, and cascade), each running timer schedules its next
//! overflow as an event.
//!
//! Reconfiguration uses generation-based invalidation: rewriting a timer bumps
//! its generation, so any overflow event already queued under the old
//! generation is ignored when it fires. The queue is never scanned.
//!
//! In count-up (cascade) mode a timer is not driven by the prescaler at all; it
//! increments once each time the timer below it overflows. That increment — and
//! any resulting overflow, IRQ, and further cascade — happens synchronously at
//! the overflowing timer's timestamp, keeping the whole cascade deterministic
//! within one instant.

use crate::event::{EventKind, TimerEvent};
use crate::interrupt::{InterruptController, IrqSource};
use emu_core::{EventContext, Scheduler, Timestamp};

/// Which of the four timers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TimerId {
    Timer0,
    Timer1,
    Timer2,
    Timer3,
}

impl TimerId {
    pub fn index(self) -> usize {
        self as usize
    }

    fn from_index(index: usize) -> TimerId {
        match index {
            0 => TimerId::Timer0,
            1 => TimerId::Timer1,
            2 => TimerId::Timer2,
            3 => TimerId::Timer3,
            _ => unreachable!("timer index out of range: {index}"),
        }
    }

    /// The interrupt source this timer's overflow requests.
    fn irq_source(self) -> IrqSource {
        match self {
            TimerId::Timer0 => IrqSource::Timer0,
            TimerId::Timer1 => IrqSource::Timer1,
            TimerId::Timer2 => IrqSource::Timer2,
            TimerId::Timer3 => IrqSource::Timer3,
        }
    }
}

/// The prescaler divisors F/{1,64,256,1024} as right-shift amounts, indexed by
/// the 2-bit control field. All are powers of two, so tick counts are exact
/// shifts.
const PRESCALER_SHIFTS: [u32; 4] = [0, 6, 8, 10];

/// The counter wraps after reaching this value.
const OVERFLOW: u32 = 0x1_0000;

/// One timer's state.
#[derive(Clone, Copy, Debug)]
struct Timer {
    id: TimerId,
    /// Value written to `TMxCNT_L`; copied into the counter on overflow or on a
    /// 0→1 start.
    reload: u16,
    /// Counter value at `last_sync`. For a running prescaler timer the live
    /// value is derived from this; for a cascade or stopped timer this *is* the
    /// live value.
    counter: u16,
    /// Guest time at which `counter` was last exact.
    last_sync: Timestamp,
    prescaler_shift: u32,
    enabled: bool,
    irq_enabled: bool,
    /// Count-up mode: driven by the timer below rather than the prescaler.
    /// Always false for Timer 0.
    cascade: bool,
    generation: u32,
}

impl Timer {
    fn new(id: TimerId) -> Self {
        Timer {
            id,
            reload: 0,
            counter: 0,
            last_sync: 0,
            prescaler_shift: 0,
            enabled: false,
            irq_enabled: false,
            cascade: false,
            generation: 0,
        }
    }

    /// Whether this timer's count advances with guest time (running under the
    /// prescaler) rather than being driven by cascades or frozen.
    fn free_running(&self) -> bool {
        self.enabled && !self.cascade
    }

    /// The live counter value at `now`.
    fn live_counter(&self, now: Timestamp) -> u16 {
        if self.free_running() {
            let elapsed = now.saturating_sub(self.last_sync);
            let ticks = (elapsed >> self.prescaler_shift) as u16;
            self.counter.wrapping_add(ticks)
        } else {
            self.counter
        }
    }

    /// Queue this timer's next overflow, based on its current counter and sync
    /// point. Only meaningful for a free-running timer.
    fn schedule_overflow(&self, scheduler: &mut Scheduler<EventKind>) {
        let ticks_to_overflow = OVERFLOW - self.counter as u32;
        let delta = (ticks_to_overflow as u64) << self.prescaler_shift;
        let at = self.last_sync + delta;
        scheduler.schedule_at(
            at,
            EventKind::Timer(TimerEvent::Overflow {
                timer: self.id,
                generation: self.generation,
            }),
        );
    }
}

/// The bank of four timers.
#[derive(Clone, Copy, Debug)]
pub struct Timers {
    timers: [Timer; 4],
}

impl Default for Timers {
    fn default() -> Self {
        Self::new()
    }
}

impl Timers {
    pub fn new() -> Self {
        Timers {
            timers: [
                Timer::new(TimerId::Timer0),
                Timer::new(TimerId::Timer1),
                Timer::new(TimerId::Timer2),
                Timer::new(TimerId::Timer3),
            ],
        }
    }

    /// Write `TMxCNT_L`: set the reload value. Per hardware, this does not
    /// affect the currently running counter.
    pub fn write_reload(&mut self, id: TimerId, value: u16) {
        self.timers[id.index()].reload = value;
    }

    /// Read `TMxCNT_L`: the current counter value at `now`.
    pub fn read_counter(&self, id: TimerId, now: Timestamp) -> u16 {
        self.timers[id.index()].live_counter(now)
    }

    /// Write `TMxCNT_H`: prescaler (bits 0-1), count-up (bit 2, not on Timer 0),
    /// IRQ enable (bit 6), start (bit 7). Reconfiguring bumps the generation to
    /// invalidate any queued overflow, then reschedules if the timer is now
    /// free-running.
    pub fn write_control(
        &mut self,
        id: TimerId,
        value: u16,
        now: Timestamp,
        scheduler: &mut Scheduler<EventKind>,
    ) {
        let timer = &mut self.timers[id.index()];
        let was_enabled = timer.enabled;

        // Freeze the live count before changing timing parameters.
        if timer.free_running() {
            timer.counter = timer.live_counter(now);
            timer.last_sync = now;
        }

        timer.prescaler_shift = PRESCALER_SHIFTS[(value & 0b11) as usize];
        timer.irq_enabled = value & (1 << 6) != 0;
        timer.cascade = id != TimerId::Timer0 && value & (1 << 2) != 0;
        timer.enabled = value & (1 << 7) != 0;

        // A 0→1 start reloads the counter.
        if timer.enabled && !was_enabled {
            timer.counter = timer.reload;
            timer.last_sync = now;
        }

        // Invalidate any pending overflow, then reschedule if applicable.
        timer.generation = timer.generation.wrapping_add(1);
        if timer.free_running() {
            timer.schedule_overflow(scheduler);
        }
    }

    /// Handle a scheduled timer overflow: reload, request an IRQ, reschedule the
    /// next overflow, and drive any cascade — all at the event's timestamp.
    pub fn handle_overflow(
        &mut self,
        event: TimerEvent,
        irq: &mut InterruptController,
        ctx: &mut EventContext<'_, EventKind>,
    ) {
        let TimerEvent::Overflow { timer, generation } = event;
        let index = timer.index();

        // Ignore an event left stale by a reconfiguration.
        if self.timers[index].generation != generation {
            return;
        }

        {
            let t = &mut self.timers[index];
            t.counter = t.reload;
            t.last_sync = ctx.now;
            if t.irq_enabled {
                irq.request(timer.irq_source());
            }
            if t.free_running() {
                t.schedule_overflow(ctx.scheduler);
            }
        }

        self.cascade_from(index, irq);
    }

    /// Propagate a cascade: increment the next timer if it is in count-up mode,
    /// and continue up the chain for as long as each increment overflows. All of
    /// this happens at the originating overflow's instant.
    fn cascade_from(&mut self, source: usize, irq: &mut InterruptController) {
        let mut source = source;
        while source + 1 < self.timers.len() {
            let next = source + 1;
            let t = &mut self.timers[next];
            if !(t.enabled && t.cascade) {
                break;
            }
            let (incremented, overflowed) = t.counter.overflowing_add(1);
            if !overflowed {
                t.counter = incremented;
                break;
            }
            t.counter = t.reload;
            if t.irq_enabled {
                irq.request(TimerId::from_index(next).irq_source());
            }
            source = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Control-register field helpers for readable tests.
    const PRESCALE_1: u16 = 0b00;
    const START: u16 = 1 << 7;

    #[test]
    fn live_counter_derives_from_elapsed_time() {
        let mut timers = Timers::new();
        let mut sched: Scheduler<EventKind> = Scheduler::new();
        // reload 0xFFF0, prescaler F/1, running: counts up one per cycle.
        timers.write_reload(TimerId::Timer0, 0xFFF0);
        timers.write_control(TimerId::Timer0, START | PRESCALE_1, 0, &mut sched);
        assert_eq!(timers.read_counter(TimerId::Timer0, 0), 0xFFF0);
        assert_eq!(timers.read_counter(TimerId::Timer0, 5), 0xFFF5);
        // Next overflow is 16 cycles after start (0x10000 - 0xFFF0).
        assert_eq!(sched.next_deadline(), Some(16));
    }

    #[test]
    fn prescaler_scales_the_rate() {
        let mut timers = Timers::new();
        let mut sched: Scheduler<EventKind> = Scheduler::new();
        timers.write_reload(TimerId::Timer0, 0);
        // Prescaler F/256 (shift 8): one increment per 256 cycles.
        timers.write_control(TimerId::Timer0, START | 0b10, 0, &mut sched);
        assert_eq!(timers.read_counter(TimerId::Timer0, 255), 0);
        assert_eq!(timers.read_counter(TimerId::Timer0, 256), 1);
        assert_eq!(timers.read_counter(TimerId::Timer0, 512), 2);
        // 0x10000 increments * 256 cycles until overflow.
        assert_eq!(sched.next_deadline(), Some(0x10000 * 256));
    }
}
