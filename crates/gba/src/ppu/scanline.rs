//! The aggregate `Ppu`'s render entry points: a thin delegator to the shared
//! [`video2d::render_scanline`] pass, plus the debug entry points (`explain`,
//! `inspect`) that re-run it with a recorder. Because there is a single renderer,
//! an explained pixel can never disagree with the drawn one.

use super::debug::explain::{ExplainError, FrameId, PixelExplanation, ScanlineExplanation};
use super::debug::sink::{NullSink, PixelRecorder, ProvenanceSink, ScanlineRecorder};
use super::memory::PpuMemoryView;
use super::state::{HEIGHT, WIDTH};
use super::Ppu;

impl Ppu {
    /// The renderer: latch if needed, then fill scanline `y` via the shared pass.
    pub(crate) fn render_scanline<S: ProvenanceSink>(
        &mut self,
        y: u16,
        mem: &PpuMemoryView<'_>,
        sink: &mut S,
    ) {
        if self.segments.is_empty() {
            self.latch_for_scanline();
        }
        video2d::render_scanline(
            &mut self.framebuffer,
            &self.segments,
            &mut self.scratch,
            y,
            mem,
            video2d::VramLayout::gba(),
            None, // the GBA has no 3D engine
            sink,
        );
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
            .unwrap_or_else(|| video2d::scanline_state_explanation(&self.latched, y));
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
