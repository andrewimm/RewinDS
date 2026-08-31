//! The shared 2D software renderer for the GBA and NDS 2D engines.
//!
//! This crate owns the pure, machine-independent pixel pipeline: the register
//! snapshot, the background/object generators, the compositor, priority resolver,
//! and color effects, plus the pixel-provenance instrumentation. It reads guest
//! graphics memory only through [`memory::PpuMemoryView`] (three region slices),
//! so it depends on no bus, scheduler, or machine crate.
//!
//! A machine's PPU aggregate owns the framebuffer, the latched segments, and the
//! scratch buffers, and drives the renderer through [`scanline::render_scanline`]
//! (and the [`latch`] helpers). Timing, MMIO routing, and interrupts stay on the
//! machine side.

pub mod bg;
pub mod compositor;
pub mod debug;
pub mod effects;
pub mod latch;
pub mod memory;
pub mod obj;
pub mod priority;
pub mod registers;
pub mod scanline;
pub mod state;
pub mod window;

pub use memory::PpuMemoryView;
pub use registers::Registers;
pub use scanline::{render_scanline, scanline_state_explanation};
pub use state::{Color15, LayerId, HEIGHT, WIDTH};

#[cfg(test)]
mod test_support;
#[cfg(test)]
pub use test_support::TestPpu;
