//! Nintendo DS hardware semantics.
//!
//! This crate owns the DS-specific machine: the ARM9 (ARMv5TE) and ARM7 (ARMv4T)
//! cores, their shared scheduler, the DS memory maps, and the specialised
//! coprocessor and 2D/3D hardware. It builds on the device-agnostic `arm`
//! interpreter, turning on the ARMv5TE delta for the ARM9.
//!
//! The bring-up starts with the CP15 system-control coprocessor ([`cp15`]),
//! which the ARM9 drives through `MCR`/`MRC` to place the tightly coupled
//! memories in the memory map.

pub mod boot;
pub mod bus;
pub mod cp15;
pub mod dma;
pub mod interrupt;
pub mod ipc;
pub mod key1;
pub mod memory;
pub mod ppu;
pub mod system;
pub mod timer;
pub mod vram;

pub use boot::{BootError, Header};
pub use cp15::Cp15;
pub use dma::{Dma, DmaTiming};
pub use interrupt::{Interrupts, IrqSource};
pub use ipc::Ipc;
pub use memory::{Core, Memory};
pub use ppu::Ppu;
pub use system::{Machine, NdsEvent, System};
pub use timer::{TimerId, Timers};
pub use vram::Vram;
