//! The GBA picture processing unit: timing, the video register block, register
//! latching, and the software scanline renderer.
//!
//! The aggregate [`Ppu`] owns the CPU-visible register state, the snapshot that
//! governs the line being drawn, the internal affine reference state, the output
//! framebuffer, and reused per-scanline scratch. Timing lives in
//! [`timing::TimingState`]; the renderer and its debug entry points are in
//! [`scanline`]. The renderer is driven from [`crate::machine`] at each visible
//! line's HBlank via [`crate::bus::Bus::render_ppu_scanline`].

pub mod bg;
pub mod compositor;
pub mod debug;
pub mod effects;
pub mod latch;
pub mod memory;
pub mod priority;
pub mod registers;
pub mod scanline;
pub mod state;
pub mod timing;
pub mod window;

use crate::event::{EventKind, PpuEvent};
use crate::interrupt::InterruptController;
use emu_core::{EventContext, Scheduler, Timestamp};

pub use debug::{PixelExplanation, ScanlineExplanation, VideoInstrumentation};
pub use state::{Color15, LayerId, HEIGHT, WIDTH};

/// The picture processing unit.
#[derive(Clone, Debug, Default)]
pub struct Ppu {
    pub timing: timing::TimingState,
    pub registers: registers::Registers,
    pub latched: state::LatchedState,
    pub affine: state::AffineInternalState,
    pub framebuffer: state::Framebuffer,
    scratch: state::Scratch,
    frame_counter: u64,
}

impl Ppu {
    pub fn new() -> Self {
        Self::default()
    }

    // --- timing delegators (unchanged external surface) ---

    /// The current scanline (VCOUNT), 0..=227.
    pub fn vcount(&self) -> u16 {
        self.timing.vcount()
    }

    pub fn vblank_flag(&self) -> bool {
        self.timing.vblank_flag()
    }

    pub fn hblank_flag(&self) -> bool {
        self.timing.hblank_flag()
    }

    pub fn vcount_match(&self) -> bool {
        self.timing.vcount_match()
    }

    pub fn read_dispstat(&self) -> u16 {
        self.timing.read_dispstat()
    }

    pub fn write_dispstat(&mut self, value: u16) {
        self.timing.write_dispstat(value);
    }

    pub fn read_vcount(&self) -> u16 {
        self.timing.read_vcount()
    }

    // --- DISPCNT and the rest of the video register block ---

    /// Read `DISPCNT` (`4000000h`).
    pub fn read_dispcnt(&self) -> u16 {
        self.registers.dispcnt
    }

    /// Write `DISPCNT`.
    pub fn write_dispcnt(&mut self, value: u16) {
        self.registers.dispcnt = value;
    }

    /// Read a video register in the `0x008..=0x054` block.
    pub fn read_video_register(&self, offset: u32) -> u16 {
        self.registers.read16(offset)
    }

    /// Apply a masked write to a video register in the `0x008..=0x054` block.
    pub fn write_video_register(&mut self, offset: u32, value: u16, mask: u16) {
        self.registers.write16(offset, value, mask);
    }

    /// Whether the display is force-blanked (`DISPCNT` bit 7), during which the
    /// PPU does not access video memory.
    pub fn forced_blank(&self) -> bool {
        self.registers.dispcnt & (1 << 7) != 0
    }

    /// Whether the PPU is actively drawing and thus contending for video memory:
    /// a visible scanline, outside HBlank, with the display enabled. A CPU or DMA
    /// access to VRAM/palette/OAM during this window costs one extra cycle.
    pub fn is_rendering(&self) -> bool {
        (self.vcount() as usize) < HEIGHT && !self.hblank_flag() && !self.forced_blank()
    }

    // --- framebuffer / frame accounting ---

    /// The current output image, in canonical BGR555.
    pub fn framebuffer(&self) -> &[Color15] {
        &self.framebuffer.pixels
    }

    /// The number of frames completed so far.
    pub fn frame(&self) -> u64 {
        self.frame_counter
    }

    /// Mark the end of a frame (called at the start of VBlank).
    pub fn end_frame(&mut self) {
        self.frame_counter = self.frame_counter.wrapping_add(1);
    }

