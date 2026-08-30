//! The observation surface the one renderer feeds at every semantic stage.
//!
//! The renderer is generic over a [`ProvenanceSink`]. On the fast path it is
//! [`NullSink`], whose methods are inlinable no-ops that drop the explanation
//! closures unbuilt — so instrumentation costs nothing when off. The same
//! renderer with [`PixelRecorder`] or [`ScanlineRecorder`] produces explanations,
//! which guarantees an explained pixel can never diverge from the drawn one.

use super::explain::{
    CandidateExplanation, EffectExplanation, PixelExplanation, ResolvedExplanation,
    ScanlineStateExplanation, WindowExplanation,
};
use super::provenance::{RejectionReason, WindowRegion};
use crate::ppu::state::{Color15, LayerId};

/// The stages the renderer reports. Heavy payloads are passed as closures so a
/// sink that ignores them pays nothing to build them.
pub trait ProvenanceSink {
    /// Whether this sink wants per-candidate detail at column `x`. The renderer
    /// skips even assembling the closures for columns that return `false`.
    fn wants(&self, _x: u16) -> bool {
        false
    }
    fn record_scanline(&mut self, _f: impl FnOnce() -> ScanlineStateExplanation) {}
    fn record_candidate(&mut self, _x: u16, _layer: LayerId, _f: impl FnOnce() -> CandidateExplanation) {}
    fn record_rejection(&mut self, _x: u16, _layer: LayerId, _reason: RejectionReason) {}
    fn record_window(&mut self, _x: u16, _f: impl FnOnce() -> WindowExplanation) {}
    fn record_resolved(&mut self, _x: u16, _f: impl FnOnce() -> ResolvedExplanation) {}
    fn record_effect(&mut self, _x: u16, _f: impl FnOnce() -> EffectExplanation) {}
}

/// The zero-cost fast path: every method is a no-op.
pub struct NullSink;
impl ProvenanceSink for NullSink {}

/// Collects full per-candidate provenance for a single target pixel.
pub struct PixelRecorder {
    target_x: u16,
    scanline: Option<ScanlineStateExplanation>,
    candidates: Vec<CandidateExplanation>,
    rejections: Vec<(LayerId, RejectionReason)>,
    window: Option<WindowExplanation>,
    resolved: Option<ResolvedExplanation>,
    effect: Option<EffectExplanation>,
}

impl PixelRecorder {
    pub fn new(x: u16) -> Self {
        PixelRecorder {
            target_x: x,
            scanline: None,
            candidates: Vec::new(),
            rejections: Vec::new(),
            window: None,
            resolved: None,
            effect: None,
        }
    }

    /// Assemble the collected pieces into a [`PixelExplanation`].
    pub fn finish(mut self, frame: u64, y: u16) -> PixelExplanation {
        // Fold any recorded rejections onto their candidate, or synthesize a
        // rejected (invisible) candidate entry so the reason is still reported.
        for (layer, reason) in self.rejections.drain(..) {
            if let Some(c) = self.candidates.iter_mut().find(|c| c.candidate.layer == layer) {
                c.rejection_reason = Some(reason);
                c.visible_after_window = false;
            }
        }

        let partial = self.scanline.is_none() || self.resolved.is_none() || self.effect.is_none();
        let scanline_state = self.scanline.unwrap_or_else(|| ScanlineStateExplanation {
            y,
            video_mode: 0,
            forced_blank: false,
            active_backgrounds: Vec::new(),
        });
        let video_mode = scanline_state.video_mode;
        let effect = self.effect.unwrap_or(EffectExplanation {
            mode: super::explain::EffectMode::None,
            applied: super::explain::AppliedEffect::None,
            result: Color15::default(),
        });
        let resolved = self.resolved.unwrap_or_else(|| {
            let backdrop = crate::ppu::state::CandidatePixel::backdrop(effect.result);
            ResolvedExplanation {
                top: backdrop,
                second: backdrop,
                tie_break: super::explain::TieBreak {
                    winner: LayerId::Backdrop,
                    over: LayerId::Backdrop,
                    by_priority: true,
                },
            }
        });
        let window = self.window.unwrap_or(WindowExplanation {
            region: WindowRegion::Outside,
            layers_enabled: [true; 5],
            effects_enabled: true,
        });

        PixelExplanation {
            frame,
            x: self.target_x,
            y,
            final_color: effect.result,
            video_mode,
            scanline_state,
            window,
            candidates: self.candidates,
            resolved,
            effect,
            partial,
        }
    }
}

impl ProvenanceSink for PixelRecorder {
    fn wants(&self, x: u16) -> bool {
        x == self.target_x
    }
    fn record_scanline(&mut self, f: impl FnOnce() -> ScanlineStateExplanation) {
        self.scanline = Some(f());
    }
    fn record_candidate(&mut self, x: u16, _layer: LayerId, f: impl FnOnce() -> CandidateExplanation) {
        if x == self.target_x {
            self.candidates.push(f());
        }
    }
    fn record_rejection(&mut self, x: u16, layer: LayerId, reason: RejectionReason) {
        if x == self.target_x {
            self.rejections.push((layer, reason));
        }
    }
    fn record_window(&mut self, x: u16, f: impl FnOnce() -> WindowExplanation) {
        if x == self.target_x {
            self.window = Some(f());
        }
    }
    fn record_resolved(&mut self, x: u16, f: impl FnOnce() -> ResolvedExplanation) {
        if x == self.target_x {
            self.resolved = Some(f());
        }
    }
    fn record_effect(&mut self, x: u16, f: impl FnOnce() -> EffectExplanation) {
        if x == self.target_x {
            self.effect = Some(f());
        }
    }
}

/// Collects a whole-scanline summary (the latched state). The final line of pixels
/// is copied from the framebuffer by the caller after rendering.
pub struct ScanlineRecorder {
    state: Option<ScanlineStateExplanation>,
}

impl ScanlineRecorder {
    pub fn new() -> Self {
        ScanlineRecorder { state: None }
    }

    /// The captured latched state, if the scanline was rendered.
    pub fn take_state(self) -> Option<ScanlineStateExplanation> {
        self.state
    }
}

impl Default for ScanlineRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl ProvenanceSink for ScanlineRecorder {
    fn record_scanline(&mut self, f: impl FnOnce() -> ScanlineStateExplanation) {
        self.state = Some(f());
    }
}
