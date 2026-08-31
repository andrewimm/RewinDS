//! The PPU's structured debug surface: pixel provenance, explanations, and the
//! instrumentation sink that lets one renderer answer "why is this pixel this
//! color?" without a separate debug renderer.

pub mod explain;
pub mod provenance;
pub mod sink;

pub use explain::{
    AppliedEffect, CandidateExplanation, EffectExplanation, EffectMode, ExplainError, FrameId,
    PixelExplanation, ResolvedExplanation, ScanlineExplanation, ScanlineStateExplanation, TieBreak,
    VideoInstrumentation, WindowExplanation,
};
pub use provenance::{
    AffineBgProvenance, AffineMatrix, AffineWrap, BackdropProvenance, BackgroundId, BgColorMode,
    BitmapBgProvenance, BitmapSample, ObjColorMode, ObjMode, ObjProvenance, RejectionReason,
    SourceProvenance, TextBgProvenance, WindowRegion,
};
pub use sink::{NullSink, PixelRecorder, ProvenanceSink, ScanlineRecorder};

use crate::state::LayerId;

/// A short, stable name for a layer, for text rendering of explanations.
pub fn layer_name(layer: LayerId) -> &'static str {
    match layer {
        LayerId::Bg0 => "BG0",
        LayerId::Bg1 => "BG1",
        LayerId::Bg2 => "BG2",
        LayerId::Bg3 => "BG3",
        LayerId::Obj => "OBJ",
        LayerId::Backdrop => "backdrop",
    }
}
