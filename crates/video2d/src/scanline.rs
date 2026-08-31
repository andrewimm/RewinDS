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
use crate::memory::PpuMemoryView;
use crate::obj;
use crate::priority;
use crate::state::{CandidatePixel, Color15, Framebuffer, LatchedState, LayerId, Scratch, ScanlineSegment, WIDTH};
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
    sink: &mut S,
) {
    let first_state = segments[0].state;
    sink.record_scanline(|| scanline_state_explanation(&first_state, y));

    let backdrop = CandidatePixel::backdrop(mem.palette15(0));
    let row = y as usize * WIDTH;
    let count = segments.len();
    // Render each span with the register state in effect across it. A line with
    // no mid-line writes is a single full-width span, matching a plain latch.
    for i in 0..count {
        let seg = segments[i];
        let x_start = seg.x_start as usize;
        let x_end = if i + 1 < count {
            segments[i + 1].x_start as usize
        } else {
            WIDTH
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
        bg::generate(y, &seg.state, &seg.affine, mem, scratch, sink);
        obj::generate(y, &seg.state, mem, scratch, sink);
        window::compute_line(y, &seg.state, &scratch.obj, &mut scratch.window);

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
