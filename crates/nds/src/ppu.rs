//! The DS 2D video: shared scanline timing plus the two 2D engines (A and B).
//!
//! One video timeline drives VCOUNT, the H/V-blank flags, and the scanline schedule,
//! raising the V-blank, H-blank, and V-count interrupts on **both** cores (each core
//! has its own `DISPSTAT` enables). The DS runs its dot clock at 5.585664 MHz =
//! 33.513982 MHz / 6, and the master tick is one ARM9 cycle (~67 MHz), so each dot
//! spans 12 master ticks.
//!
//! Each frame both engines render into their own BGR555 framebuffer through the
//! shared [`video2d`] renderer. Engine A (main) reads BG/OBJ VRAM from `0x0600_0000`/
//! `0x0640_0000` and can also present an "LCDC" VRAM block directly; Engine B (sub)
//! reads from `0x0620_0000`/`0x0660_0000` and only supports off/graphics. `POWCNT1`
//! bit 15 selects which engine drives the top screen.

use emu_core::{EventContext, Scheduler, Timestamp};

use crate::interrupt::{Interrupts, IrqSource};
use crate::system::NdsEvent;
use crate::vram::Vram;

pub const WIDTH: usize = 256;
pub const HEIGHT: usize = 192;

/// Pack the 3D engine's 6-bit-per-channel RGB into the 2D engines' BGR555.
fn pack_bgr555(c: [u8; 3]) -> u16 {
    ((c[0] as u16) >> 1) | (((c[1] as u16) >> 1) << 5) | (((c[2] as u16) >> 1) << 10)
}

/// Offset of the OBJ region inside an engine's flat VRAM view (after the 512 KB BG
/// region); also the renderer's `obj_tile_base`.
const OBJ_VIEW_BASE: u32 = 0x8_0000;
/// Size of an engine's flat view: 512 KB BG + 256 KB OBJ.
const VRAM_VIEW_SIZE: usize = 0xC_0000;

/// Total scanlines per frame (192 visible + 71 blank).
const TOTAL_LINES: u16 = 263;
/// Master ticks per dot (67.03 MHz / 5.585664 MHz).
const MASTER_PER_DOT: Timestamp = 12;
/// Dots per scanline (256 visible + 99 blank).
const DOTS_PER_LINE: Timestamp = 355;
/// Master ticks per scanline.
pub const CYCLES_PER_LINE: Timestamp = DOTS_PER_LINE * MASTER_PER_DOT;
/// H-blank begins after the 256 visible dots.
const HBLANK_START: Timestamp = 256 * MASTER_PER_DOT;

/// The two events the PPU schedules each scanline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PpuEvent {
    LineStart,
    HBlank,
}

/// One 2D engine: its `DISPCNT`, the shared 2D register block, a reusable flat VRAM
/// view, the renderer's working state, and the output framebuffer.
struct Engine {
    /// Sub-engine (B) vs main (A): selects the VRAM regions and the limited display
    /// modes, and drops the DISPCNT char/screen base bits.
    is_b: bool,
    dispcnt: u32,
    registers: video2d::Registers,
    framebuffer: Vec<u16>,
    render_fb: video2d::state::Framebuffer,
    latched: video2d::state::LatchedState,
    affine: video2d::state::AffineInternalState,
    segments: Vec<video2d::state::ScanlineSegment>,
    scratch: video2d::state::Scratch,
    /// The Engine's flat VRAM view: BG region at offset 0, OBJ region at
    /// [`OBJ_VIEW_BASE`].
    vram_view: Vec<u8>,
    /// Assembled extended palettes: BG (32 KB = 4 slots × 8 KB) and OBJ (8 KB),
    /// consulted for 8bpp layers when `DISPCNT` bits 30/31 enable them.
    bg_ext: Vec<u8>,
    obj_ext: Vec<u8>,
}

impl Engine {
    fn new(is_b: bool) -> Self {
        Engine {
            is_b,
            dispcnt: 0,
            registers: video2d::Registers::default(),
            framebuffer: vec![0; WIDTH * HEIGHT],
            render_fb: video2d::state::Framebuffer::new(WIDTH, HEIGHT),
            latched: video2d::state::LatchedState::default(),
            affine: video2d::state::AffineInternalState::default(),
            segments: Vec::new(),
            scratch: video2d::state::Scratch::default(),
            vram_view: vec![0; VRAM_VIEW_SIZE],
            bg_ext: vec![0; 0x8000],
            obj_ext: vec![0; 0x2000],
        }
    }

