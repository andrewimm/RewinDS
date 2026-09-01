//! The ARM7 SPI bus (`SPICNT`/`SPIDATA` at `0x40001C0`/`0x40001C2`).
//!
//! Three devices share the bus, selected by `SPICNT` bits 8-9: the power-management
//! chip, the firmware serial flash, and the touchscreen controller. Only the
//! **firmware** device is modelled — games read their touchscreen calibration and
//! other user settings from it during boot (see [`crate::firmware`]); the others
//! return zero for now (touchscreen input is a later milestone).
//!
//! Transfers are byte-at-a-time: the CPU writes a byte to `SPIDATA` to clock it out
//! and reads the byte clocked back in. A multi-byte command holds chip-select
//! (`SPICNT` bit 11) across the transfers and releases it on the last one, which
//! ends the command. Transfers complete instantly here (the busy flag reads 0).

use crate::firmware;

/// `SPICNT` device select (bits 8-9).
const DEVICE_FIRMWARE: u16 = 1;

/// The SPI bus controller and the firmware-flash device on it.
pub struct Spi {
    /// `SPICNT` (`0x40001C0`). The busy flag (bit 7) always reads 0.
    cnt: u16,
    /// The last byte clocked in, returned by reads of `SPIDATA`.
    data: u8,
    /// The firmware serial flash image.
    flash: Vec<u8>,
    /// The in-progress flash command byte (0 = idle, awaiting a command).
    command: u8,
    /// The flash byte address a read command walks through.
    address: u32,
    /// How many bytes of the current command have been clocked (command + address).
    phase: u32,
}

impl Default for Spi {
    fn default() -> Self {
        Spi::new()
    }
}

impl Spi {
    pub fn new() -> Self {
        Spi {
            cnt: 0,
            data: 0,
            flash: firmware::firmware_flash(),
            command: 0,
            address: 0,
            phase: 0,
        }
    }

    /// Read `SPICNT`; the busy flag (bit 7) is always clear (instant transfers).
    pub fn read_cnt(&self) -> u16 {
        self.cnt & !(1 << 7)
    }

    /// Write `SPICNT`. Chip-select hold (bit 11) takes effect on the *next* transfer
    /// (deselect happens after a transfer with hold clear, in [`Self::write_data`]),
    /// so it is not acted on here; only disabling the bus (bit 15) ends a command.
    pub fn write_cnt(&mut self, value: u16) {
        self.cnt = value;
        if value & (1 << 15) == 0 {
            self.deselect();
        }
    }

    /// Read `SPIDATA`: the byte clocked in by the last transfer.
    pub fn read_data(&self) -> u16 {
        self.data as u16
    }

    /// Write `SPIDATA`: clock one byte out to the selected device and latch the byte
    /// clocked back in. Chip-select is released after the byte unless bit 11 is set.
    pub fn write_data(&mut self, value: u16) {
        if self.cnt & (1 << 15) == 0 {
            return; // bus disabled
        }
        let device = (self.cnt >> 8) & 3;
        self.data = if device == DEVICE_FIRMWARE {
            self.firmware_transfer(value as u8)
        } else {
            0 // power management / touchscreen not modelled
        };
        if self.cnt & (1 << 11) == 0 {
            self.deselect();
        }
    }

    /// End the current command (chip-select released).
    fn deselect(&mut self) {
        self.command = 0;
        self.address = 0;
        self.phase = 0;
    }

    /// Clock one byte through the firmware flash, returning the byte read back. The
    /// only command a booting game needs is READ (`0x03`) + a 3-byte big-endian
    /// address, then a run of reads; a few status/id commands return benign values.
    fn firmware_transfer(&mut self, value: u8) -> u8 {
        if self.command == 0 {
            self.command = value;
            self.phase = 0;
            return 0;
        }
        match self.command {
            0x03 => {
                if self.phase < 3 {
                    self.address = (self.address << 8) | value as u32;
                    self.phase += 1;
                    0
                } else {
                    let byte = self.flash[self.address as usize & (self.flash.len() - 1)];
                    self.address += 1;
                    byte
                }
            }
            0x05 => 0,    // RDSR: status = ready, not write-protected
            0x9F => 0x00, // RDID: no meaningful JEDEC id
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a firmware READ of the user-settings pointer and confirm it points at
    /// area 1, then read the touchscreen calibration there and confirm it is the
    /// non-zero value from `firmware::user_settings` (the deadlock-breaking data).
    #[test]
    fn firmware_read_returns_user_settings_calibration() {
        let mut spi = Spi::new();
        // Select firmware, 8-bit, keep chip-select held across the command.
        let hold = (DEVICE_FIRMWARE << 8) | (1 << 11) | (1 << 15);
        let read_at = |spi: &mut Spi, addr: u32, n: usize| -> Vec<u8> {
            spi.write_cnt(hold);
            spi.write_data(0x03); // READ
            spi.write_data((addr >> 16) as u16);
            spi.write_data((addr >> 8) as u16);
            spi.write_data(addr as u16);
            let mut out = Vec::new();
            for _ in 0..n {
                spi.write_data(0);
                out.push(spi.read_data() as u8);
            }
            spi.write_cnt(0); // disable the bus → end the command
            out
        };

        // Header [0x20] = user-settings offset / 8 = 0x3FE00 / 8.
        let ptr = read_at(&mut spi, 0x20, 2);
        let offset = u16::from_le_bytes([ptr[0], ptr[1]]) as u32 * 8;
        assert_eq!(offset, 0x3FE00);

        // adc.x1 at settings + 0x58 must be non-zero (matches user_settings()).
        let cal = read_at(&mut spi, offset + 0x58, 2);
        assert_eq!(u16::from_le_bytes([cal[0], cal[1]]), 0x0200);
    }
}
