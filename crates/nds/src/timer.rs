//! Each core's four 16-bit timers.
//!
//! Timers are modelled lazily, as the scheduler design requires: a running timer
//! does not tick every cycle. It stores its counter at the last synchronization
//! point and derives the current count on demand from the elapsed master-tick
//! time. Because an overflow is externally observable (reload, IRQ, cascade), a
//! running timer schedules its next overflow as an event on the shared timeline.
//!
//! Reconfiguration uses generation-based invalidation: rewriting a timer bumps
//! its generation, so any overflow event already queued under the old generation
//! is ignored when it fires. In count-up (cascade) mode a timer is driven not by
//! the prescaler but by the timer below it overflowing — synchronously, at that
//! instant.
//!
//! Two identical banks exist (one per core); this is the shape of one, tagged
//! with its [`Core`] so its events name the right side. DS timers run at
//! 33.513982 MHz for both cores — half the master tick — so an F/1 increment
//! spans two master ticks (the GBA's prescaler shifts, plus one).

use emu_core::{EventContext, Scheduler, Timestamp};

use crate::interrupt::{Interrupts, IrqSource};
use crate::memory::Core;
use crate::system::NdsEvent;

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

    pub fn from_index(index: usize) -> TimerId {
        match index {
            0 => TimerId::Timer0,
            1 => TimerId::Timer1,
            2 => TimerId::Timer2,
            3 => TimerId::Timer3,
            _ => unreachable!("timer index out of range: {index}"),
        }
    }

    fn irq_source(self) -> IrqSource {
        match self {
            TimerId::Timer0 => IrqSource::Timer0,
            TimerId::Timer1 => IrqSource::Timer1,
            TimerId::Timer2 => IrqSource::Timer2,
            TimerId::Timer3 => IrqSource::Timer3,
        }
    }
}

/// Prescaler divisors F/{1,64,256,1024} as right-shift amounts in **master
/// ticks**: the GBA's `[0,6,8,10]` plus one, since the DS timer clock is half the
/// master rate.
const PRESCALER_SHIFTS: [u32; 4] = [1, 7, 9, 11];

/// The counter wraps after reaching this value.
const OVERFLOW: u32 = 0x1_0000;

/// One timer's state.
#[derive(Clone, Copy, Debug)]
struct Timer {
    id: TimerId,
    reload: u16,
    counter: u16,
    last_sync: Timestamp,
    prescaler_shift: u32,
    enabled: bool,
    irq_enabled: bool,
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
            prescaler_shift: PRESCALER_SHIFTS[0],
            enabled: false,
            irq_enabled: false,
            cascade: false,
            generation: 0,
        }
    }

    fn free_running(&self) -> bool {
        self.enabled && !self.cascade
    }

    fn live_counter(&self, now: Timestamp) -> u16 {
        if self.free_running() {
            let elapsed = now.saturating_sub(self.last_sync);
            let ticks = (elapsed >> self.prescaler_shift) as u16;
            self.counter.wrapping_add(ticks)
        } else {
            self.counter
        }
    }

    fn schedule_overflow(&self, core: Core, scheduler: &mut Scheduler<NdsEvent>) {
        let ticks_to_overflow = OVERFLOW - self.counter as u32;
        let delta = (ticks_to_overflow as u64) << self.prescaler_shift;
        scheduler.schedule_at(
            self.last_sync + delta,
            NdsEvent::TimerOverflow {
                core,
                timer: self.id,
                generation: self.generation,
            },
        );
    }
}

/// One core's bank of four timers.
#[derive(Clone, Copy, Debug)]
pub struct Timers {
    core: Core,
    timers: [Timer; 4],
}

impl Timers {
    pub fn new(core: Core) -> Self {
        Timers {
            core,
            timers: [
                Timer::new(TimerId::Timer0),
                Timer::new(TimerId::Timer1),
                Timer::new(TimerId::Timer2),
                Timer::new(TimerId::Timer3),
            ],
        }
    }

