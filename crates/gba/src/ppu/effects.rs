//! Color effects — alpha blend, brighten, darken — applied after priority
//! resolution.
//!
//! Effects run once per pixel on the resolved top (and, for alpha, second)
//! candidate. Until color effects are implemented this is the identity: the top
//! candidate's color passes through unchanged.

use super::debug::explain::{AppliedEffect, EffectExplanation, EffectMode};
use super::registers::Registers;
use super::state::{CandidatePixel, Color15};

/// The outcome of the color-effects stage.
#[derive(Clone, Copy, Debug)]
pub struct EffectResult {
    pub color: Color15,
    pub mode: EffectMode,
    pub applied: AppliedEffect,
}

/// Apply color effects to a resolved pixel. Identity until color effects exist.
pub fn apply(top: CandidatePixel, _second: CandidatePixel, _regs: &Registers) -> EffectResult {
    EffectResult {
        color: top.color,
        mode: EffectMode::None,
        applied: AppliedEffect::None,
    }
}

impl EffectResult {
    /// The structured explanation of the applied effect.
    pub fn explain(&self) -> EffectExplanation {
        EffectExplanation {
            mode: self.mode,
            applied: self.applied,
            result: self.color,
        }
    }
}
