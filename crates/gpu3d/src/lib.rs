//! The DS 3D graphics engine: a fixed-point geometry pipeline and a software
//! scanline rasterizer, built as the correctness reference for a later compute
//! backend. All geometry and lighting math is fixed-point integer — there is no
//! `f32` anywhere in this crate.
//!
//! This crate owns the console's GX register block (the ARM9 side of `0x4000400`+,
//! plus `DISP3DCNT` and the clear/fog/toon control registers) behind [`Gpu3d`]. The
//! `nds` crate routes those registers here, drains the command FIFO into the
//! geometry engine, and composites the rasterized output as BG0 of 2D Engine A.
//!
//! Current status: the command processor and FIFO (this module, [`command`],
//! [`fifo`]) are in place; the matrix engine, geometry, clipping, lighting,
//! rasterizer, and provenance follow in later phases.

pub mod command;
pub mod fifo;

use command::Decoder;
use fifo::{Entry, Fifo};

/// IO register offsets relative to `0x0400_0000` (the ARM9 GX block).
mod reg {
    pub const DISP3DCNT: u32 = 0x060;
    pub const GXFIFO_LO: u32 = 0x400;
    pub const GXFIFO_HI: u32 = 0x440; // exclusive end of the GXFIFO region
    pub const PORT_HI: u32 = 0x600; // exclusive end of the command-port region
    pub const GXSTAT: u32 = 0x600;
    pub const RAM_COUNT: u32 = 0x604;
}

/// The 3D engine device: the command FIFO and its decoder, plus the GX control
/// registers. The geometry engine, render buffers, and rasterizer attach in later
/// phases.
#[derive(Default)]
pub struct Gpu3d {
    fifo: Fifo,
    decoder: Decoder,
    /// `DISP3DCNT` (`0x4000060`): 3D display/blend/fog/edge enables.
    disp3dcnt: u16,
}

impl Gpu3d {
    pub fn new() -> Self {
        Gpu3d::default()
    }

    // --- command submission -------------------------------------------------

    /// Submit one packed GXFIFO word (the bulk/DMA path). Decodes into per-parameter
    /// [`Entry`]s and buffers them. Returns `false` if the FIFO filled mid-word — the
    /// caller should stall the CPU rather than lose the write.
    pub fn write_gxfifo(&mut self, word: u32) -> bool {
        fifo::push_decoded(&mut self.fifo, &mut self.decoder, word) == 0
    }

    /// Submit one word to a direct command port (`command` implied by the address).
    /// Each write is one parameter (a dummy `0` for zero-parameter commands).
    pub fn write_port(&mut self, command: u8, word: u32) -> bool {
        let param = if command::param_count(command) == 0 { 0 } else { word };
        self.fifo.push(Entry { command, param })
    }

    /// Drain the next buffered command entry (the geometry engine's input).
    pub fn pop_command(&mut self) -> Option<Entry> {
        self.fifo.pop()
    }

    /// Drain and discard every buffered command. **Temporary integration scaffolding:**
    /// until the geometry engine consumes the FIFO, the `nds` glue calls this after
    /// each submission so a full FIFO never back-pressures the CPU (and `GXSTAT` reads
    /// as empty/idle). Replaced by real command execution in the geometry phase.
    pub fn discard_fifo(&mut self) {
        while self.fifo.pop().is_some() {}
    }

    // --- FIFO status for the host bus ---------------------------------------

    /// Whether a GXFIFO DMA should run to refill the FIFO (it is below half full).
    pub fn fifo_wants_dma(&self) -> bool {
        self.fifo.wants_dma()
    }

    /// Whether the FIFO currently asserts its IRQ condition (per its IRQ mode).
    pub fn fifo_irq_asserted(&self) -> bool {
        self.fifo.irq_asserted()
    }

    /// Whether the FIFO is full — a further command write must stall the CPU.
    pub fn fifo_full(&self) -> bool {
        self.fifo.is_full()
    }

    // --- MMIO ---------------------------------------------------------------

