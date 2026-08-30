//! Machine-agnostic emulator core: the guest timeline, the deterministic
//! scheduler, and (later) the memory bus and debugger API.
//!
//! This crate has no GBA or DS knowledge. It provides the infrastructure those
//! machines are built on — most centrally a [`Scheduler`] that is generic over
//! the event payload, so each machine supplies its own event kinds and devices.

pub mod scheduler;
pub mod time;

pub use scheduler::{
    EventContext, EventHandle, EventHandler, ScheduledEvent, Scheduler,
};
pub use time::Timestamp;
