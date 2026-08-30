//! Window masking — reducing the window registers to a per-pixel visibility mask.
//!
//! The compositor never sees raw window-register encoding: it asks this module
//! whether each layer (and color effects) is visible at a position. Until windows
//! are implemented, every layer is visible everywhere.

use super::debug::explain::WindowExplanation;
use super::debug::provenance::WindowRegion;
use super::state::LatchedState;

/// Per-pixel visibility of each source layer and of color effects.
#[derive(Clone, Copy, Debug)]
pub struct WindowMask {
    /// Visibility of BG0..BG3.
    pub bg: [bool; 4],
    pub obj: bool,
    pub effects: bool,
}

impl WindowMask {
    /// Everything visible — the state when no window is active.
    pub fn all_visible() -> Self {
        WindowMask {
            bg: [true; 4],
            obj: true,
            effects: true,
        }
    }
}

/// The window mask at screen position `x` for the scanline governed by `state`.
/// Currently always fully visible; real WIN0/WIN1/OBJ-window precedence is not yet
/// implemented.
pub fn mask_at(_x: usize, _state: &LatchedState) -> WindowMask {
    WindowMask::all_visible()
}

impl WindowMask {
    /// The structured explanation of this mask.
    pub fn explain(&self) -> WindowExplanation {
        WindowExplanation {
            region: WindowRegion::Outside,
            layers_enabled: [self.bg[0], self.bg[1], self.bg[2], self.bg[3], self.obj],
            effects_enabled: self.effects,
        }
    }
}