    fn write_dispcnt(&mut self, value: u32, bytes: u32) {
        if bytes == 4 {
            self.dispcnt = value;
        } else {
            self.dispcnt = (self.dispcnt & !0xFFFF) | (value & 0xFFFF);
        }
        // Mirror the low half into the shared snapshot. The DS shares the GBA's low
        // DISPCNT bits (BG mode, per-layer enables) EXCEPT the OBJ tile-mapping bit:
        // DS = bit 4, the renderer expects the GBA's bit 6. Translate it.
        let low = self.dispcnt as u16;
        let obj_1d = (low >> 4) & 1;
        self.registers.dispcnt = (low & !(1 << 6)) | (obj_1d << 6);
    }

    /// Render this engine's frame from its display mode (`DISPCNT` bits 16-17).
    fn render(
        &mut self,
        vram: &Vram,
        palette: &[u8],
        oam: &[u8],
        three_d: Option<&gpu3d::raster::Framebuffer3d>,
    ) {
        match (self.dispcnt >> 16) & 3 {
            // Graphics: composite the BG/OBJ layers through the shared renderer.
            1 => self.render_graphics(vram, palette, oam, three_d),
            // "VRAM display": Engine A only — blit an LCDC VRAM block (A–D) as a bitmap.
            2 if !self.is_b => {
                let block = ((self.dispcnt >> 18) & 3) as usize;
                for (i, px) in self.framebuffer.iter_mut().enumerate() {
                    *px = vram.block_read16(block, i * 2);
                }
            }
            // Display off, or main-memory FIFO (unmodelled): black.
            _ => self.framebuffer.fill(0),
        }
    }

    /// Composite the BG/OBJ layers through the shared `video2d` renderer: assemble a
    /// flat view of this engine's banked BG and OBJ VRAM, derive its base offsets, and
    /// draw every visible line.
    fn render_graphics(
        &mut self,
        vram: &Vram,
        palette: &[u8],
        oam: &[u8],
        three_d: Option<&gpu3d::raster::Framebuffer3d>,
    ) {
        let (bg_view, obj_view) = self.vram_view.split_at_mut(OBJ_VIEW_BASE as usize);
        if self.is_b {
            vram.assemble_engine_b_bg(bg_view);
            vram.assemble_engine_b_obj(obj_view);
            vram.assemble_bg_ext_b(&mut self.bg_ext);
            vram.assemble_obj_ext_b(&mut self.obj_ext);
        } else {
            vram.assemble_engine_a_bg(bg_view);
            vram.assemble_engine_a_obj(obj_view);
            vram.assemble_bg_ext_a(&mut self.bg_ext);
            vram.assemble_obj_ext_a(&mut self.obj_ext);
        }

        // Engine A adds a global 64 KB-step char/screen base from DISPCNT (bits 24-26
        // char, 27-29 screen); Engine B has none. OBJ tiles live in the OBJ half of
        // the flat view, with the DS tile-OBJ 1D boundary from DISPCNT bits 20-21.
        let (char_base, screen_base) = if self.is_b {
            (0, 0)
        } else {
            (
                ((self.dispcnt >> 24) & 7) * 0x1_0000,
                ((self.dispcnt >> 27) & 7) * 0x1_0000,
            )
        };
        let layout = video2d::VramLayout {
            bg_char_base: char_base,
            bg_screen_base: screen_base,
            obj_tile_base: OBJ_VIEW_BASE,
            obj_tile_boundary: 32 << ((self.dispcnt >> 20) & 3),
            // Extended-palette enables: DISPCNT bit 30 (BG), bit 31 (OBJ).
            bg_ext_palette: self.dispcnt & (1 << 30) != 0,
            obj_ext_palette: self.dispcnt & (1 << 31) != 0,
            // Bitmap-OBJ mapping (DISPCNT bit 6 = 1D/2D, bit 5 = 256-dot width, bit 22 =
            // 256-byte 1D boundary — Engine A only; Engine B's 1D boundary is fixed at 128).
            obj_bitmap_1d: self.dispcnt & (1 << 6) != 0,
            obj_bitmap_boundary: if !self.is_b && self.dispcnt & (1 << 22) != 0 { 256 } else { 128 },
            obj_bitmap_wide: self.dispcnt & (1 << 5) != 0,
            // The DS character-base field is 4 bits (BGxCNT bits 2-5).
            bg_char_base_mask: 0xF,
            mode_semantics: video2d::ModeSemantics::Ds,
        };
        let mem = video2d::PpuMemoryView::new(&self.vram_view, palette, oam)
            .with_ext_palettes(&self.bg_ext, &self.obj_ext);
        // BG0 sources the 3D engine when DISPCNT bit 3 is set (Engine A only).
        let bg0_3d = three_d.filter(|_| self.dispcnt & (1 << 3) != 0);
        for y in 0..HEIGHT as u16 {
            // Reconstruct each affine/extended BG's reference for this scanline (the
            // reference advances by PB/PD down the frame) and re-latch, so affine and
            // extended backgrounds sample the correct map row per line rather than a
            // frozen line-0 reference. The whole frame renders at V-blank with the
            // final register values, so mid-frame register changes are not modelled.
            self.affine.bg2 = video2d::latch::affine_reference_for_line(&self.registers, 0, y);
            self.affine.bg3 = video2d::latch::affine_reference_for_line(&self.registers, 1, y);
            video2d::latch::latch_for_scanline(
                &self.registers,
                &self.affine,
                &mut self.latched,
                &mut self.segments,
            );
            let line = bg0_3d.map(|fb| {
                let mut l = [None; WIDTH];
                for (x, cell) in l.iter_mut().enumerate() {
                    let p = fb.pixels[y as usize * gpu3d::raster::WIDTH + x];
                    if p.covered {
                        *cell = Some(video2d::ExternalBg0Pixel {
                            color: video2d::Color15(pack_bgr555(p.color)),
                            alpha: p.alpha,
                        });
                    }
                }
                l
            });
            video2d::render_scanline(
                &mut self.render_fb,
                &self.segments,
                &mut self.scratch,
                y,
                &mem,
                layout,
                line.as_ref().map(|l| &l[..]),
                &mut video2d::debug::sink::NullSink,
            );
        }
        for (dst, src) in self.framebuffer.iter_mut().zip(self.render_fb.pixels.iter()) {
            *dst = src.0;
        }
    }
}

