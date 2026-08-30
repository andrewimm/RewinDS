//! The aggregate explanation types returned by the debug API.
//!
//! These are built only when instrumentation is active (an `explain_pixel` /
//! `inspect_scanline` call, or a running scanline/pixel instrumentation level).
//! They use plain `Vec`/`Box` because they are off the hot path; the fast renderer
//! never constructs them.

use super::provenance::{BackgroundId, RejectionReason, SourceProvenance, WindowRegion};
use crate::ppu::state::{CandidatePixel, Color15, LayerId};

/// A frame identity, for the frame-indexed explain API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameId(pub u64);

/// Which instrumentation a running frame should collect. `Off` is the default and
/// costs nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VideoInstrumentation {
    #[default]
    Off,
    /// Collect a whole-line summary for scanline `y`.
    Scanline { y: u16 },
    /// Collect full per-candidate provenance for one pixel.
    Pixel { x: u16, y: u16 },
    /// As `Pixel`, and additionally resolve source memory addresses.
    MemorySources { x: u16, y: u16 },
}

/// Why a debug request could not be fully answered (spec §56).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplainError {
    /// The requested pixel is outside the 240×160 visible framebuffer.
    PixelOutsideFramebuffer { x: u16, y: u16 },
    /// A past frame was requested but history does not reach it (needs rewind).
    FrameUnavailable { requested: u64, oldest: u64 },
    /// The request needs deterministic replay, which is unavailable.
    ReplayUnavailable,
}

/// One candidate's explanation: what it was, where it came from, and whether it
/// survived windowing.
#[derive(Clone, Copy, Debug)]
pub struct CandidateExplanation {
    pub candidate: CandidatePixel,
    pub provenance: SourceProvenance,
    pub visible_after_window: bool,
    pub rejection_reason: Option<RejectionReason>,
}

/// How the top candidate beat the second.
#[derive(Clone, Copy, Debug)]
pub struct TieBreak {
    pub winner: LayerId,
    pub over: LayerId,
    /// `true` if the winner had a strictly lower priority value; `false` if the
    /// priorities tied and layer order decided it.
    pub by_priority: bool,
}

/// The priority-resolution outcome: the two front-most candidates.
#[derive(Clone, Copy, Debug)]
pub struct ResolvedExplanation {
    pub top: CandidatePixel,
    pub second: CandidatePixel,
    pub tie_break: TieBreak,
}

/// The color-effect mode selected by `BLDCNT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectMode {
    None,
    Alpha,
    Brighten,
    Darken,
}

/// The color effect actually applied to the final pixel (spec §31).
#[derive(Clone, Copy, Debug)]
pub enum AppliedEffect {
    None,
    Alpha {
        first: LayerId,
        second: LayerId,
        eva: u8,
        evb: u8,
    },
    Brighten {
        source: LayerId,
        evy: u8,
    },
    Darken {
        source: LayerId,
        evy: u8,
    },
}

/// The color-effects stage outcome.
#[derive(Clone, Copy, Debug)]
pub struct EffectExplanation {
    pub mode: EffectMode,
    pub applied: AppliedEffect,
    pub result: Color15,
}

/// The window decision at a screen position.
#[derive(Clone, Copy, Debug)]
pub struct WindowExplanation {
    pub region: WindowRegion,
    /// Per-layer enable in the resolved region: `[bg0, bg1, bg2, bg3, obj]`.
    pub layers_enabled: [bool; 5],
    pub effects_enabled: bool,
}

/// The latched register context for a scanline.
#[derive(Clone, Debug)]
pub struct ScanlineStateExplanation {
    pub y: u16,
    pub video_mode: u8,
    pub forced_blank: bool,
    pub active_backgrounds: Vec<BackgroundId>,
}

/// The full explanation of one output pixel (spec §36).
#[derive(Clone, Debug)]
pub struct PixelExplanation {
    pub frame: u64,
    pub x: u16,
    pub y: u16,
    pub final_color: Color15,
    pub video_mode: u8,
    pub scanline_state: ScanlineStateExplanation,
    pub window: WindowExplanation,
    pub candidates: Vec<CandidateExplanation>,
    pub resolved: ResolvedExplanation,
    pub effect: EffectExplanation,
    /// `true` when the explanation is known to be incomplete.
    pub partial: bool,
}

impl PixelExplanation {
    /// The candidate explanation contributed by `layer`, if any.
    pub fn candidate_for(&self, layer: LayerId) -> Option<&CandidateExplanation> {
        self.candidates.iter().find(|c| c.candidate.layer == layer)
    }
}

/// A whole-scanline summary — cheaper than 240 pixel explanations (spec §39).
#[derive(Clone, Debug)]
pub struct ScanlineExplanation {
    pub state: ScanlineStateExplanation,
    pub final_line: Box<[Color15]>,
}
