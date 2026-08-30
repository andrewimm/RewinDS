//! Minimal serial I/O (SIO) — enough for boot-time link/adapter detection.
//!
//! Real serial communication (link cable, wireless adapter) is not modeled. But
//! games probe the serial port at boot and wait for the transfer-complete
//! interrupt; with no SIO at all that interrupt never fires and they hang. This
//! completes a started transfer immediately as "nothing connected" (received data
//! reads all-ones) and raises the serial interrupt, so detection times out and
//! the game proceeds in single-player.

use crate::interrupt::{InterruptController, IrqSource};

/// `SIOCNT` bit 7 — start / busy.
const START_BUSY: u16 = 1 << 7;
/// `SIOCNT` bit 14 — transfer-complete interrupt enable.
const IRQ_ENABLE: u16 = 1 << 14;

/// The serial registers, modeled just enough to answer detection probes.
#[derive(Clone, Copy, Debug, Default)]
pub struct Serial {
    siocnt: u16,
    rcnt: u16,
    /// `SIOMULTI0..3` / `SIODATA32` — received data. Nothing connected reads
    /// `0xFFFF` in every slot.
    data: [u16; 4],
    /// `SIOMLT_SEND` / `SIODATA8`.
    send: u16,
}

impl Serial {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read a serial register (offsets relative to `0x4000000`).
    pub fn read16(&self, offset: u32) -> u16 {
        match offset {
            0x120 => self.data[0],
            0x122 => self.data[1],
            0x124 => self.data[2],
            0x126 => self.data[3],
            0x128 => self.siocnt,
            0x12A => self.send,
            0x134 => self.rcnt,
            _ => 0,
        }
    }

    /// Apply a masked write. Starting a transfer (`SIOCNT` bit 7) completes it
    /// immediately and fires the serial interrupt if enabled.
    pub fn write16(&mut self, offset: u32, value: u16, mask: u16, irq: &mut InterruptController) {
        let merge = |cur: u16| (cur & !mask) | (value & mask);
        match offset {
            0x120 => self.data[0] = merge(self.data[0]),
            0x122 => self.data[1] = merge(self.data[1]),
            0x124 => self.data[2] = merge(self.data[2]),
            0x126 => self.data[3] = merge(self.data[3]),
            0x128 => {
                self.siocnt = merge(self.siocnt);
                if self.siocnt & START_BUSY != 0 {
                    self.complete_transfer(irq);
                }
            }
            0x12A => self.send = merge(self.send),
            0x134 => self.rcnt = merge(self.rcnt),
            _ => {}
        }
    }

    fn complete_transfer(&mut self, irq: &mut InterruptController) {
        // Nothing is connected: received data is all-ones and the transfer ends.
        self.data = [0xFFFF; 4];
        self.siocnt &= !START_BUSY;
        if self.siocnt & IRQ_ENABLE != 0 {
            irq.request(IrqSource::Serial);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starting_a_transfer_completes_and_fires_irq() {
        let mut serial = Serial::new();
        let mut irq = InterruptController::new();
        // Start a transfer with the transfer-complete IRQ enabled.
        serial.write16(0x128, START_BUSY | IRQ_ENABLE, 0xFFFF, &mut irq);
        // The busy bit clears, received data reads all-ones, IRQ requested.
        assert_eq!(serial.read16(0x128) & START_BUSY, 0);
        assert_eq!(serial.read16(0x120), 0xFFFF);
        assert_ne!(irq.iflags() & IrqSource::Serial.mask(), 0);
    }

    #[test]
    fn no_irq_without_enable() {
        let mut serial = Serial::new();
        let mut irq = InterruptController::new();
        serial.write16(0x128, START_BUSY, 0xFFFF, &mut irq);
        assert_eq!(serial.read16(0x128) & START_BUSY, 0); // still completes
        assert_eq!(irq.iflags() & IrqSource::Serial.mask(), 0); // but no interrupt
    }
}
