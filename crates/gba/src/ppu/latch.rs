//! Register latching: capturing the CPU-visible register block into the snapshot
//! that governs a scanline.
//!
//! Renderers read only the latched state, never live MMIO, so a mid-frame write
//! affects only lines that latch after it. This is the dedicated home for
//! scanline latching and, once affine backgrounds exist, internal
//! affine-reference advancement and delayed register effects.

use super::state::{AffineReference, LatchedState, ScanlineSegment, HEIGHT, WIDTH};
use super::Ppu;
use emu_core::Timestamp;

impl Ppu {
    /// Snapshot the live registers for the scanline about to be drawn, resetting
    /// it to a single full-width segment.
    pub(crate) fn latch_for_scanline(&mut self) {
        self.latched = LatchedState::from_registers(&self.registers);
        self.segments.clear();
        self.segments.push(ScanlineSegment {
            x_start: 0,
            state: self.latched,
            affine: self.affine,
        });
    }

    /// Record a mid-scanline register write: split the current scanline at the
    /// pixel the write lands on, so the new register state governs the rest of the
    /// line. A write outside the visible draw (VBlank, or past the last pixel) only
    /// updates the registers, taking effect at the next line's latch.
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
    /// registers. Done at frame start and whenever the CPU writes a reference.
    pub(crate) fn reload_affine_references(&mut self) {
        self.affine.bg2 = AffineReference {
            x: self.registers.bg_ref_x[0],
            y: self.registers.bg_ref_y[0],
        };
        self.affine.bg3 = AffineReference {
            x: self.registers.bg_ref_x[1],
            y: self.registers.bg_ref_y[1],
        };
    }

    /// Advance each internal affine reference by its `PB`/`PD` as one visible
    /// scanline completes — this is what distinguishes the internal reference from
    /// the CPU-visible register.
    pub(crate) fn advance_affine_references(&mut self) {
        self.affine.bg2.x += self.registers.bg_pb[0] as i32;
        self.affine.bg2.y += self.registers.bg_pd[0] as i32;
        self.affine.bg3.x += self.registers.bg_pb[1] as i32;
        self.affine.bg3.y += self.registers.bg_pd[1] as i32;
    }

    /// The affine reference that background index `k` (0 = BG2, 1 = BG3) would hold
    /// at line `y`, reconstructed from the register reference. The explain path
    /// uses this because it does not run the frame that advances the internal
    /// reference. Exact when no affine register changed mid-frame.
    pub(crate) fn affine_reference_for_line(&self, k: usize, y: u16) -> AffineReference {
        AffineReference {
            x: self.registers.bg_ref_x[k] + self.registers.bg_pb[k] as i32 * y as i32,
            y: self.registers.bg_ref_y[k] + self.registers.bg_pd[k] as i32 * y as i32,
        }
    }
}
