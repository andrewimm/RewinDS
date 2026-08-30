//! Per-pixel candidate gathering: collect the visible candidates for a screen
//! position from the per-layer scratch buffers plus the backdrop, ready for
//! priority resolution.

use super::debug::explain::CandidateExplanation;
use super::debug::provenance::{BackdropProvenance, SourceProvenance};
use super::debug::sink::ProvenanceSink;
use super::memory::PALETTE_BASE;
use super::priority::CandidateSet;
use super::state::{CandidatePixel, LatchedState, LayerId, Scratch};
use super::window;

/// Gather the candidates competing for pixel `x`, applying window visibility. The
/// backdrop is always included as the fallback.
pub fn gather<S: ProvenanceSink>(
    x: usize,
    state: &LatchedState,
    scratch: &Scratch,
    backdrop: CandidatePixel,
    sink: &mut S,
) -> CandidateSet {
    let mask = window::mask_at(x, state);
    let mut set = CandidateSet::new(backdrop);
    for bg in 0..4 {
        if mask.bg[bg] {
            set.push(scratch.bg[bg].pixels[x]);
        }
    }
    // OBJ candidates will join here once sprites are implemented.

    if sink.wants(x as u16) {
        sink.record_window(x as u16, || mask.explain());
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
