//! Background candidate generation: dispatch the active video mode to the text,
//! affine, or bitmap sampler, filling the per-layer scratch lines.

pub mod affine;
pub mod bitmap;
pub mod extended;
pub mod text;

use super::debug::sink::ProvenanceSink;
use super::memory::{ModeSemantics, PpuMemoryView, VramLayout};
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
#[allow(clippy::too_many_arguments)]
pub fn generate<S: ProvenanceSink>(
    y: u16,
    state: &LatchedState,
    affine: &AffineInternalState,
    mem: &PpuMemoryView,
    width: usize,
    layout: VramLayout,
    scratch: &mut Scratch,
    sink: &mut S,
) {
    if state.forced_blank() {
        return;
    }
    if layout.mode_semantics == ModeSemantics::Ds {
        generate_ds(y, state, affine, mem, width, layout, scratch, sink);
        return;
    }
    match state.mode {
        // Mode 0: four text backgrounds.
        0 => {
            for bg in 0..4 {
                text::render_text_scanline(bg, y, state, mem, width, layout, scratch, sink);
            }
        }
        // Mode 1: BG0/BG1 text, BG2 affine.
        1 => {
            for bg in 0..2 {
                text::render_text_scanline(bg, y, state, mem, width, layout, scratch, sink);
            }
            affine::render_affine_scanline(2, y, state, affine.bg2, mem, width, layout, scratch, sink);
        }
        // Mode 2: BG2 and BG3 affine.
        2 => {
            affine::render_affine_scanline(2, y, state, affine.bg2, mem, width, layout, scratch, sink);
            affine::render_affine_scanline(3, y, state, affine.bg3, mem, width, layout, scratch, sink);
        }
        3 => bitmap::render_mode3(y, state, mem, scratch, sink),
        4 => bitmap::render_mode4(y, state, mem, scratch, sink),
        5 => bitmap::render_mode5(y, state, mem, scratch, sink),
        // Modes 6/7 are invalid; nothing is drawn.
        _ => {}
    }
}

/// The kind of layer a DS background renders as, given the video mode. Unlike the
/// GBA — whose mode selects one fixed arrangement — the DS assigns each of BG0-3 a
/// type per mode, mixing text, affine, and "extended" backgrounds on one screen.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DsBg {
    Text,
    Affine,
    /// An affine-addressed background carrying either a 16-bit tile map or a bitmap
    /// (sub-type from `BGxCNT`). Not yet rendered (Phase A) — treated as transparent.
    Extended,
    /// This background does not exist in this mode.
    None,
}

/// The DS layer type for background `bg` (0-3) in DISPCNT `mode` (Engine A table; the
/// Engine B subset never sets the 3D BG0 or uses mode 6, so the same table serves).
/// BG0 is always `Text` here — the caller drops it when it is the 3D engine instead.
fn ds_bg_type(mode: u8, bg: usize) -> DsBg {
    use DsBg::*;
    match bg {
        0 => Text,
        1 => {
            if mode <= 5 {
                Text
            } else {
                None
            }
        }
        2 => match mode {
            0 | 1 | 3 => Text,
            2 | 4 => Affine,
            5 | 6 => Extended, // mode 6 BG2 is a large bitmap (an extended sub-type)
            _ => None,
        },
        3 => match mode {
            0 => Text,
            1 | 2 => Affine,
            3..=5 => Extended,
            _ => None,
        },
        _ => None,
    }
}

/// Generate this scanline's background candidates under DS mode semantics: dispatch
/// each of BG0-3 to its per-mode layer type, skipping BG0 when it is the 3D engine
/// (`DISPCNT` bit 3), which the 2D renderer does not produce.
#[allow(clippy::too_many_arguments)]
fn generate_ds<S: ProvenanceSink>(
    y: u16,
    state: &LatchedState,
    affine: &AffineInternalState,
    mem: &PpuMemoryView,
    width: usize,
    layout: VramLayout,
    scratch: &mut Scratch,
    sink: &mut S,
) {
    let bg0_is_3d = state.regs.dispcnt & (1 << 3) != 0;
    for bg in 0..4 {
        if bg == 0 && bg0_is_3d {
            continue; // the 3D engine output is not a 2D layer
        }
        match ds_bg_type(state.mode, bg) {
            DsBg::Text => {
                text::render_text_scanline(bg, y, state, mem, width, layout, scratch, sink)
            }
            DsBg::Affine => {
                let reference = if bg == 2 { affine.bg2 } else { affine.bg3 };
                affine::render_affine_scanline(
                    bg, y, state, reference, mem, width, layout, scratch, sink,
                );
            }
            DsBg::Extended => {
                let reference = if bg == 2 { affine.bg2 } else { affine.bg3 };
                extended::render_extended_scanline(
                    bg, y, state, reference, mem, width, layout, scratch, sink,
                );
            }
            DsBg::None => {}
        }
    }
}