    // --- timing lifecycle ---

    /// Begin LCD timing at `now`, latching scanline 0's registers.
    pub fn start(&mut self, now: Timestamp, scheduler: &mut Scheduler<EventKind>) {
        self.timing.start(now, scheduler);
        self.latch_for_scanline();
    }

    /// Dispatch a PPU timing event, latching registers when a visible line begins.
    pub fn handle_event(
        &mut self,
        event: PpuEvent,
        irq: &mut InterruptController,
        ctx: &mut EventContext<'_, EventKind>,
    ) {
        self.timing.handle_event(event, irq, ctx);
        if matches!(event, PpuEvent::LineStart) && (self.vcount() as usize) < HEIGHT {
            self.latch_for_scanline();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::memory::{PpuMemoryView, PALETTE_BASE, VRAM_BASE};
    use super::state::Color15;
    use super::*;
    use crate::bus::Memory;
    use crate::ppu::debug::provenance::SourceProvenance;

    /// A palette-0 backdrop color and a rendered line should be that color when no
    /// background is enabled.
    #[test]
    fn backdrop_fills_the_line_when_no_bg_is_enabled() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        // Backdrop = palette entry 0 = red (0x001F).
        mem.palette[0..2].copy_from_slice(&0x001Fu16.to_le_bytes());
        ppu.latch_for_scanline();

        let view = PpuMemoryView::new(&mem);
        ppu.render_scanline(0, &view, &mut super::debug::sink::NullSink);
        assert_eq!(ppu.framebuffer()[0], Color15(0x001F));
        assert_eq!(ppu.framebuffer()[239], Color15(0x001F));
    }

    /// Mode 3 samples direct color from VRAM into BG2.
    #[test]
    fn mode3_samples_direct_color_from_vram() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        // Enable Mode 3 with BG2 on.
        ppu.write_dispcnt(0x0003 | (1 << 10));
        // Pixel (1, 0): byte offset (0*240 + 1) * 2 = 2. Green (0x03E0).
        let off = 2;
        mem.vram[off..off + 2].copy_from_slice(&0x03E0u16.to_le_bytes());
        ppu.latch_for_scanline();

        let view = PpuMemoryView::new(&mem);
        ppu.render_scanline(0, &view, &mut super::debug::sink::NullSink);
        assert_eq!(ppu.framebuffer()[1], Color15(0x03E0));
    }

    /// The explain path reports the exact VRAM source address for a Mode 3 pixel.
    #[test]
    fn explain_reports_mode3_source_address() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt(0x0003 | (1 << 10));
        let off = (5 * WIDTH + 10) * 2;
        mem.vram[off..off + 2].copy_from_slice(&0x7FFFu16.to_le_bytes());

        let view = PpuMemoryView::new(&mem);
        let explanation = ppu.explain_current_pixel(10, 5, &view).unwrap();
        assert_eq!(explanation.final_color, Color15(0x7FFF));
        assert_eq!(explanation.video_mode, 3);

