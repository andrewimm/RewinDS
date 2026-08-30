//! Bitmap-mode backgrounds (video modes 3, 4, 5) — BG2 sampled as a framebuffer.
//!
//! Bitmap output is not special-cased into the screen: it produces ordinary BG2
//! candidates that flow through windowing, priority, and effects like any other
//! source.

use crate::ppu::debug::explain::CandidateExplanation;
use crate::ppu::debug::provenance::{BitmapBgProvenance, BitmapSample, SourceProvenance};
use crate::ppu::debug::sink::ProvenanceSink;
use crate::ppu::memory::{PpuMemoryView, VRAM_BASE};
use crate::ppu::state::{CandidatePixel, Color15, LatchedState, LayerId, PixelFlags, Scratch, WIDTH};

/// The BG2 index into the scratch/priority machinery.
const BG2: usize = 2;

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
        let pixel_index = y as usize * WIDTH + x;
        let byte_offset = pixel_index * 2;
        let color = Color15(mem.vram16(byte_offset));

        // Bitmap modes have no transparent color: every pixel is opaque.
        let candidate = CandidatePixel {
            color,
            layer: LayerId::Bg2,
            priority,
            flags: PixelFlags::default(),
        };
        scratch.bg[BG2].pixels[x] = Some(candidate);

        if sink.wants(x as u16) {
            let vram_address = VRAM_BASE + byte_offset as u32;
            sink.record_candidate(x as u16, LayerId::Bg2, || CandidateExplanation {
                candidate,
                provenance: SourceProvenance::BitmapBg(BitmapBgProvenance {
                    video_mode: 3,
                    frame: 0,
                    source_x: x as u16,
                    source_y: y,
                    vram_address,
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
