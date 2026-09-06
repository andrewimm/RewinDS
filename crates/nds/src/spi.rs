//! The ARM7 SPI bus (`SPICNT`/`SPIDATA` at `0x40001C0`/`0x40001C2`).
//!
//! Three devices share the bus, selected by `SPICNT` bits 8-9: the power-management
//! chip, the firmware serial flash, and the touchscreen controller (a TSC2046-style
//! ADC). The firmware device serves the user settings games read at boot (see
//! [`crate::firmware`]); the touchscreen device reports the current pen position as
//! 12-bit ADC values (see [`Spi::set_touch`]); the power chip stores its registers.
//!
//! Transfers are byte-at-a-time: the CPU writes a byte to `SPIDATA` to clock it out
//! and reads the byte clocked back in. A multi-byte command holds chip-select
//! (`SPICNT` bit 11) across the transfers and releases it on the last one, which
//! ends the command. Transfers complete instantly here (the busy flag reads 0).

use crate::firmware;

/// Power-on reset values of the DS Power Management chip's registers (GBATEK "DS Power
/// Management Device"). Register 0 powers up with the sound amplifier (bit 0) and both
/// backlights (bits 2-3) enabled (`0x0D`); register 2 = `0x01`, register 4 = `0x03`.
/// These are the chip's hardware defaults, present from power-on — a direct-booted game
/// reads them before it writes any of its own, and boot handshakes check e.g. the
/// sound-amplifier bit, so starting them at zero would misreport the console's state.
fn pmic_reset() -> [u8; 8] {
    let mut regs = [0u8; 8];
    regs[0] = 0x0D;
    regs[2] = 0x01;
    regs[4] = 0x03;
    regs
}

