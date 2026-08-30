//! The deterministic event scheduler.
//!
//! The emulator does not run hardware components concurrently; it models the
//! single guest timeline as a queue of predicted, externally observable state
//! transitions. Devices predict their next transition and schedule it as an
//! event; the CPU executes until the next event deadline; the scheduler
//! dispatches every event due at that time; the CPU resumes.
//!
//! This scheduler is deliberately machine-agnostic. It is generic over the
//! event payload `E`, so it knows *when* things happen and in *what order*, but
//! nothing about what an event means. A machine crate defines its own event
//! enum and runs `Scheduler<ThatEnum>`.
//!
//! # Invariants
//!
//! - **Single canonical time.** [`Scheduler::now`] is the one authoritative
//!   timestamp. Time only moves forward during normal execution.
//! - **Events cannot be skipped.** Time never advances past a scheduled event
//!   without that event being dispatched first (the machine loop uses
//!   [`Scheduler::next_deadline`] as the bound for CPU execution).
//! - **Deterministic same-time ordering.** Events at the same timestamp fire in
//!   insertion order, via a per-event sequence number — never host container
//!   order.
//!
//! Cancellation is intentionally *not* a queue operation. Hardware that
//! reconfigures before a pending event fires uses generation numbers carried
//! inside the event payload: the device bumps its generation, and the stale
//! event is ignored by the handler when it fires. The scheduler never scans the
//! queue, and stale events are allowed to remain in it.

use crate::time::Timestamp;
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

/// An opaque reference to a scheduled event, returned when one is scheduled.
///
/// It carries the event's unique sequence number. Because cancellation is
/// generation-based rather than queue-removal-based, this is primarily an
/// identity for inspection and future direct-slot strategies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EventHandle(pub u64);

/// An event placed on the timeline: when it fires, a tie-break sequence for
/// same-time determinism, and the machine-defined payload.
#[derive(Clone, Copy, Debug)]
pub struct ScheduledEvent<E> {
    pub at: Timestamp,
    pub sequence: u64,
    pub kind: E,
}

// Ordering is defined purely on `(at, sequence)` so the payload `E` need not be
// orderable. The sequence is unique and monotonic, so the order is total and
// resolves same-timestamp ties to insertion order.
impl<E> PartialEq for ScheduledEvent<E> {
    fn eq(&self, other: &Self) -> bool {
        self.at == other.at && self.sequence == other.sequence
    }
}

impl<E> Eq for ScheduledEvent<E> {}

impl<E> Ord for ScheduledEvent<E> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.at
            .cmp(&other.at)
            .then_with(|| self.sequence.cmp(&other.sequence))
    }
}

impl<E> PartialOrd for ScheduledEvent<E> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A machine's dispatch target for due events.
///
/// The scheduler pops each due event and hands it to the handler along with a
/// [`EventContext`], through which the handler may schedule follow-up events —
/// including ones at the current timestamp, to model same-cycle cascades.
pub trait EventHandler<E> {
    fn handle(&mut self, event: E, ctx: &mut EventContext<'_, E>);
}

/// The scheduler-level context passed to an [`EventHandler`]. Device-level
/// context (interrupt controller, bus, …) is the handler's own state.
pub struct EventContext<'a, E> {
    pub now: Timestamp,
    pub scheduler: &'a mut Scheduler<E>,
}

/// The deterministic event queue and its position on the guest timeline.
#[derive(Clone, Debug)]
pub struct Scheduler<E> {
    now: Timestamp,
    next_sequence: u64,
    events: BinaryHeap<Reverse<ScheduledEvent<E>>>,
}

impl<E> Default for Scheduler<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E> Scheduler<E> {
    /// Create an empty scheduler positioned at time zero.
    pub fn new() -> Self {
        Scheduler {
            now: 0,
            next_sequence: 0,
            events: BinaryHeap::new(),
        }
    }

    /// The current guest timestamp.
    pub fn now(&self) -> Timestamp {
        self.now
    }

    /// The timestamp of the earliest pending event, or `None` if the queue is
    /// empty. This is the deadline the CPU must not execute past. The event may
    /// turn out to be stale; that is resolved when it is dispatched, not here.
    pub fn next_deadline(&self) -> Option<Timestamp> {
        self.events.peek().map(|Reverse(event)| event.at)
    }

    /// Schedule `kind` to fire at absolute time `at`.
    ///
    /// Panics in debug builds if `at` is before [`Scheduler::now`]; scheduling
    /// into the past is always a bug.
    pub fn schedule_at(&mut self, at: Timestamp, kind: E) -> EventHandle {
        debug_assert!(
            at >= self.now,
            "scheduled event in the past: at={at}, now={}",
            self.now
        );
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.events.push(Reverse(ScheduledEvent { at, sequence, kind }));
        EventHandle(sequence)
    }

