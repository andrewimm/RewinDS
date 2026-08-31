//! 2D Engine A timing and a minimal display path.
//!
//! This drives the DS video timeline — VCOUNT, the H/V-blank flags, the scanline
//! schedule — and raises the V-blank, H-blank, and V-count interrupts on **both**
//! cores (each core has its own `DISPSTAT` enables). The DS runs its dot clock at
//! 5.585664 MHz = 33.513982 MHz / 6, and the master tick is one ARM9 cycle
//! (~67 MHz), so each dot spans 12 master ticks.
//!
//! Rendering is deliberately minimal for now: the "VRAM display" mode (a direct
//! 15-bit blit of an LCDC VRAM block) is implemented, which produces a real image
//! without the full tile/sprite pipeline. Graphics mode (BG/OBJ) awaits the
//! `video2d` renderer extraction; it renders black until then.

use emu_core::{EventContext, Scheduler, Timestamp};

use crate::interrupt::{Interrupts, IrqSource};
use crate::system::NdsEvent;
use crate::vram::Vram;

pub const WIDTH: usize = 256;
pub const HEIGHT: usize = 192;

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

/// Engine A timing state plus its output framebuffer.
pub struct Ppu {
    /// `DISPCNT` — Engine A control (ARM9).
    dispcnt: u32,
    /// The Engine A 2D register block (`0x008`-`0x054`), shared in layout with the
    /// GBA so the extracted `video2d` renderer consumes it directly. The full
    /// tiled BG/OBJ render is wired once `video2d` is generalized for the DS's
    /// dimensions and banked VRAM; today this captures the state and the backdrop.
    registers: video2d::Registers,
    /// `DISPSTAT` per core (each core has its own blank-IRQ enables).
    dispstat: [u16; 2],
    vcount: u16,
    frame: u64,
    /// Output image in BGR555, `WIDTH * HEIGHT`.
    framebuffer: Vec<u16>,
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
            dispcnt: 0,
            registers: video2d::Registers::default(),
            dispstat: [0; 2],
            vcount: 0,
            frame: 0,
            framebuffer: vec![0; WIDTH * HEIGHT],
            started: false,
        }
    }

    pub fn frame(&self) -> u64 {
        self.frame
    }

    pub fn vcount(&self) -> u16 {
        self.vcount
    }

    pub fn framebuffer(&self) -> &[u16] {
        &self.framebuffer
    }

    // --- registers ----------------------------------------------------------

    pub fn dispcnt(&self) -> u32 {
        self.dispcnt
    }

    pub fn write_dispcnt(&mut self, value: u32, bytes: u32) {
        if bytes == 4 {
            self.dispcnt = value;
        } else {
            self.dispcnt = (self.dispcnt & !0xFFFF) | (value & 0xFFFF);
        }
        // Mirror the low half into the shared register snapshot (the DS shares the
        // GBA's low DISPCNT bits: BG mode and per-layer enables).
        self.registers.dispcnt = self.dispcnt as u16;
    }

    /// Write into the Engine A 2D register block (`BGxCNT` … `BLDY`, offset
    /// relative to `0x0400_0000`), routed to the shared `video2d` register layout.
    pub fn write_register(&mut self, offset: u32, value: u16, mask: u16) {
        self.registers.write16(offset, value, mask);
    }

    /// Read back from the Engine A 2D register block.
    pub fn read_register(&self, offset: u32) -> u16 {
        self.registers.read16(offset)
    }

    /// Read `DISPSTAT` for a core: the live blank/match flags merged with its
    /// stored enable/setting bits.
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
    pub fn handle_event(
        &mut self,
        event: PpuEvent,
        irqs: &mut [Interrupts; 2],
        vram: &Vram,
        palette: &[u8],
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
                    // Entering V-blank: render the finished frame and signal.
                    self.render_frame(vram, palette);
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

    /// Produce the frame from the Engine A display mode (`DISPCNT` bits 16-17).
    fn render_frame(&mut self, vram: &Vram, palette: &[u8]) {
        match (self.dispcnt >> 16) & 3 {
            // Graphics: full BG/OBJ compositing awaits the `video2d` generalization
            // (DS dimensions + banked VRAM); present the backdrop (BG palette 0).
            1 => {
                let backdrop = u16::from_le_bytes([palette[0], palette[1]]);
                self.framebuffer.fill(backdrop);
            }
            // Direct "VRAM display": blit an LCDC VRAM block (A–D) as a bitmap.
            2 => {
                let block = ((self.dispcnt >> 18) & 3) as usize;
                for (i, px) in self.framebuffer.iter_mut().enumerate() {
                    *px = vram.block_read16(block, i * 2);
                }
            }
            // Display off, or main-memory FIFO (unmodelled): black.
            _ => self.framebuffer.fill(0),
        }
    }
}