/// `SPICNT` device select (bits 8-9).
const DEVICE_POWER: u16 = 0;
const DEVICE_FIRMWARE: u16 = 1;
const DEVICE_TOUCH: u16 = 2;

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
    /// Power-management (PMIC) registers, and its per-command index state.
    pmic: [u8; 8],
    pmic_index: u8,
    pmic_reading: bool,
    /// Whether the next PMIC byte is the command byte (reset on chip-select release).
    pmic_command: bool,
    /// Current pen position as a raw 12-bit ADC `(x, y)` pair, or `None` when no pen
    /// is down. Set by the host through [`Self::set_touch`].
    touch: Option<(u16, u16)>,
    /// The touchscreen readout shift register: a conversion result staged MSB-first
    /// for the two data bytes the reader clocks out after a control byte.
    touch_output: u16,
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
            pmic: pmic_reset(),
            pmic_index: 0,
            pmic_reading: false,
            pmic_command: true,
            touch: None,
            touch_output: 0,
        }
    }

    /// Replace the firmware flash with a real dump, for firmware boot (where the
    /// ARM7 BIOS reads and runs the firmware's boot code rather than the emulator
    /// synthesizing the settings). Direct boot keeps the synthesized flash.
    pub fn set_flash(&mut self, data: &[u8]) {
        self.flash = data.to_vec();
    }

    /// Set (or clear) the touchscreen pen position as a raw 12-bit ADC `(x, y)` pair.
    /// `None` lifts the pen: position channels then read 0. The caller converts a
    /// screen pixel to ADC via [`crate::firmware::touch_adc`].
    pub fn set_touch(&mut self, adc: Option<(u16, u16)>) {
        self.touch = adc;
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
        self.data = match device {
            DEVICE_FIRMWARE => self.firmware_transfer(value as u8),
            DEVICE_POWER => self.power_transfer(value as u8),
            DEVICE_TOUCH => self.touch_transfer(value as u8),
            _ => 0,
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
        self.pmic_command = true;
    }

    /// Clock one byte through the power-management chip: the first byte selects a
    /// register (bit 7 = read) and the following bytes read or write it. Registers
    /// (backlight, power, sound-amp control) are stored and read back.
    fn power_transfer(&mut self, value: u8) -> u8 {
        if self.pmic_command {
            self.pmic_reading = value & 0x80 != 0;
            self.pmic_index = value & 0x7F;
            self.pmic_command = false;
            return 0;
        }
        let index = (self.pmic_index & 0x7) as usize;
        self.pmic_index = self.pmic_index.wrapping_add(1);
        if self.pmic_reading {
            self.pmic[index]
        } else {
            self.pmic[index] = value;
            0
        }
    }

    /// Clock one byte through the touchscreen ADC. A control byte (bit 7 set) starts
    /// a conversion for its channel (bits 6-4: 1 = Y, 5 = X); the 12-bit result then
    /// clocks out MSB-first over the next two bytes, positioned as a TSC2046 does
    /// (`result << 3`, so the reader recovers it as `(b0 << 5) | (b1 >> 3)`). With no
    /// pen down every position channel converts to 0.
    fn touch_transfer(&mut self, value: u8) -> u8 {
        if value & 0x80 != 0 {
            let channel = (value >> 4) & 7;
            let (x, y) = self.touch.unwrap_or((0, 0));
            let result = match channel {
                1 => y, // Y position
                5 => x, // X position
                _ => 0, // pressure/aux channels unused
            };
            self.touch_output = (result & 0x0FFF) << 3;
            return 0; // the control byte's own readback is discarded by the reader
        }
        let byte = (self.touch_output >> 8) as u8;
        self.touch_output <<= 8;
        byte
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

    /// Drive the touchscreen X and Y channels and reconstruct the 12-bit ADC value
    /// the reader recovers as `(b0 << 5) | (b1 >> 3)`.
    #[test]
    fn touchscreen_reports_position_adc() {
        let mut spi = Spi::new();
        spi.set_touch(Some((0x123, 0x456)));
        let hold = (DEVICE_TOUCH << 8) | (1 << 11) | (1 << 15);
        let read_channel = |spi: &mut Spi, ctrl: u16| -> u16 {
            spi.write_cnt(hold);
            spi.write_data(ctrl); // control byte (bit7 start + channel)
            spi.write_data(0);
            let b0 = spi.read_data();
            spi.write_data(0);
            let b1 = spi.read_data();
            spi.write_cnt(0);
            (b0 << 5) | (b1 >> 3)
        };
        assert_eq!(read_channel(&mut spi, 0x80 | (5 << 4)), 0x123, "X channel");
        assert_eq!(read_channel(&mut spi, 0x80 | (1 << 4)), 0x456, "Y channel");
    }

    /// With no pen down, a position conversion reads zero.
    #[test]
    fn touchscreen_pen_up_reads_zero() {
        let mut spi = Spi::new();
        let hold = (DEVICE_TOUCH << 8) | (1 << 11) | (1 << 15);
        spi.write_cnt(hold);
        spi.write_data(0x80 | (5 << 4));
        spi.write_data(0);
        let b0 = spi.read_data();
        spi.write_data(0);
        let b1 = spi.read_data();
        assert_eq!((b0 << 5) | (b1 >> 3), 0);
    }

    /// The power chip powers up with its hardware defaults, not zeroed — register 0
    /// reports the sound amplifier (bit 0) and backlights (bits 2-3) already enabled.
    /// A direct-booted game reads these before writing its own, and an IPC boot
    /// handshake checks the sound-amplifier bit, so a zeroed register 0 stalls it.
    #[test]
    fn power_management_powers_up_with_hardware_defaults() {
        let mut spi = Spi::new();
        let hold = (DEVICE_POWER << 8) | (1 << 11) | (1 << 15);
        let read_reg = |spi: &mut Spi, reg: u8| -> u8 {
            spi.write_cnt(hold);
            spi.write_data((reg | 0x80) as u16); // read command
            spi.write_data(0); // clock the byte out
            let v = spi.read_data() as u8;
            spi.write_cnt(0);
            v
        };
        assert_eq!(read_reg(&mut spi, 0), 0x0D); // sound amp + both backlights
        assert_eq!(read_reg(&mut spi, 0) & 1, 1); // sound-amplifier-enable bit set
        assert_eq!(read_reg(&mut spi, 2), 0x01);
        assert_eq!(read_reg(&mut spi, 4), 0x03);
    }

    #[test]
    fn power_management_register_round_trips() {
        let mut spi = Spi::new();
        let hold = (DEVICE_POWER << 8) | (1 << 11) | (1 << 15);
        // Write 0x2A to power register 3 (command byte: register in bits 0-6, held).
        spi.write_cnt(hold);
        spi.write_data(3); // register 3, write
        spi.write_data(0x2A);
        spi.write_cnt(1 << 15); // release the bus mid-way is fine; re-select to read
        spi.write_cnt(0); // fully deselect

        // Read it back (command byte with bit 7 = read).
        spi.write_cnt(hold);
        spi.write_data(3 | 0x80); // register 3, read
        spi.write_data(0); // clock the data out
        assert_eq!(spi.read_data(), 0x2A);
    }
}
