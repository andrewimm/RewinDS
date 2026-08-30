//! Background candidate generation: dispatch the active video mode to the text,
//! affine, or bitmap sampler, filling the per-layer scratch lines.

pub mod bitmap;
pub mod text;

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
        // Mode 0: four text backgrounds.
        0 => {
            for bg in 0..4 {
                text::render_text_scanline(bg, y, state, mem, scratch, sink);
            }
        }
        // Mode 1: BG0/BG1 text, BG2 affine (affine backgrounds not yet implemented).
        1 => {
            for bg in 0..2 {
                text::render_text_scanline(bg, y, state, mem, scratch, sink);
            }
        }
        // Mode 2: BG2/BG3 affine (not yet implemented).
        2 => {}
        3 => bitmap::render_mode3(y, state, mem, scratch, sink),
        4 => bitmap::render_mode4(y, state, mem, scratch, sink),
        5 => bitmap::render_mode5(y, state, mem, scratch, sink),
        // Modes 6/7 are invalid; nothing is drawn.
        _ => {}
    }
}