    /// Write `TMxCNT_L`: set the reload value (does not disturb a running count).
    pub fn write_reload(&mut self, id: TimerId, value: u16) {
        self.timers[id.index()].reload = value;
    }

    /// Read `TMxCNT_L`: the live counter at `now`.
    pub fn read_counter(&self, id: TimerId, now: Timestamp) -> u16 {
        self.timers[id.index()].live_counter(now)
    }

    /// Reconstruct `TMxCNT_H`.
    pub fn read_control(&self, id: TimerId) -> u16 {
        let t = &self.timers[id.index()];
        let prescaler = match t.prescaler_shift {
            7 => 1,
            9 => 2,
            11 => 3,
            _ => 0,
        };
        prescaler
            | ((t.cascade as u16) << 2)
            | ((t.irq_enabled as u16) << 6)
            | ((t.enabled as u16) << 7)
    }

    /// Write `TMxCNT_H`: prescaler (bits 0-1), count-up (bit 2, not on Timer 0),
    /// IRQ enable (bit 6), start (bit 7). Bumps the generation to invalidate any
    /// queued overflow, then reschedules if now free-running.
    pub fn write_control(
        &mut self,
        id: TimerId,
        value: u16,
        now: Timestamp,
        scheduler: &mut Scheduler<NdsEvent>,
    ) {
        let core = self.core;
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

        timer.generation = timer.generation.wrapping_add(1);
        if timer.free_running() {
            timer.schedule_overflow(core, scheduler);
        }
    }

    /// Service a scheduled overflow: reload, request an IRQ, reschedule, and drive
    /// any cascade — all at the event's timestamp. Stale generations are ignored.
    pub fn handle_overflow(
        &mut self,
        id: TimerId,
        generation: u32,
        irq: &mut Interrupts,
        ctx: &mut EventContext<'_, NdsEvent>,
    ) {
        let index = id.index();
        if self.timers[index].generation != generation {
            return;
        }

        let core = self.core;
        {
            let t = &mut self.timers[index];
            t.counter = t.reload;
            t.last_sync = ctx.now;
            if t.irq_enabled {
                irq.request(id.irq_source());
            }
            if t.free_running() {
                t.schedule_overflow(core, ctx.scheduler);
            }
        }
        self.cascade_from(index, irq);
    }

    /// Propagate a cascade up the chain for as long as each increment overflows.
    fn cascade_from(&mut self, source: usize, irq: &mut Interrupts) {
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

    const START: u16 = 1 << 7;

    #[test]
    fn live_counter_derives_from_elapsed_master_ticks() {
        let mut timers = Timers::new(Core::Arm9);
        let mut sched: Scheduler<NdsEvent> = Scheduler::new();
        // reload 0xFFF0, F/1: one increment per two master ticks.
        timers.write_reload(TimerId::Timer0, 0xFFF0);
        timers.write_control(TimerId::Timer0, START, 0, &mut sched);
        assert_eq!(timers.read_counter(TimerId::Timer0, 0), 0xFFF0);
        assert_eq!(timers.read_counter(TimerId::Timer0, 10), 0xFFF5); // 10 ticks / 2
                                                                      // Next overflow is (0x10000 - 0xFFF0) * 2 = 32 master ticks after start.
        assert_eq!(sched.next_deadline(), Some(32));
    }

    #[test]
    fn prescaler_scales_the_rate() {
        let mut timers = Timers::new(Core::Arm7);
        let mut sched: Scheduler<NdsEvent> = Scheduler::new();
        timers.write_reload(TimerId::Timer0, 0);
        // F/256 -> shift 9: one increment per 512 master ticks.
        timers.write_control(TimerId::Timer0, START | 0b10, 0, &mut sched);
        assert_eq!(timers.read_counter(TimerId::Timer0, 511), 0);
        assert_eq!(timers.read_counter(TimerId::Timer0, 512), 1);
        assert_eq!(sched.next_deadline(), Some(0x10000 * 512));
    }
}
