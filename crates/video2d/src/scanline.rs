//! The one scanline renderer, as a machine-independent free function.
//!
//! `render_scanline` is generic over a [`ProvenanceSink`]: a frame loop drives it
//! with [`NullSink`](crate::debug::sink::NullSink) (zero cost), while the debug
//! entry points drive the same code with a recorder. Because there is a single
//! renderer, an explained pixel can never disagree with the drawn one. The caller
//! (a machine's PPU aggregate) owns the framebuffer, the latched segments, and
//! the scratch, and passes them in.

use crate::bg;
use crate::compositor;
use crate::debug::explain::ScanlineStateExplanation;
use crate::debug::provenance::{BackgroundId, RejectionReason};
use crate::debug::sink::ProvenanceSink;
use crate::effects;
use crate::memory::{PpuMemoryView, VramLayout};
use crate::obj;
use crate::priority;
use crate::state::{CandidatePixel, Color15, Framebuffer, LatchedState, LayerId, Scratch, ScanlineSegment};
use crate::window;

/// Build the structured latched-state summary for scanline `y`.
pub fn scanline_state_explanation(state: &LatchedState, y: u16) -> ScanlineStateExplanation {
    let mut active_backgrounds = Vec::new();
    for index in 0..4 {
        if state.bg_enabled(index) {
            active_backgrounds.push(match index {
                0 => BackgroundId::Bg0,
                1 => BackgroundId::Bg1,
                2 => BackgroundId::Bg2,
                _ => BackgroundId::Bg3,
            });
        }
    }
    ScanlineStateExplanation {
        y,
        video_mode: state.mode,
        forced_blank: state.forced_blank(),
        active_backgrounds,
    }
}

