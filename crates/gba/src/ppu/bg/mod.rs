//! Background candidate generation: dispatch the active video mode to the text,
//! affine, or bitmap sampler, filling the per-layer scratch lines.

pub mod bitmap;

use super::debug::sink::ProvenanceSink;
use super::memory::PpuMemoryView;
use super::state::{AffineInternalState, LatchedState, Scratch};

/// Generate this scanline's background candidates into `scratch`, according to the
/// latched video mode. Force-blank produces nothing (the screen shows white via
/// the backdrop path).
pub fn generate<S: ProvenanceSink>(
    y: u16,
    state: &LatchedState,
    _affine: &AffineInternalState,
    mem: &PpuMemoryView,
    scratch: &mut Scratch,
    sink: &mut S,
) {
    if state.forced_blank() {
        return;
    }
    match state.mode {
        // Modes 0-2 (text/affine tiled backgrounds) are not yet implemented.
        0..=2 => {}
        3 => bitmap::render_mode3(y, state, mem, scratch, sink),
        4 => bitmap::render_mode4(y, state, mem, scratch, sink),
        5 => bitmap::render_mode5(y, state, mem, scratch, sink),
        // Modes 6/7 are invalid; nothing is drawn.
        _ => {}
    }
}