    /// Write an ARM9 GX register. `offset` is relative to `0x0400_0000`. Command
    /// submission (GXFIFO and the direct ports) is always a 32-bit word.
    pub fn write_register(&mut self, offset: u32, value: u32, bytes: u32) {
        match offset {
            reg::GXFIFO_LO..reg::GXFIFO_HI => {
                self.write_gxfifo(value);
            }
            reg::GXFIFO_HI..reg::PORT_HI => {
                let command = ((offset - reg::GXFIFO_LO) >> 2) as u8;
                self.write_port(command, value);
            }
            reg::GXSTAT => {
                // Only the IRQ mode (bits 30-31) is writable here; bit 15 (write-1)
                // clears the matrix-stack error, handled by the matrix engine later.
                self.fifo.set_irq_mode_bits(value >> 30);
            }
            reg::DISP3DCNT => {
                self.disp3dcnt = merge16(self.disp3dcnt, value, bytes);
            }
            // Clear/fog/toon/edge control registers are consumed by the rasterizer
            // (later phases); accept and ignore their writes for now.
            _ => {}
        }
    }

    /// Read an ARM9 GX register. `offset` is relative to `0x0400_0000`.
    pub fn read_register(&self, offset: u32, _bytes: u32) -> u32 {
        match offset {
            reg::GXSTAT => self.gxstat(),
            reg::RAM_COUNT => 0, // vertex/polygon RAM counts — geometry phase
            reg::DISP3DCNT => self.disp3dcnt as u32,
            _ => 0,
        }
    }

    /// `GXSTAT` (`0x4000600`): the FIFO's own bits plus (later) matrix-stack level,
    /// stack error, and geometry-busy.
    fn gxstat(&self) -> u32 {
        self.fifo.gxstat_bits()
    }
}

/// Merge a partial (byte/halfword) write into a 16-bit register, honouring the write
/// width so a byte write to the high or low half lands correctly.
fn merge16(current: u16, value: u32, bytes: u32) -> u16 {
    match bytes {
        1 => (current & 0xFF00) | (value as u16 & 0x00FF),
        _ => value as u16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gxfifo_write_then_drain_round_trips_a_command() {
        let mut g = Gpu3d::new();
        // MTX_MODE (1 param) via the packed FIFO.
        g.write_register(reg::GXFIFO_LO, command::op::MTX_MODE as u32, 4);
        g.write_register(reg::GXFIFO_LO, 1, 4); // param = position mode
        assert_eq!(g.pop_command(), Some(Entry { command: command::op::MTX_MODE, param: 1 }));
        assert_eq!(g.pop_command(), None);
    }

    #[test]
    fn direct_port_address_implies_the_command() {
        let mut g = Gpu3d::new();
        // Port for VTX_16 (0x23) is 0x400 + 0x23*4 = 0x48C; two parameter words.
        let port = reg::GXFIFO_LO + (command::op::VTX_16 as u32) * 4;
        assert_eq!(port, 0x48C);
        g.write_register(port, 0x0ABC_0DEF, 4);
        g.write_register(port, 0x0000_0123, 4);
        assert_eq!(g.pop_command(), Some(Entry { command: command::op::VTX_16, param: 0x0ABC_0DEF }));
        assert_eq!(g.pop_command(), Some(Entry { command: command::op::VTX_16, param: 0x0000_0123 }));
    }

    #[test]
    fn gxstat_reports_count_and_flags() {
        let mut g = Gpu3d::new();
        assert_eq!(g.read_register(reg::GXSTAT, 4) & (1 << 26), 1 << 26); // empty
        // Push a few entries via a zero-param packing word (4 MTX_PUSH).
        let packing = u32::from_le_bytes([command::op::MTX_PUSH; 4]);
        g.write_register(reg::GXFIFO_LO, packing, 4);
        let stat = g.read_register(reg::GXSTAT, 4);
        assert_eq!((stat >> 16) & 0x1FF, 4); // count == 4
        assert_eq!(stat & (1 << 26), 0); // not empty
        assert_eq!(stat & (1 << 25), 1 << 25); // less than half
    }

    #[test]
    fn gxstat_write_sets_irq_mode() {
        let mut g = Gpu3d::new();
        g.write_register(reg::GXSTAT, 1 << 30, 4); // IRQ mode 1 (less than half)
        assert!(g.fifo_irq_asserted()); // empty is < half
        assert_eq!(g.read_register(reg::GXSTAT, 4) >> 30, 1);
    }
}
