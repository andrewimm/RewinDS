//! Test-only glue mirroring the small slice of a machine PPU aggregate that the
//! renderer's unit tests drive, so those tests keep their original shape after
//! the extraction (they used to run against the GBA `Ppu`).

use crate::latch;
use crate::memory::PpuMemoryView;
use crate::registers::Registers;
use crate::scanline;
use crate::state::{
    AffineInternalState, AffineReference, Color15, Framebuffer, LatchedState, Scratch,
    ScanlineSegment, HEIGHT, WIDTH,
};
use crate::debug::explain::{ExplainError, PixelExplanation};
use crate::debug::sink::{PixelRecorder, ProvenanceSink};

/// A minimal stand-in for a machine's PPU aggregate: the render-relevant state
/// plus the latch/render operations the tests call.
#[derive(Default)]
pub struct TestPpu {
    pub registers: Registers,
    pub affine: AffineInternalState,
    pub latched: LatchedState,
    pub framebuffer: Framebuffer,
    pub scratch: Scratch,
    pub segments: Vec<ScanlineSegment>,
    /// The VRAM base layout (defaults to the GBA's); tests set it to exercise the
    /// DS-style base offsets.
    pub layout: crate::memory::VramLayout,
}

impl TestPpu {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write_dispcnt(&mut self, value: u16) {
        self.registers.dispcnt = value;
    }

    pub fn latch_for_scanline(&mut self) {
        latch::latch_for_scanline(
            &self.registers,
            &self.affine,
            &mut self.latched,
            &mut self.segments,
        );
    }

    pub fn affine_reference_for_line(&self, k: usize, y: u16) -> AffineReference {
        latch::affine_reference_for_line(&self.registers, k, y)
    }

    pub fn render_scanline<S: ProvenanceSink>(
        &mut self,
        y: u16,
        mem: &PpuMemoryView<'_>,
        sink: &mut S,
    ) {
        if self.segments.is_empty() {
            self.latch_for_scanline();
        }
        scanline::render_scanline(
            &mut self.framebuffer,
            &self.segments,
            &mut self.scratch,
            y,
            mem,
            self.layout,
            sink,
        );
    }

    pub fn framebuffer(&self) -> &[Color15] {
        &self.framebuffer.pixels
    }

    pub fn explain_current_pixel(
        &mut self,
        x: u16,
        y: u16,
        mem: &PpuMemoryView<'_>,
    ) -> Result<PixelExplanation, ExplainError> {
        if x as usize >= WIDTH || y as usize >= HEIGHT {
            return Err(ExplainError::PixelOutsideFramebuffer { x, y });
        }
        self.affine.bg2 = self.affine_reference_for_line(0, y);
        self.affine.bg3 = self.affine_reference_for_line(1, y);
        self.latch_for_scanline();
        let mut recorder = PixelRecorder::new(x);
        self.render_scanline(y, mem, &mut recorder);
        Ok(recorder.finish(0, y))
    }
}