    /// Schedule `kind` to fire `delta` ticks from now.
    ///
    /// Panics on timestamp overflow — time arithmetic is never silently wrapped.
    pub fn schedule_after(&mut self, delta: Timestamp, kind: E) -> EventHandle {
        let at = self
            .now
            .checked_add(delta)
            .expect("guest timestamp overflow");
        self.schedule_at(at, kind)
    }

    /// Advance the current time to `now`.
    ///
    /// Time may not move backward during normal execution (checkpoint restore is
    /// a separate operation that replaces the whole scheduler). This is normally
    /// called with the CPU's timestamp after it runs to a deadline.
    pub fn set_now(&mut self, now: Timestamp) {
        debug_assert!(
            now >= self.now,
            "time moved backward: now={now}, previous={}",
            self.now
        );
        self.now = now;
    }

    /// Jump directly to the next event's time, returning it. Used when the CPU
    /// is halted and executes no instructions, so the timeline can still
    /// progress to the next hardware transition. Returns `None` if the queue is
    /// empty (nothing will ever wake the machine).
    pub fn advance_to_next_event(&mut self) -> Option<Timestamp> {
        let at = self.next_deadline()?;
        self.set_now(at);
        Some(at)
    }

    /// Dispatch every event due at the current time.
    ///
    /// All events with `at <= now` are handled. Dispatch may itself schedule new
    /// events at the current timestamp, and those are handled too before the
    /// method returns — the loop continues until no event remains at `now`. This
    /// is what makes same-cycle cascades (e.g. one timer overflow driving
    /// another) complete deterministically before the CPU resumes.
    pub fn run_due_events<H: EventHandler<E>>(&mut self, handler: &mut H) {
        self.run_due_events_traced(handler, |_, _| {});
    }

    /// Like [`Scheduler::run_due_events`], but calls `on_fire(now, &event)` for
    /// each event just before it is dispatched — a hook for tracing.
    pub fn run_due_events_traced<H, F>(&mut self, handler: &mut H, mut on_fire: F)
    where
        H: EventHandler<E>,
        F: FnMut(Timestamp, &E),
    {
        loop {
            match self.events.peek() {
                Some(Reverse(event)) if event.at <= self.now => {}
                _ => break,
            }
            let Reverse(event) = self.events.pop().expect("peek just succeeded");
            let now = self.now;
            on_fire(now, &event.kind);
            handler.handle(
                event.kind,
                &mut EventContext {
                    now,
                    scheduler: &mut *self,
                },
            );
        }
    }

    /// The number of pending events, stale ones included.
    pub fn pending_count(&self) -> usize {
        self.events.len()
    }
}

