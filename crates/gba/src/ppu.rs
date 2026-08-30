//! A timing-only model of the GBA's picture processing unit.
//!
//! No pixels are rendered here. This is the scanline/blank state machine the
//! rest of the timing model — VCOUNT, the HBlank/VBlank/V-counter interrupts,
//! and later the blank-triggered DMAs and the renderer itself — attaches to.
//!
//! The display runs continuously. Each of the 228 scanlines is 1232 cycles: the
//! line starts (VCOUNT advances), then at cycle 1006 the horizontal blank
//! begins, then at 1232 the next line starts. Two scheduled events per line
//! drive this; VBlank and V-counter transitions are derived at line start. No
//! generation invalidation is needed because DISPSTAT writes change only which
//! interrupts are enabled and the V-count target, never the timing itself.

use crate::event::{EventKind, PpuEvent};
use crate::interrupt::{InterruptController, IrqSource};
use emu_core::{EventContext, Scheduler, Timestamp};

/// Cycles per scanline (308 dots × 4 cycles).
const CYCLES_PER_LINE: Timestamp = 1232;
/// Cycle within a scanline at which the HBlank flag is raised. Drawing ends at
/// 960, but hardware holds the flag low for 1006 cycles.
const HBLANK_START_CYCLE: Timestamp = 1006;
/// Scanlines per frame (160 visible + 68 blank).
const TOTAL_LINES: u16 = 228;
/// First scanline of the vertical blank.
const VBLANK_START_LINE: u16 = 160;
/// Last scanline for which the VBlank flag is set (227 clears it).
const VBLANK_FLAG_LAST_LINE: u16 = 226;

/// The PPU timing state (`DISPSTAT` control bits plus the current scanline).
#[derive(Clone, Copy, Debug, Default)]
pub struct Ppu {
    vcount: u16,
    hblank_flag: bool,
    /// `DISPSTAT` V-count target (LYC).
    vcount_target: u16,
    vblank_irq_enable: bool,
    hblank_irq_enable: bool,
    vcount_irq_enable: bool,
}

impl Ppu {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current scanline (VCOUNT), 0..=227.
    pub fn vcount(&self) -> u16 {
        self.vcount
    }

    /// Whether the current scanline is within the VBlank flag window (160..=226).
    pub fn vblank_flag(&self) -> bool {
        (VBLANK_START_LINE..=VBLANK_FLAG_LAST_LINE).contains(&self.vcount)
    }

    /// Whether the current scanline is in its horizontal blank.
    pub fn hblank_flag(&self) -> bool {
        self.hblank_flag
    }

    /// Whether VCOUNT currently matches the V-count target.
    pub fn vcount_match(&self) -> bool {
        self.vcount == self.vcount_target
    }

    /// Read `DISPSTAT` (`4000004h`).
    pub fn read_dispstat(&self) -> u16 {
        (self.vblank_flag() as u16)
            | ((self.hblank_flag as u16) << 1)
            | ((self.vcount_match() as u16) << 2)
            | ((self.vblank_irq_enable as u16) << 3)
            | ((self.hblank_irq_enable as u16) << 4)
            | ((self.vcount_irq_enable as u16) << 5)
            | (self.vcount_target << 8)
    }

    /// Write `DISPSTAT`. Bits 0-2 are read-only flags and ignored; bits 3-5 are
    /// the interrupt enables; bits 8-15 are the V-count target.
    pub fn write_dispstat(&mut self, value: u16) {
        self.vblank_irq_enable = value & (1 << 3) != 0;
        self.hblank_irq_enable = value & (1 << 4) != 0;
        self.vcount_irq_enable = value & (1 << 5) != 0;
        self.vcount_target = (value >> 8) & 0xFF;
    }

    /// Read `VCOUNT` (`4000006h`).
    pub fn read_vcount(&self) -> u16 {
        self.vcount
    }

    /// Begin LCD timing at `now`, on scanline 0.
    pub fn start(&mut self, now: Timestamp, scheduler: &mut Scheduler<EventKind>) {
        self.vcount = 0;
        self.hblank_flag = false;
        self.schedule_line_events(now, scheduler);
    }

    /// Dispatch a PPU timing event.
    pub fn handle_event(
        &mut self,
        event: PpuEvent,
        irq: &mut InterruptController,
        ctx: &mut EventContext<'_, EventKind>,
    ) {
        match event {
            PpuEvent::HBlank => self.on_hblank(irq),
            PpuEvent::LineStart => self.on_line_start(irq, ctx),
        }
    }

    fn on_hblank(&mut self, irq: &mut InterruptController) {
        self.hblank_flag = true;
        // The HBlank interrupt fires on every scanline, VBlank lines included —
        // matching hardware. (GBATEK's "no HBlank IRQ within VBlank" note is
        // inaccurate; it is HBlank-triggered *DMA* that is restricted to the
        // visible lines, which will matter when DMA is added.)
        if self.hblank_irq_enable {
            irq.request(IrqSource::HBlank);
        }
    }

    fn on_line_start(&mut self, irq: &mut InterruptController, ctx: &mut EventContext<'_, EventKind>) {
        self.vcount = (self.vcount + 1) % TOTAL_LINES;
        self.hblank_flag = false;

        // Entering the vertical blank raises the VBlank interrupt once per frame.
        if self.vcount == VBLANK_START_LINE && self.vblank_irq_enable {
            irq.request(IrqSource::VBlank);
        }
        // The V-counter interrupt fires on the line whose number matches LYC.
        if self.vcount_match() && self.vcount_irq_enable {
            irq.request(IrqSource::VCounterMatch);
        }

        self.schedule_line_events(ctx.now, ctx.scheduler);
    }

    /// Queue this scanline's HBlank and the following line start.
    fn schedule_line_events(&self, line_start: Timestamp, scheduler: &mut Scheduler<EventKind>) {
        scheduler.schedule_at(
            line_start + HBLANK_START_CYCLE,
            EventKind::Ppu(PpuEvent::HBlank),
        );
        scheduler.schedule_at(
            line_start + CYCLES_PER_LINE,
            EventKind::Ppu(PpuEvent::LineStart),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispstat_round_trips_control_bits() {
        let mut ppu = Ppu::new();
        // LYC = 100, V-count IRQ and VBlank IRQ enabled.
        ppu.write_dispstat((100 << 8) | (1 << 5) | (1 << 3));
        assert_eq!(ppu.vcount_target, 100);
        // At line 0: no flags set, so the read is just the control bits back.
        assert_eq!(ppu.read_dispstat(), (100 << 8) | (1 << 5) | (1 << 3));
    }

    #[test]
    fn hblank_flag_reads_back_in_dispstat() {
        let mut ppu = Ppu::new();
        let mut irq = InterruptController::new();
        ppu.on_hblank(&mut irq);
        assert!(ppu.hblank_flag());
        assert_ne!(ppu.read_dispstat() & (1 << 1), 0);
    }
}