        let bg2 = explanation.candidate_for(LayerId::Bg2).expect("BG2 candidate");
        match bg2.provenance {
            SourceProvenance::BitmapBg(p) => {
                assert_eq!(p.vram_address, VRAM_BASE + off as u32);
                assert_eq!(p.resolved, Color15(0x7FFF));
            }
            _ => panic!("expected bitmap provenance"),
        }
        // The backdrop is also a candidate, lower priority.
        let backdrop = explanation
            .candidate_for(LayerId::Backdrop)
            .expect("backdrop candidate");
        match backdrop.provenance {
            SourceProvenance::Backdrop(p) => assert_eq!(p.palette_address, PALETTE_BASE),
            _ => panic!("expected backdrop provenance"),
        }
    }

    /// Mode 4 looks up an 8-bit index in the background palette; index 0 is
    /// transparent and shows the backdrop.
    #[test]
    fn mode4_indexed_lookup_and_transparency() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt(0x0004 | (1 << 10));
        mem.palette[0..2].copy_from_slice(&0x7C00u16.to_le_bytes()); // backdrop = blue
        mem.palette[10..12].copy_from_slice(&0x03E0u16.to_le_bytes()); // index 5 = green
        mem.vram[0] = 5; // pixel (0,0) -> index 5
        mem.vram[1] = 0; // pixel (1,0) -> transparent
        ppu.latch_for_scanline();

        let view = PpuMemoryView::new(&mem);
        ppu.render_scanline(0, &view, &mut super::debug::sink::NullSink);
        assert_eq!(ppu.framebuffer()[0], Color15(0x03E0));
        assert_eq!(ppu.framebuffer()[1], Color15(0x7C00)); // transparent -> backdrop
    }

    /// Mode 4's `DISPCNT` bit 4 selects the second VRAM page.
    #[test]
    fn mode4_frame_select_reads_second_page() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt(0x0004 | (1 << 10) | (1 << 4)); // frame 1
        mem.palette[2..4].copy_from_slice(&0x001Fu16.to_le_bytes()); // index 1 = red
        mem.vram[0xA000] = 1; // frame 1, pixel (0,0)
        mem.vram[0] = 7; // frame 0 value, must be ignored
        ppu.latch_for_scanline();

        let view = PpuMemoryView::new(&mem);
        ppu.render_scanline(0, &view, &mut super::debug::sink::NullSink);
        assert_eq!(ppu.framebuffer()[0], Color15(0x001F));
    }

    /// Mode 5 is direct color but only 160×128; outside those bounds is backdrop.
    #[test]
    fn mode5_direct_color_within_reduced_dimensions() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt(0x0005 | (1 << 10));
        mem.palette[0..2].copy_from_slice(&0x7C00u16.to_le_bytes()); // backdrop = blue
        mem.vram[0..2].copy_from_slice(&0x03E0u16.to_le_bytes()); // pixel (0,0) = green
        ppu.latch_for_scanline();

        let view = PpuMemoryView::new(&mem);
        ppu.render_scanline(0, &view, &mut super::debug::sink::NullSink);
        assert_eq!(ppu.framebuffer()[0], Color15(0x03E0));
        // x = 200 is beyond mode 5's 160-wide framebuffer -> backdrop.
        assert_eq!(ppu.framebuffer()[200], Color15(0x7C00));

        // A line beyond mode 5's 128-tall framebuffer is entirely backdrop.
        ppu.render_scanline(130, &view, &mut super::debug::sink::NullSink);
        assert_eq!(ppu.framebuffer()[130 * WIDTH], Color15(0x7C00));
    }

    /// Mode 4's explanation reports both the VRAM index address and the palette
    /// entry it resolved through.
    #[test]
    fn explain_reports_mode4_palette_source() {
        use crate::ppu::debug::provenance::BitmapSample;
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt(0x0004 | (1 << 10));
        mem.palette[20..22].copy_from_slice(&0x7FFFu16.to_le_bytes()); // index 10 = white
        let off = 3 * WIDTH + 4; // pixel (4, 3)
        mem.vram[off] = 10;

        let view = PpuMemoryView::new(&mem);
        let explanation = ppu.explain_current_pixel(4, 3, &view).unwrap();
        assert_eq!(explanation.final_color, Color15(0x7FFF));
        assert_eq!(explanation.video_mode, 4);

        let bg2 = explanation.candidate_for(LayerId::Bg2).expect("BG2 candidate");
        match bg2.provenance {
            SourceProvenance::BitmapBg(p) => {
                assert_eq!(p.vram_address, VRAM_BASE + off as u32);
                assert_eq!(p.palette_address, Some(PALETTE_BASE + 10 * 2));
                assert!(matches!(p.raw, BitmapSample::Indexed(10)));
            }
            _ => panic!("expected bitmap provenance"),
        }
    }

    /// A pixel outside the visible framebuffer is an explicit error.
    #[test]
    fn explain_rejects_out_of_range_pixel() {
        let mut ppu = Ppu::new();
        let mem = Memory::default();
        let view = PpuMemoryView::new(&mem);
        assert!(matches!(
            ppu.explain_current_pixel(240, 0, &view),
            Err(debug::ExplainError::PixelOutsideFramebuffer { .. })
        ));
    }
}
