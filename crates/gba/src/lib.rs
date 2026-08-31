//! GBA hardware semantics.
//!
//! This crate owns the GBA's devices, memory map, and timing behavior, built on
//! the machine-agnostic `emu_core` scheduler and bus vocabulary. It supplies the
//! concrete [`EventKind`] plugged into `emu_core::Scheduler` and the [`Gba`]
//! machine whose [`Bus`] the CPU (and later DMA) drives.

pub mod apu;
pub mod bus;
pub mod cartridge;
pub mod dma;
pub mod event;
pub mod interrupt;
pub mod io;
pub mod keypad;
pub mod machine;
pub mod ppu;
pub mod prefetch;
pub mod serial;
pub mod system;
pub mod timer;
pub mod trace;

pub use apu::Apu;
pub use bus::{Bus, Memory};
pub use cartridge::{Backup, Cartridge, FlashSize, SaveType};
pub use dma::{Dma, DmaChannel, DmaTiming};
pub use prefetch::Prefetch;
pub use event::{EventKind, PpuEvent, TimerEvent};
pub use interrupt::{InterruptController, IrqSource};
pub use io::{Io, PowerState, SystemControl};
pub use keypad::{Key, Keypad};
pub use machine::Gba;
pub use ppu::debug::{
    ExplainError, PixelExplanation, ScanlineExplanation, SourceProvenance, VideoInstrumentation,
};
pub use ppu::inspect::{BackgroundKind, BackgroundSummary};
pub use ppu::memory::PpuMemoryView;
pub use ppu::obj::evaluate::SpriteInstance;
pub use ppu::{Color15, LayerId, Ppu};
pub use system::{HaltProgress, System};
pub use timer::{TimerId, Timers};
pub use trace::{Trace, TraceRecord};

// Re-export the shared bus vocabulary so callers of this crate's bus don't need
// to depend on emu-core directly.
pub use emu_core::{Access, AccessKind, AccessMaster, AccessSequence, AccessWidth, BusResult};
