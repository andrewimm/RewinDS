//! Bitmap-mode backgrounds (video modes 3, 4, 5) — BG2 sampled as a framebuffer.
//!
//! Bitmap output is not special-cased into the screen: it produces ordinary BG2
//! candidates that flow through windowing, priority, and effects like any other
//! source.

use crate::ppu::debug::explain::CandidateExplanation;
use crate::ppu::debug::provenance::{
    BitmapBgProvenance, BitmapSample, RejectionReason, SourceProvenance,
};
use crate::ppu::debug::sink::ProvenanceSink;
use crate::ppu::memory::{PpuMemoryView, PALETTE_BASE, VRAM_BASE};
use crate::ppu::state::{CandidatePixel, Color15, LatchedState, LayerId, PixelFlags, Scratch, WIDTH};

/// The BG2 index into the scratch/priority machinery.
const BG2: usize = 2;
/// Bytes per page in the paged bitmap modes (4 and 5): frame 1 follows frame 0.
const PAGE_SIZE: usize = 0xA000;
/// Mode 5's reduced framebuffer dimensions.
const MODE5_WIDTH: usize = 160;
const MODE5_HEIGHT: usize = 128;

/// The bitmap page selected by `DISPCNT` bit 4 (modes 4 and 5).
fn frame_index(state: &LatchedState) -> u8 {
    ((state.regs.dispcnt >> 4) & 1) as u8
}

/// A BG2 candidate at the background's configured priority.
fn bg2_candidate(color: Color15, priority: u8) -> CandidatePixel {
    CandidatePixel {
        color,
        layer: LayerId::Bg2,
        priority,
        flags: PixelFlags::default(),
    }
}

/// Render one Mode 3 scanline: a 240×160 array of direct 15-bit colors in VRAM.
pub fn render_mode3<S: ProvenanceSink>(
    y: u16,
    state: &LatchedState,
    mem: &PpuMemoryView,
    scratch: &mut Scratch,
    sink: &mut S,
) {
    if !state.bg_enabled(BG2) {
        return;
    }
    let priority = (state.regs.bgcnt[BG2] & 0x3) as u8;

    for x in 0..WIDTH {
        let byte_offset = (y as usize * WIDTH + x) * 2;
        let color = Color15(mem.vram16(byte_offset));

        // Direct-color bitmap modes have no transparent color: every pixel opaque.
        let candidate = bg2_candidate(color, priority);
        scratch.bg[BG2].pixels[x] = Some(candidate);

        if sink.wants(x as u16) {
            sink.record_candidate(x as u16, LayerId::Bg2, || CandidateExplanation {
                candidate,
                provenance: SourceProvenance::BitmapBg(BitmapBgProvenance {
                    video_mode: 3,
                    frame: 0,
                    source_x: x as u16,
                    source_y: y,
                    vram_address: VRAM_BASE + byte_offset as u32,
                    raw: BitmapSample::Direct(color),
                    palette_address: None,
                    resolved: color,
                    priority,
                }),
                visible_after_window: true,
                rejection_reason: None,
            });
        }
    }
}

/// Render one Mode 4 scanline: a 240×160 array of 8-bit palette indices in one of
/// two VRAM pages. Index 0 is transparent (the backdrop shows through).
pub fn render_mode4<S: ProvenanceSink>(
    y: u16,
    state: &LatchedState,
    mem: &PpuMemoryView,
    scratch: &mut Scratch,
    sink: &mut S,
) {
    if !state.bg_enabled(BG2) {
        return;
    }
    let priority = (state.regs.bgcnt[BG2] & 0x3) as u8;
    let frame = frame_index(state);
    let page_base = frame as usize * PAGE_SIZE;

    for x in 0..WIDTH {
        let vram_offset = page_base + y as usize * WIDTH + x;
        let index = mem.vram[vram_offset];
        let opaque = index != 0;
        let color = mem.palette15(index as usize);

        if opaque {
            scratch.bg[BG2].pixels[x] = Some(bg2_candidate(color, priority));
        }

        if sink.wants(x as u16) {
            let candidate = bg2_candidate(color, priority);
            sink.record_candidate(x as u16, LayerId::Bg2, || CandidateExplanation {
                candidate,
                provenance: SourceProvenance::BitmapBg(BitmapBgProvenance {
                    video_mode: 4,
                    frame,
                    source_x: x as u16,
                    source_y: y,
                    vram_address: VRAM_BASE + vram_offset as u32,
                    raw: BitmapSample::Indexed(index),
                    palette_address: Some(PALETTE_BASE + index as u32 * 2),
                    resolved: color,
                    priority,
                }),
                visible_after_window: opaque,
                rejection_reason: (!opaque).then_some(RejectionReason::TransparentPixel {
                    palette_index: 0,
                }),
            });
        }
    }
}

/// Render one Mode 5 scanline: a 160×128 direct-color framebuffer in one of two
/// VRAM pages. Coordinates outside those dimensions are transparent.
pub fn render_mode5<S: ProvenanceSink>(
    y: u16,
    state: &LatchedState,
    mem: &PpuMemoryView,
    scratch: &mut Scratch,
    sink: &mut S,
) {
    if !state.bg_enabled(BG2) {
        return;
    }
    if y as usize >= MODE5_HEIGHT {
        return;
    }
    let priority = (state.regs.bgcnt[BG2] & 0x3) as u8;
    let frame = frame_index(state);
    let page_base = frame as usize * PAGE_SIZE;

    for x in 0..MODE5_WIDTH {
        let byte_offset = page_base + (y as usize * MODE5_WIDTH + x) * 2;
        let color = Color15(mem.vram16(byte_offset));

        let candidate = bg2_candidate(color, priority);
        scratch.bg[BG2].pixels[x] = Some(candidate);

        if sink.wants(x as u16) {
            sink.record_candidate(x as u16, LayerId::Bg2, || CandidateExplanation {
                candidate,
                provenance: SourceProvenance::BitmapBg(BitmapBgProvenance {
                    video_mode: 5,
                    frame,
                    source_x: x as u16,
                    source_y: y,
                    vram_address: VRAM_BASE + byte_offset as u32,
                    raw: BitmapSample::Direct(color),
                    palette_address: None,
                    resolved: color,
                    priority,
                }),
                visible_after_window: true,
                rejection_reason: None,
            });
        }
    }
}