/// The renderer. Fills scanline `y` of `framebuffer` from the pre-latched
/// `segments`, feeding `sink` at each semantic stage. `scratch` is reused per
/// span. `segments` must be non-empty (the caller latches first).
pub fn render_scanline<S: ProvenanceSink>(
    framebuffer: &mut Framebuffer,
    segments: &[ScanlineSegment],
    scratch: &mut Scratch,
    y: u16,
    mem: &PpuMemoryView<'_>,
    layout: VramLayout,
    sink: &mut S,
) {
    let first_state = segments[0].state;
    debug_assert!(y < framebuffer.height as u16);
    sink.record_scanline(|| scanline_state_explanation(&first_state, y));

    // The active screen geometry comes from the framebuffer, so one renderer
    // serves both the GBA (240×160) and the DS (256×192).
    let width = framebuffer.width;
    let height = framebuffer.height;
    let backdrop = CandidatePixel::backdrop(mem.palette15(0));
    let row = y as usize * width;
    let count = segments.len();
    // Render each span with the register state in effect across it. A line with
    // no mid-line writes is a single full-width span, matching a plain latch.
    for i in 0..count {
        let seg = segments[i];
        let x_start = seg.x_start as usize;
        let x_end = if i + 1 < count {
            segments[i + 1].x_start as usize
        } else {
            width
        };
        if x_start >= x_end {
            continue;
        }

        // Forced blank (DISPCNT bit 7): the PPU drives white and touches no
        // VRAM/palette/OAM. Fill the span and skip the compositor entirely.
        if seg.state.forced_blank() {
            for pixel in &mut framebuffer.pixels[row + x_start..row + x_end] {
                *pixel = Color15(0x7FFF);
            }
            continue;
        }

        scratch.clear();
        bg::generate(y, &seg.state, &seg.affine, mem, width, layout, scratch, sink);
        obj::generate(y, &seg.state, mem, width, layout, scratch, sink);
        window::compute_line(y, &seg.state, &scratch.obj, width, height, &mut scratch.window);

        for x in x_start..x_end {
            let set = compositor::gather(x, scratch, backdrop, sink);
            let resolved = priority::resolve(&set);
            let effects_enabled = scratch.window.mask[x].effects;
            let effect =
                effects::apply(resolved.top, resolved.second, &seg.state.regs, effects_enabled);
            framebuffer.pixels[row + x] = effect.color;
            if sink.wants(x as u16) {
                sink.record_resolved(x as u16, || resolved.explain());
                sink.record_effect(x as u16, || effect.explain());
                // Candidates that were present but beaten on priority (neither the
                // top nor the blend operand) are explained as such.
                for candidate in set.as_slice() {
                    let layer = candidate.layer;
                    if layer != LayerId::Backdrop
                        && layer != resolved.top.layer
                        && layer != resolved.second.layer
                    {
                        sink.record_rejection(
                            x as u16,
                            layer,
                            RejectionReason::LowerPriority {
                                winner_priority: resolved.top.priority,
                            },
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod generalization_tests {
    use crate::latch;
    use crate::memory::{TestMemory, VramLayout, VRAM_BASE};
    use crate::debug::provenance::SourceProvenance;
    use crate::debug::sink::NullSink;
    use crate::registers::Registers;
    use crate::scanline::render_scanline;
    use crate::state::{AffineInternalState, Color15, Framebuffer, LatchedState, Scratch};
    use crate::TestPpu;

    /// A wide (256) framebuffer is rendered across its full width — the compositor
    /// loop and row stride follow the framebuffer, not a hardcoded 240.
    #[test]
    fn renders_full_256_wide_line() {
        let mut fb = Framebuffer::new(256, 192);
        let mut scratch = Scratch::default();
        let regs = Registers::default(); // mode 0, no BG enabled, dispcnt = 0
        let mut mem = TestMemory::new();
        mem.palette[0..2].copy_from_slice(&0x1234u16.to_le_bytes()); // backdrop

        let mut latched = LatchedState::default();
        let mut segments = Vec::new();
        latch::latch_for_scanline(&regs, &AffineInternalState::default(), &mut latched, &mut segments);
        render_scanline(&mut fb, &segments, &mut scratch, 0, &mem.view(), VramLayout::gba(), &mut NullSink);

        // Every one of the 256 columns holds the backdrop — including past 240.
        assert!((0..256).all(|x| fb.pixels[x] == Color15(0x1234)));
    }

    /// A non-zero `bg_char_base`/`bg_screen_base` shifts the sampled tilemap and
    /// tile addresses by exactly that offset (asserted on provenance).
    #[test]
    fn bg_base_offsets_shift_the_sampled_addresses() {
        let mut ppu = TestPpu::new();
        ppu.layout = VramLayout { bg_char_base: 0x8000, bg_screen_base: 0x4000, obj_tile_base: 0x1_0000 };
        ppu.write_dispcnt(0x0100); // mode 0, BG0 enabled
        ppu.registers.bgcnt[0] = 0; // char base 0, screen base 0

        let mut mem = TestMemory::new();
        // Map entry (tile 1) at the *offset* screen base; tile 1's texels at the
        // *offset* char base. Palette so texel 1 is red.
        mem.vram[0x4000..0x4002].copy_from_slice(&1u16.to_le_bytes()); // map[0] -> tile 1
        mem.vram[0x8000 + 32] = 0x11; // tile 1, 4bpp, row 0: texels 1,1
        mem.palette[2..4].copy_from_slice(&0x001Fu16.to_le_bytes()); // entry 1 = red

        let ex = ppu.explain_current_pixel(0, 0, &mem.view()).unwrap();
        let text = ex.candidates.iter().find_map(|c| match &c.provenance {
            SourceProvenance::TextBg(t) => Some(t),
            _ => None,
        }).expect("text bg candidate");
        assert_eq!(text.map_address, VRAM_BASE + 0x4000);
        assert_eq!(text.tile_address, VRAM_BASE + 0x8000 + 32);
    }

    /// A non-default `obj_tile_base` is honored (asserted on OBJ provenance).
    #[test]
    fn obj_tile_base_is_honored() {
        let mut ppu = TestPpu::new();
        ppu.layout = VramLayout { bg_char_base: 0, bg_screen_base: 0, obj_tile_base: 0x2_0000 };
        ppu.write_dispcnt((1 << 12) | (1 << 6)); // OBJ enabled, 1D mapping

        let mut mem = TestMemory::new();
        mem.vram.resize(0x2_1000, 0); // room for the relocated OBJ tiles
        // A single 8×8 4bpp sprite at (0,0), tile 0.
        mem.oam[0..2].copy_from_slice(&0u16.to_le_bytes()); // attr0: y=0
        mem.oam[2..4].copy_from_slice(&0u16.to_le_bytes()); // attr1: x=0
        mem.oam[4..6].copy_from_slice(&0u16.to_le_bytes()); // attr2: tile 0
        mem.vram[0x2_0000] = 0x11; // tile 0 at the relocated base
        mem.palette[(256 + 1) * 2..(256 + 1) * 2 + 2].copy_from_slice(&0x03E0u16.to_le_bytes());

        let ex = ppu.explain_current_pixel(0, 0, &mem.view()).unwrap();
        let obj = ex.candidates.iter().find_map(|c| match &c.provenance {
            SourceProvenance::Obj(o) => Some(o),
            _ => None,
        }).expect("obj candidate");
        assert_eq!(obj.tile_address, VRAM_BASE + 0x2_0000);
    }
}
