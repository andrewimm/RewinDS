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
    fn render(&mut self, vram: &Vram, palette: &[u8], oam: &[u8]) {
        match (self.dispcnt >> 16) & 3 {
            // Graphics: composite the BG/OBJ layers through the shared renderer.
            1 => self.render_graphics(vram, palette, oam),
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
    fn render_graphics(&mut self, vram: &Vram, palette: &[u8], oam: &[u8]) {
        video2d::latch::reload_affine_references(&self.registers, &mut self.affine);
        video2d::latch::latch_for_scanline(
            &self.registers,
            &self.affine,
            &mut self.latched,
            &mut self.segments,
        );

        let (bg_view, obj_view) = self.vram_view.split_at_mut(OBJ_VIEW_BASE as usize);
        if self.is_b {
            vram.assemble_engine_b_bg(bg_view);
            vram.assemble_engine_b_obj(obj_view);
        } else {
            vram.assemble_engine_a_bg(bg_view);
            vram.assemble_engine_a_obj(obj_view);
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
        };
        let mem = video2d::PpuMemoryView::new(&self.vram_view, palette, oam);
        for y in 0..HEIGHT as u16 {
            video2d::render_scanline(
                &mut self.render_fb,
                &self.segments,
                &mut self.scratch,
                y,
                &mem,
                layout,
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
                    self.engines[0].render(vram, &palette[..0x400], &oam[..0x400]);
                    self.engines[1].render(vram, &palette[0x400..], &oam[0x400..]);
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
