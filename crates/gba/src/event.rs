//! The GBA's scheduled-event taxonomy.
//!
//! This is the concrete payload plugged into `emu_core::Scheduler`, which is
//! generic over it. Each variant names an externally observable hardware
//! transition. Only the timer is modeled so far; PPU-timing, DMA, audio, and
//! cartridge events join this enum as those devices are built.

use crate::timer::TimerId;

/// A scheduled GBA hardware event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Timer(TimerEvent),
}

/// Timer events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerEvent {
    /// A timer's counter is about to wrap past `0xFFFF`. `generation` is checked
    /// against the timer's current generation on dispatch so that events left
    /// stale by a reconfiguration are ignored rather than removed from the
    /// queue.
    Overflow { timer: TimerId, generation: u32 },
}
