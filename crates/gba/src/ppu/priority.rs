//! Priority resolution — the single source of truth for which candidate wins a
//! pixel, and the tie-break when priorities are equal.
//!
//! No other module orders candidates. BG/OBJ generators only produce candidates;
//! blending only consumes the resolved top and second. Keeping the rule here means
//! there is exactly one place to test and to explain.

use super::debug::explain::{ResolvedExplanation, TieBreak};
use super::state::CandidatePixel;

/// The candidates competing for one screen position. The backdrop is always
/// present, so there is always at least one.
pub struct CandidateSet {
    items: [CandidatePixel; 6],
    len: usize,
}

impl CandidateSet {
    /// Start a set with the always-present backdrop candidate.
    pub fn new(backdrop: CandidatePixel) -> Self {
        let mut set = CandidateSet {
            items: [backdrop; 6],
            len: 0,
        };
        set.items[0] = backdrop;
        set.len = 1;
        set
    }

    /// Add an opaque candidate (a `None` transparent pixel is ignored).
    pub fn push(&mut self, candidate: Option<CandidatePixel>) {
        if let Some(candidate) = candidate {
            if self.len < self.items.len() {
                self.items[self.len] = candidate;
                self.len += 1;
            }
        }
    }

    pub fn as_slice(&self) -> &[CandidatePixel] {
        &self.items[..self.len]
    }
}

/// The two front-most candidates. `second` is needed for alpha blending.
#[derive(Clone, Copy, Debug)]
pub struct ResolvedPixel {
    pub top: CandidatePixel,
    pub second: CandidatePixel,
}

/// Lower is more on top: sort by priority, then by layer tie-break rank.
fn ordering_key(candidate: &CandidatePixel) -> (u8, u8) {
    (candidate.priority, candidate.layer.tiebreak_rank())
}

/// Resolve the candidate set to its top and second candidates.
pub fn resolve(set: &CandidateSet) -> ResolvedPixel {
    let items = set.as_slice();
    let mut top = items[0];
    let mut second = items[0];
    let mut have_second = false;
    for &candidate in &items[1..] {
        if ordering_key(&candidate) < ordering_key(&top) {
            second = top;
            have_second = true;
            top = candidate;
        } else if !have_second || ordering_key(&candidate) < ordering_key(&second) {
            second = candidate;
            have_second = true;
        }
    }
    if !have_second {
        second = top;
    }
    ResolvedPixel { top, second }
}

impl ResolvedPixel {
    /// Build the structured explanation of how `top` beat `second`.
    pub fn explain(&self) -> ResolvedExplanation {
        let by_priority = self.top.priority < self.second.priority;
        ResolvedExplanation {
            top: self.top,
            second: self.second,
            tie_break: TieBreak {
                winner: self.top.layer,
                over: self.second.layer,
                by_priority,
            },
        }
    }
}
