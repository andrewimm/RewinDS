//! GBA hardware semantics.
//!
//! This crate owns the GBA's devices and their timing behavior, built on the
//! machine-agnostic `emu_core` scheduler. It supplies the concrete
//! [`EventKind`] plugged into `emu_core::Scheduler` and the [`Gba`] machine that
//! dispatches those events into its devices.

pub mod event;
pub mod interrupt;
pub mod machine;
pub mod ppu;
pub mod system;
pub mod timer;

pub use event::{EventKind, PpuEvent, TimerEvent};
pub use interrupt::{InterruptController, IrqSource};
pub use machine::{Gba, PowerState};
pub use ppu::Ppu;
pub use system::{HaltProgress, System};
pub use timer::{TimerId, Timers};
