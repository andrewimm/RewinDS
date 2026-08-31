//! Register latching on the aggregate `Ppu`: thin methods over the pure
//! [`video2d::latch`] helpers, plus mid-line segment splitting (which needs guest
//! time and so stays machine-side).

use super::state::{LatchedState, ScanlineSegment, HEIGHT, WIDTH};
use super::Ppu;
use emu_core::Timestamp;

impl Ppu {
    /// Snapshot the live registers for the scanline about to be drawn.
    pub(crate) fn latch_for_scanline(&mut self) {
        video2d::latch::latch_for_scanline(
            &self.registers,
            &self.affine,
            &mut self.latched,
            &mut self.segments,
        );
    }

    /// Record a mid-scanline register write: split the current scanline at the
    /// pixel the write lands on, so the new register state governs the rest of the
    /// line. A write outside the visible draw only updates the registers, taking
    /// effect at the next line's latch.
    pub(crate) fn split_segment(&mut self, now: Timestamp) {
        if (self.vcount() as usize) >= HEIGHT {
            return;
        }
        // Four cycles per pixel across the 240-pixel visible draw.
        let x = (now.saturating_sub(self.line_start_time) / 4) as usize;
        if x >= WIDTH {
            return;
        }
        let state = LatchedState::from_registers(&self.registers);
        let segment = ScanlineSegment {
            x_start: x as u16,
            state,
            affine: self.affine,
        };
        match self.segments.last_mut() {
            // A write at or before the last split refines it in place.
            Some(last) if x as u16 <= last.x_start => *last = segment,
            _ => self.segments.push(segment),
        }
    }

    /// Reload the internal affine reference points from the `BGxX`/`BGxY`
    /// registers.
    pub(crate) fn reload_affine_references(&mut self) {
        video2d::latch::reload_affine_references(&self.registers, &mut self.affine);
    }

    /// Advance each internal affine reference by its `PB`/`PD` as one visible
    /// scanline completes.
    pub(crate) fn advance_affine_references(&mut self) {
        video2d::latch::advance_affine_references(&self.registers, &mut self.affine);
    }

    /// The affine reference background index `k` (0 = BG2, 1 = BG3) would hold at
    /// line `y`, reconstructed from the register reference.
    pub(crate) fn affine_reference_for_line(&self, k: usize, y: u16) -> super::state::AffineReference {
        video2d::latch::affine_reference_for_line(&self.registers, k, y)
    }
}
