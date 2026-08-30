//! The one scanline renderer, and the debug entry points that re-run it.
//!
//! `render_scanline` is generic over a [`ProvenanceSink`]: the frame loop drives
//! it with [`NullSink`] (zero cost), while `explain`/`inspect` drive the same code
//! with a recorder. Because there is a single renderer, an explained pixel can
//! never disagree with the drawn one.

use super::bg;
use super::compositor;
use super::obj;
use super::window;
use super::debug::explain::{
    ExplainError, FrameId, PixelExplanation, ScanlineExplanation, ScanlineStateExplanation,
};
use super::debug::provenance::{BackgroundId, RejectionReason};
use super::debug::sink::{NullSink, PixelRecorder, ProvenanceSink, ScanlineRecorder};
use super::effects;
use super::memory::PpuMemoryView;
use super::priority;
use super::state::{CandidatePixel, LatchedState, LayerId, HEIGHT, WIDTH};
use super::Ppu;

/// Build the structured latched-state summary for scanline `y`.
fn scanline_state_explanation(state: &LatchedState, y: u16) -> ScanlineStateExplanation {
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

impl Ppu {
    /// The renderer. Fills scanline `y` of the framebuffer and feeds `sink` at each
    /// semantic stage.
    pub(crate) fn render_scanline<S: ProvenanceSink>(
        &mut self,
        y: u16,
        mem: &PpuMemoryView<'_>,
        sink: &mut S,
    ) {
        if self.segments.is_empty() {
            self.latch_for_scanline();
        }
        let first_state = self.segments[0].state;
        sink.record_scanline(|| scanline_state_explanation(&first_state, y));

        let backdrop = CandidatePixel::backdrop(mem.palette15(0));
        let row = y as usize * WIDTH;
        let count = self.segments.len();
        // Render each span with the register state in effect across it. A line with
        // no mid-line writes is a single full-width span, matching a plain latch.
        for i in 0..count {
            let seg = self.segments[i];
            let x_start = seg.x_start as usize;
            let x_end = if i + 1 < count {
                self.segments[i + 1].x_start as usize
            } else {
                WIDTH
            };
            if x_start >= x_end {
                continue;
            }

            self.scratch.clear();
            bg::generate(y, &seg.state, &seg.affine, mem, &mut self.scratch, sink);
            obj::generate(y, &seg.state, mem, &mut self.scratch, sink);
            window::compute_line(y, &seg.state, &self.scratch.obj, &mut self.scratch.window);

            for x in x_start..x_end {
                let set = compositor::gather(x, &self.scratch, backdrop, sink);
                let resolved = priority::resolve(&set);
                let effects_enabled = self.scratch.window.mask[x].effects;
                let effect =
                    effects::apply(resolved.top, resolved.second, &seg.state.regs, effects_enabled);
                self.framebuffer.pixels[row + x] = effect.color;
                if sink.wants(x as u16) {
                    sink.record_resolved(x as u16, || resolved.explain());
                    sink.record_effect(x as u16, || effect.explain());
                    // Candidates that were present but beaten on priority (neither
                    // the top nor the blend operand) are explained as such.
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

    /// The normal frame-loop entry: draw the current visible scanline with no
    /// instrumentation.
    pub(crate) fn render_current_scanline(&mut self, mem: &PpuMemoryView<'_>) {
        let y = self.timing.vcount();
        if (y as usize) < HEIGHT {
            self.render_scanline(y, mem, &mut NullSink);
        }
    }

    /// Explain how the pixel at `(x, y)` in the current state came to be its color,
    /// by re-running the renderer for scanline `y` with a per-pixel recorder.
    pub fn explain_current_pixel(
        &mut self,
        x: u16,
        y: u16,
        mem: &PpuMemoryView<'_>,
    ) -> Result<PixelExplanation, ExplainError> {
        if x as usize >= WIDTH || y as usize >= HEIGHT {
            return Err(ExplainError::PixelOutsideFramebuffer { x, y });
        }
        // Set the affine reference for line `y` before latching, so the single
        // reconstructed segment captures it.
        self.affine.bg2 = self.affine_reference_for_line(0, y);
        self.affine.bg3 = self.affine_reference_for_line(1, y);
        self.latch_for_scanline();
        let mut recorder = PixelRecorder::new(x);
        self.render_scanline(y, mem, &mut recorder);
        Ok(recorder.finish(self.frame_counter, y))
    }

    /// Frame-indexed explanation. Only the current frame is available until rewind
    /// exists; any other frame returns [`ExplainError::FrameUnavailable`].
    pub fn explain_pixel(
        &mut self,
        frame: FrameId,
        x: u16,
        y: u16,
        mem: &PpuMemoryView<'_>,
    ) -> Result<PixelExplanation, ExplainError> {
        if frame.0 != self.frame_counter {
            return Err(ExplainError::FrameUnavailable {
                requested: frame.0,
                oldest: self.frame_counter,
            });
        }
        self.explain_current_pixel(x, y, mem)
    }

    /// A whole-scanline summary — the latched state plus the final line of pixels.
    pub fn inspect_scanline(&mut self, y: u16, mem: &PpuMemoryView<'_>) -> ScanlineExplanation {
        let y = y.min(HEIGHT as u16 - 1);
        self.affine.bg2 = self.affine_reference_for_line(0, y);
        self.affine.bg3 = self.affine_reference_for_line(1, y);
        self.latch_for_scanline();
        let mut recorder = ScanlineRecorder::new();
        self.render_scanline(y, mem, &mut recorder);
        let state = recorder
            .take_state()
            .unwrap_or_else(|| scanline_state_explanation(&self.latched, y));
        let sprites = self.scratch.sprites.clone();
        let row = y as usize * WIDTH;
        let final_line = self.framebuffer.pixels[row..row + WIDTH]
            .to_vec()
            .into_boxed_slice();
        ScanlineExplanation {
            state,
            sprites,
            final_line,
        }
    }
}
