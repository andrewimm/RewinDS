//! GBA hardware semantics.
//!
//! This crate owns the GBA's devices, memory map, and timing behavior, built on
//! the machine-agnostic `emu_core` scheduler and bus vocabulary. It supplies the
//! concrete [`EventKind`] plugged into `emu_core::Scheduler` and the [`Gba`]
//! machine whose [`Bus`] the CPU (and later DMA) drives.

pub mod bus;
pub mod dma;
pub mod event;
pub mod interrupt;
pub mod io;
pub mod machine;
pub mod ppu;
pub mod system;
pub mod timer;

pub use bus::{Bus, Memory};
pub use dma::{Dma, DmaChannel, DmaTiming};
pub use event::{EventKind, PpuEvent, TimerEvent};
pub use interrupt::{InterruptController, IrqSource};
pub use io::{Io, PowerState, SystemControl};
pub use machine::Gba;
pub use ppu::Ppu;
pub use system::{HaltProgress, System};
pub use timer::{TimerId, Timers};

// Re-export the shared bus vocabulary so callers of this crate's bus don't need
// to depend on emu-core directly.
pub use emu_core::{Access, AccessKind, AccessMaster, AccessSequence, AccessWidth, BusResult};
