//! The guest timeline.
//!
//! There is one authoritative machine timestamp for the whole emulator. Every
//! hardware component lives on this single timeline; no device may consult host
//! wall-clock time to decide guest behavior.

/// A point on the guest timeline, in master ticks.
///
/// For the GBA this is currently one system clock cycle. Before DS support this
/// may become a finer master-tick unit that divides exactly into every DS clock
/// domain — a change that stays confined to this alias and the clock-conversion
/// layer, not the scheduler.
pub type Timestamp = u64;