impl<E: Copy> Scheduler<E> {
    /// A chronologically sorted snapshot of the pending events, for inspection.
    /// Ordering matches dispatch order: by `at`, then insertion sequence.
    pub fn pending_events(&self) -> Vec<ScheduledEvent<E>> {
        let mut events: Vec<ScheduledEvent<E>> =
            self.events.iter().map(|Reverse(event)| *event).collect();
        events.sort_unstable();
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum TestEvent {
        Plain(u32),
        /// When handled, schedules a `Plain` at the same timestamp to exercise
        /// same-cycle cascades.
        Cascade(u32),
        /// Carries a generation for the stale-event test.
        Generational { id: u32, generation: u32 },
    }

    /// A handler that records the payloads it fired, in order.
    #[derive(Default)]
    struct Recorder {
        fired: Vec<TestEvent>,
        /// The "current" generation of device 0, for the stale-event test.
        device_generation: u32,
    }

    impl EventHandler<TestEvent> for Recorder {
        fn handle(&mut self, event: TestEvent, ctx: &mut EventContext<'_, TestEvent>) {
            match event {
                TestEvent::Cascade(n) => {
                    self.fired.push(event);
                    ctx.scheduler
                        .schedule_at(ctx.now, TestEvent::Plain(n + 100));
                }
                TestEvent::Generational { generation, .. } => {
                    // Ignore events from a superseded generation.
                    if generation == self.device_generation {
                        self.fired.push(event);
                    }
                }
                TestEvent::Plain(_) => self.fired.push(event),
            }
        }
    }

    /// Drive the scheduler the way the machine loop does: repeatedly advance to
    /// the next deadline and run due events, until the queue drains.
    fn drain(scheduler: &mut Scheduler<TestEvent>, recorder: &mut Recorder) {
        while let Some(deadline) = scheduler.next_deadline() {
            scheduler.set_now(deadline);
            scheduler.run_due_events(recorder);
        }
    }

    #[test]
    fn dispatches_in_chronological_order() {
        let mut s = Scheduler::new();
        let mut r = Recorder::default();
        s.schedule_at(300, TestEvent::Plain(3));
        s.schedule_at(100, TestEvent::Plain(1));
        s.schedule_at(200, TestEvent::Plain(2));
        drain(&mut s, &mut r);
        assert_eq!(
            r.fired,
            vec![
                TestEvent::Plain(1),
                TestEvent::Plain(2),
                TestEvent::Plain(3)
            ]
        );
    }

    #[test]
    fn same_time_keeps_insertion_order() {
        let mut s = Scheduler::new();
        let mut r = Recorder::default();
        s.schedule_at(100, TestEvent::Plain(1));
        s.schedule_at(100, TestEvent::Plain(2));
        s.schedule_at(100, TestEvent::Plain(3));
        drain(&mut s, &mut r);
        assert_eq!(
            r.fired,
            vec![
                TestEvent::Plain(1),
                TestEvent::Plain(2),
                TestEvent::Plain(3)
            ]
        );
    }

    #[test]
    fn same_time_recursive_scheduling_completes_before_leaving_now() {
        let mut s = Scheduler::new();
        let mut r = Recorder::default();
        // Cascade fires at 100 and schedules Plain(105) also at 100.
        s.schedule_at(100, TestEvent::Cascade(5));
        s.schedule_at(200, TestEvent::Plain(9));
        s.set_now(100);
        s.run_due_events(&mut r);
        // Both the cascade and its same-time consequence fired; the 200 event
        // did not, because time has not advanced there yet.
        assert_eq!(
            r.fired,
            vec![TestEvent::Cascade(5), TestEvent::Plain(105)]
        );
        assert_eq!(s.next_deadline(), Some(200));
    }

    #[test]
    fn stale_generation_events_are_ignored_by_the_handler() {
        let mut s = Scheduler::new();
        let mut r = Recorder::default();
        // Device 0 is at generation 0 when this event is scheduled.
        s.schedule_at(
            100,
            TestEvent::Generational {
                id: 0,
                generation: 0,
            },
        );
        // Reconfiguration bumps the device's generation, invalidating the above.
        r.device_generation = 1;
        s.schedule_at(
            200,
            TestEvent::Generational {
                id: 0,
                generation: 1,
            },
        );
        drain(&mut s, &mut r);
        // Only the current-generation event was acted on; the stale one, though
        // it fired, was dropped by the handler.
        assert_eq!(
            r.fired,
            vec![TestEvent::Generational {
                id: 0,
                generation: 1
            }]
        );
    }

    #[test]
    fn next_deadline_reports_earliest_event() {
        let mut s: Scheduler<TestEvent> = Scheduler::new();
        assert_eq!(s.next_deadline(), None);
        s.schedule_at(500, TestEvent::Plain(1));
        s.schedule_at(100, TestEvent::Plain(2));
        assert_eq!(s.next_deadline(), Some(100));
    }

    #[test]
    fn halt_progression_advances_without_cpu_work() {
        // With the "CPU" executing nothing, the scheduler still reaches the
        // event at 100 and fires it.
        let mut s = Scheduler::new();
        let mut r = Recorder::default();
        s.schedule_at(100, TestEvent::Plain(7));
        let reached = s.advance_to_next_event();
        assert_eq!(reached, Some(100));
        assert_eq!(s.now(), 100);
        s.run_due_events(&mut r);
        assert_eq!(r.fired, vec![TestEvent::Plain(7)]);
    }

    #[test]
    fn schedule_after_is_relative_to_now() {
        let mut s: Scheduler<TestEvent> = Scheduler::new();
        s.set_now(1000);
        s.schedule_after(250, TestEvent::Plain(1));
        assert_eq!(s.next_deadline(), Some(1250));
    }

    #[test]
    fn pending_events_are_sorted() {
        let mut s = Scheduler::new();
        s.schedule_at(300, TestEvent::Plain(3));
        s.schedule_at(100, TestEvent::Plain(1));
        s.schedule_at(100, TestEvent::Plain(2));
        let pending = s.pending_events();
        let times: Vec<_> = pending.iter().map(|e| e.at).collect();
        assert_eq!(times, vec![100, 100, 300]);
        // Same-time events keep insertion order in the snapshot too.
        assert_eq!(pending[0].kind, TestEvent::Plain(1));
        assert_eq!(pending[1].kind, TestEvent::Plain(2));
    }

    #[test]
    #[should_panic(expected = "time moved backward")]
    fn set_now_backward_panics_in_debug() {
        let mut s: Scheduler<TestEvent> = Scheduler::new();
        s.set_now(100);
        s.set_now(50);
    }

    #[test]
    #[should_panic(expected = "scheduled event in the past")]
    fn scheduling_in_the_past_panics_in_debug() {
        let mut s: Scheduler<TestEvent> = Scheduler::new();
        s.set_now(100);
        s.schedule_at(50, TestEvent::Plain(1));
    }
}
