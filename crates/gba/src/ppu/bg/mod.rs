//! Background candidate generation: dispatch the active video mode to the text,
//! affine, or bitmap sampler, filling the per-layer scratch lines.

pub mod affine;
pub mod bitmap;
pub mod text;

use super::debug::sink::ProvenanceSink;
use super::memory::PpuMemoryView;
use super::state::{AffineInternalState, LatchedState, Scratch};

/// The mosaic block `(horizontal, vertical)` sizes for background `bg`, and
/// whether mosaic is active. When inactive the factors are `1` (no snapping).
pub(super) fn bg_mosaic_factors(state: &LatchedState, bg: usize) -> (usize, usize, bool) {
    if state.bg_mosaic_enabled(bg) {
        let (h, v) = state.bg_mosaic();
        (h, v, true)
    } else {
        (1, 1, false)
    }
}

/// Generate this scanline's background candidates into `scratch`, according to the
/// latched video mode. Force-blank produces nothing (the screen shows white via
/// the backdrop path).
pub fn generate<S: ProvenanceSink>(
    y: u16,
    state: &LatchedState,
    affine: &AffineInternalState,
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
        // Mode 1: BG0/BG1 text, BG2 affine.
        1 => {
            for bg in 0..2 {
                text::render_text_scanline(bg, y, state, mem, scratch, sink);
            }
            affine::render_affine_scanline(2, y, state, affine.bg2, mem, scratch, sink);
        }
        // Mode 2: BG2 and BG3 affine.
        2 => {
            affine::render_affine_scanline(2, y, state, affine.bg2, mem, scratch, sink);
            affine::render_affine_scanline(3, y, state, affine.bg3, mem, scratch, sink);
        }
        3 => bitmap::render_mode3(y, state, mem, scratch, sink),
        4 => bitmap::render_mode4(y, state, mem, scratch, sink),
        5 => bitmap::render_mode5(y, state, mem, scratch, sink),
        // Modes 6/7 are invalid; nothing is drawn.
        _ => {}
    }
}
