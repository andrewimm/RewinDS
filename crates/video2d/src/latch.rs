//! Register latching: capturing a CPU-visible register block into the snapshot
//! that governs a scanline, plus the internal affine-reference bookkeeping.
//!
//! Renderers read only the latched state, never live registers, so a mid-frame
//! write affects only lines that latch after it. These are the pure operations;
//! the machine's PPU aggregate owns the register/affine/segment storage and calls
//! them (mid-line segment splitting, which needs guest time, lives on the
//! machine side).

use crate::registers::Registers;
use crate::state::{AffineInternalState, AffineReference, LatchedState, ScanlineSegment};

/// Snapshot the live registers for the scanline about to be drawn, resetting the
/// segment list to a single full-width segment.
pub fn latch_for_scanline(
    registers: &Registers,
    affine: &AffineInternalState,
    latched: &mut LatchedState,
    segments: &mut Vec<ScanlineSegment>,
) {
    *latched = LatchedState::from_registers(registers);
    segments.clear();
    segments.push(ScanlineSegment {
        x_start: 0,
        state: *latched,
        affine: *affine,
    });
}

/// Reload the internal affine reference points from the `BGxX`/`BGxY` registers.
/// Done at frame start and whenever the CPU writes a reference.
pub fn reload_affine_references(registers: &Registers, affine: &mut AffineInternalState) {
    affine.bg2 = AffineReference {
        x: registers.bg_ref_x[0],
        y: registers.bg_ref_y[0],
    };
    affine.bg3 = AffineReference {
        x: registers.bg_ref_x[1],
        y: registers.bg_ref_y[1],
    };
}

/// Advance each internal affine reference by its `PB`/`PD` as one visible
/// scanline completes — what distinguishes the internal reference from the
/// CPU-visible register.
pub fn advance_affine_references(registers: &Registers, affine: &mut AffineInternalState) {
    affine.bg2.x += registers.bg_pb[0] as i32;
    affine.bg2.y += registers.bg_pd[0] as i32;
    affine.bg3.x += registers.bg_pb[1] as i32;
    affine.bg3.y += registers.bg_pd[1] as i32;
}

/// The affine reference that background index `k` (0 = BG2, 1 = BG3) would hold
/// at line `y`, reconstructed from the register reference. The explain path uses
/// this because it does not run the frame that advances the internal reference.
pub fn affine_reference_for_line(registers: &Registers, k: usize, y: u16) -> AffineReference {
    AffineReference {
        x: registers.bg_ref_x[k] + registers.bg_pb[k] as i32 * y as i32,
        y: registers.bg_ref_y[k] + registers.bg_pd[k] as i32 * y as i32,
    }
}