/// The two 2D engines plus the shared scanline timeline.
pub struct Ppu {
    /// Index 0 = Engine A (main), 1 = Engine B (sub).
    engines: [Engine; 2],
    /// `DISPSTAT` per core (each core has its own blank-IRQ enables).
    dispstat: [u16; 2],
    vcount: u16,
    frame: u64,
    started: bool,
}

impl Default for Ppu {
    fn default() -> Self {
        Ppu::new()
    }
}

impl Ppu {
    pub fn new() -> Self {
        Ppu {
            engines: [Engine::new(false), Engine::new(true)],
            dispstat: [0; 2],
            vcount: 0,
            frame: 0,
            started: false,
        }
    }

    pub fn frame(&self) -> u64 {
        self.frame
    }

    pub fn vcount(&self) -> u16 {
        self.vcount
    }

    /// An engine's output image in BGR555 (`0` = Engine A, `1` = Engine B).
    pub fn framebuffer(&self, engine: usize) -> &[u16] {
        &self.engines[engine].framebuffer
    }

    // --- registers ----------------------------------------------------------

    pub fn dispcnt(&self, engine: usize) -> u32 {
        self.engines[engine].dispcnt
    }

    /// A snapshot of an engine's `video2d` register file, for debugging.
    pub fn engine_registers(&self, engine: usize) -> video2d::Registers {
        self.engines[engine].registers
    }

    /// Debug: render each 2D BG (0-3) and OBJ (index 4) of `engine` in isolation
    /// (windows and 3D off) and count how many pixels differ from the backdrop —
    /// revealing which layers actually contribute. Re-renders the engine normally
    /// afterwards. Returns `[bg0, bg1, bg2, bg3, obj]`.
    pub fn debug_layer_coverage(
        &mut self,
        engine: usize,
        vram: &Vram,
        palette: &[u8],
        oam: &[u8],
    ) -> [usize; 5] {
        let pal = &palette[engine * 0x400..engine * 0x400 + 0x400];
        let oam = &oam[engine * 0x400..engine * 0x400 + 0x400];
        let backdrop = u16::from_le_bytes([pal[0], pal[1]]) & 0x7FFF;
        let e = &mut self.engines[engine];
        let saved = e.registers.dispcnt;
        // Preserve the real composited framebuffer — these isolated re-renders must not
        // leave a no-3D image behind for a later `framebuffer()`/`screen()` read.
        let saved_fb = e.framebuffer.clone();
        let mut cov = [0usize; 5];
        for (layer, c) in cov.iter_mut().enumerate() {
            let enable = if layer < 4 { 1u16 << (8 + layer) } else { 1 << 12 };
            // Enable only this layer; clear the other BG/OBJ enables, the window
            // enables (bits 13-15), and the 3D-BG0 bit (bit 3) so BG0 renders as text.
            e.registers.dispcnt = (saved & !0xFF08) | enable;
            e.render(vram, pal, oam, None);
            *c = e.framebuffer.iter().filter(|&&p| (p & 0x7FFF) != backdrop).count();
        }
        e.registers.dispcnt = saved;
        e.framebuffer = saved_fb;
        cov
    }

