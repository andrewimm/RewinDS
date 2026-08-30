//! Register latching: capturing the CPU-visible register block into the snapshot
//! that governs a scanline.
//!
//! Renderers read only the latched state, never live MMIO, so a mid-frame write
//! affects only lines that latch after it. This is the dedicated home for
//! scanline latching and, once affine backgrounds exist, internal
//! affine-reference advancement and delayed register effects.

use super::state::LatchedState;
use super::Ppu;

impl Ppu {
    /// Snapshot the live registers for the scanline about to be drawn.
    pub(crate) fn latch_for_scanline(&mut self) {
        self.latched = LatchedState::from_registers(&self.registers);
        // Affine backgrounds will also advance the internal reference here and
        // reload it on BGxX/Y writes.
    }
}
