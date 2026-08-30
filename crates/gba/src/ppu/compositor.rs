//! Per-pixel candidate gathering: collect the window-visible candidates for a
//! screen position from the per-layer scratch buffers plus the backdrop, ready for
//! priority resolution.

use super::debug::explain::{CandidateExplanation, WindowExplanation};
use super::debug::provenance::{BackdropProvenance, RejectionReason, SourceProvenance};
use super::debug::sink::ProvenanceSink;
use super::memory::PALETTE_BASE;
use super::priority::CandidateSet;
use super::state::{CandidatePixel, LayerId, Scratch};

/// Gather the candidates competing for pixel `x`, applying window visibility. The
/// backdrop is always included as the fallback.
pub fn gather<S: ProvenanceSink>(
    x: usize,
    scratch: &Scratch,
    backdrop: CandidatePixel,
    sink: &mut S,
) -> CandidateSet {
    let mask = scratch.window.mask[x];
    let region = scratch.window.region[x];
    let wants = sink.wants(x as u16);

    let mut set = CandidateSet::new(backdrop);
    for bg in 0..4 {
        if mask.bg[bg] {
            set.push(scratch.bg[bg].pixels[x]);
        } else if wants && scratch.bg[bg].pixels[x].is_some() {
            sink.record_rejection(x as u16, LayerId::bg(bg), RejectionReason::WindowHidden { region });
        }
    }
    if mask.obj {
        set.push(scratch.obj.pixels[x]);
    } else if wants && scratch.obj.pixels[x].is_some() {
        sink.record_rejection(x as u16, LayerId::Obj, RejectionReason::WindowHidden { region });
    }

    if wants {
        sink.record_window(x as u16, || WindowExplanation {
            region,
            layers_enabled: [mask.bg[0], mask.bg[1], mask.bg[2], mask.bg[3], mask.obj],
            effects_enabled: mask.effects,
        });
        sink.record_candidate(x as u16, LayerId::Backdrop, || CandidateExplanation {
            candidate: backdrop,
            provenance: SourceProvenance::Backdrop(BackdropProvenance {
                palette_address: PALETTE_BASE,
                raw_color: backdrop.color,
            }),
            visible_after_window: true,
            rejection_reason: None,
        });
    }
    set
}