    /// Debug: render `engine`'s full composite with the given 3D framebuffer and return
    /// a copy of the BGR555 output — to test 3D-BG0 compositing directly.
    pub fn debug_render_engine(
        &mut self,
        engine: usize,
        vram: &Vram,
        palette: &[u8],
        oam: &[u8],
        three_d: Option<&gpu3d::raster::Framebuffer3d>,
    ) -> Vec<u16> {
        let pal = &palette[engine * 0x400..engine * 0x400 + 0x400];
        let oam = &oam[engine * 0x400..engine * 0x400 + 0x400];
        self.engines[engine].render(vram, pal, oam, three_d);
        self.engines[engine].framebuffer.clone()
    }

    /// Debug: render just BG `layer` (or OBJ = 4) of `engine` in isolation and return
    /// a copy of the resulting BGR555 framebuffer. Re-renders normally afterwards.
    pub fn debug_render_layer(
        &mut self,
        engine: usize,
        layer: usize,
        vram: &Vram,
        palette: &[u8],
        oam: &[u8],
    ) -> Vec<u16> {
        let pal = &palette[engine * 0x400..engine * 0x400 + 0x400];
        let oam = &oam[engine * 0x400..engine * 0x400 + 0x400];
        let e = &mut self.engines[engine];
        let saved = e.registers.dispcnt;
        let saved_fb = e.framebuffer.clone();
        let enable = if layer < 4 { 1u16 << (8 + layer) } else { 1 << 12 };
        e.registers.dispcnt = (saved & !0xFF08) | enable;
        e.render(vram, pal, oam, None);
        let out = e.framebuffer.clone();
        e.registers.dispcnt = saved;
        e.framebuffer = saved_fb; // restore the real composite (see debug_layer_coverage)
        out
    }

    pub fn write_dispcnt(&mut self, engine: usize, value: u32, bytes: u32) {
        self.engines[engine].write_dispcnt(value, bytes);
    }

    /// Write into an engine's 2D register block (`BGxCNT` … `BLDY`, offset relative to
    /// the engine's I/O base), routed to the shared `video2d` register layout.
    pub fn write_register(&mut self, engine: usize, offset: u32, value: u16, mask: u16) {
        self.engines[engine].registers.write16(offset, value, mask);
    }

    /// Read back from an engine's 2D register block.
    pub fn read_register(&self, engine: usize, offset: u32) -> u16 {
        self.engines[engine].registers.read16(offset)
    }

    /// Read `DISPSTAT` for a core: the live blank/match flags merged with its stored
    /// enable/setting bits.
    pub fn read_dispstat(&self, core: usize) -> u16 {
        let mut v = self.dispstat[core] & 0xFFB8; // keep enables + VCount setting
        if self.vcount >= HEIGHT as u16 {
            v |= 1 << 0; // V-blank
        }
        if self.vcount == self.vcount_setting(core) {
            v |= 1 << 2; // V-count match
        }
        v
    }

    pub fn write_dispstat(&mut self, core: usize, value: u16) {
        // Bits 0-2 are read-only flags; keep the enables and VCount setting.
        self.dispstat[core] = value & 0xFFB8;
    }

    fn vcount_setting(&self, core: usize) -> u16 {
        ((self.dispstat[core] >> 8) & 0xFF) | (((self.dispstat[core] >> 7) & 1) << 8)
    }

    // --- timing -------------------------------------------------------------

    /// Begin the continuous scanline schedule (idempotent).
    pub fn start(&mut self, scheduler: &mut Scheduler<NdsEvent>) {
        if self.started {
            return;
        }
        self.started = true;
        let now = scheduler.now();
        scheduler.schedule_at(now + HBLANK_START, NdsEvent::Ppu(PpuEvent::HBlank));
        scheduler.schedule_at(now + CYCLES_PER_LINE, NdsEvent::Ppu(PpuEvent::LineStart));
    }

    /// Service a scanline event, raising the enabled interrupts on both cores.
    #[allow(clippy::too_many_arguments)]
    pub fn handle_event(
        &mut self,
        event: PpuEvent,
        irqs: &mut [Interrupts; 2],
        vram: &Vram,
        palette: &[u8],
        oam: &[u8],
        three_d: Option<&gpu3d::raster::Framebuffer3d>,
        ctx: &mut EventContext<'_, NdsEvent>,
    ) {
        match event {
            PpuEvent::HBlank => {
                for (c, irq) in irqs.iter_mut().enumerate() {
                    if self.dispstat[c] & (1 << 4) != 0 {
                        irq.request(IrqSource::HBlank);
                    }
                }
            }
            PpuEvent::LineStart => {
                self.vcount = (self.vcount + 1) % TOTAL_LINES;

                if self.vcount == HEIGHT as u16 {
                    // Entering V-blank: render the finished frame and signal. Palette
                    // and OAM are split A/B: Engine A takes the low 1 KB, Engine B the
                    // high 1 KB (each engine's slice is BG then OBJ, as the renderer
                    // expects).
                    self.engines[0].render(vram, &palette[..0x400], &oam[..0x400], three_d);
                    self.engines[1].render(vram, &palette[0x400..], &oam[0x400..], None);
                    self.frame += 1;
                    for (c, irq) in irqs.iter_mut().enumerate() {
                        if self.dispstat[c] & (1 << 3) != 0 {
                            irq.request(IrqSource::VBlank);
                        }
                    }
                }

                // V-count match, per core.
                for (c, irq) in irqs.iter_mut().enumerate() {
                    if self.vcount == self.vcount_setting(c) && self.dispstat[c] & (1 << 5) != 0 {
                        irq.request(IrqSource::VCount);
                    }
                }

                let now = ctx.now;
                ctx.scheduler
                    .schedule_at(now + HBLANK_START, NdsEvent::Ppu(PpuEvent::HBlank));
                ctx.scheduler
                    .schedule_at(now + CYCLES_PER_LINE, NdsEvent::Ppu(PpuEvent::LineStart));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vram::Vram;

    /// An extended (affine-addressed) background must advance its reference point down
    /// the frame, so different scanlines sample different map rows. Regression test for
    /// the bug where the whole frame used a frozen line-0 reference, leaving affine and
    /// extended BGs stuck on one map row (which made scrolled foreground layers vanish).
    #[test]
    fn extended_bg_advances_affine_reference_per_scanline() {
        let mut ppu = Ppu::new();
        let mut vram = Vram::new();
        vram.set_control(3, 0x81); // bank D -> Engine A BG at 0x06000000

        // 16-tile-wide map (size 0 = 128×128): row 0 -> tile 1, row 1 -> tile 2.
        vram.write(0x0600_0000, 1, 2); // tile row 0, col 0
        vram.write(0x0600_0000 + 16 * 2, 2, 2); // tile row 1, col 0
        // 8bpp tiles at char base block 1 (0x4000): tile 1 = texel 1, tile 2 = texel 2.
        for i in 0..32 {
            vram.write(0x0600_4040 + i * 2, 0x0101, 2); // tile 1
            vram.write(0x0600_4080 + i * 2, 0x0202, 2); // tile 2
        }

        let mut palette = vec![0u8; 0x800];
        palette[2..4].copy_from_slice(&0x001Fu16.to_le_bytes()); // entry 1 = red
        palette[4..6].copy_from_slice(&0x03E0u16.to_le_bytes()); // entry 2 = green
        let oam = vec![0u8; 0x800];

        // Engine A: mode 5 (BG2 extended), BG2 enabled, graphics display mode.
        ppu.write_dispcnt(0, 5 | (1 << 10) | (1 << 16), 4);
        ppu.write_register(0, 0x0C, 1 << 2, 0xFFFF); // BG2CNT: tiled, char base block 1
        ppu.write_register(0, 0x20, 0x100, 0xFFFF); // BG2 PA = 1.0
        ppu.write_register(0, 0x26, 0x100, 0xFFFF); // BG2 PD = 1.0 (ref advances by 1 px/line)

        let fb = ppu.debug_render_layer(0, 2, &vram, &palette, &oam);
        // Scanline 0 samples map row 0 (tile 1 = red); scanline 8 samples row 1 (tile 2
        // = green). With the frozen-reference bug both would be red.
        assert_eq!(fb[0], 0x001F, "scanline 0 samples map row 0 (red)");
        assert_eq!(fb[8 * WIDTH], 0x03E0, "scanline 8 samples map row 1 (green)");
    }
}
