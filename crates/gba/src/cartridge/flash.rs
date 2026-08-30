//! Flash backup memory (64 KiB / 128 KiB) and its command protocol.
//!
//! Unlike SRAM, Flash is driven by a command state machine: the game writes an
//! unlock sequence (`0xAA` to `0x5555`, `0x55` to `0x2AAA`) followed by a command
//! byte to `0x5555`. Commands select chip-identify mode, program a byte, erase a
//! 4 KiB sector or the whole chip, or — on 128 KiB parts — switch which 64 KiB
//! bank the region maps. Reads return either the stored data or, in identify
//! mode, the manufacturer/device ID that games probe to learn the chip size.

/// A 4 KiB erase sector.
const SECTOR_SIZE: usize = 0x1000;
/// One bank is 64 KiB.
const BANK_SIZE: usize = 0x1_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlashSize {
    /// 64 KiB, one bank.
    K64,
    /// 128 KiB, two banks selected by the bank command.
    K128,
}

impl FlashSize {
    fn bytes(self) -> usize {
        match self {
            FlashSize::K64 => 0x1_0000,
            FlashSize::K128 => 0x2_0000,
        }
    }

    /// (manufacturer, device) ID reported in identify mode. These match the parts
    /// emulators conventionally present for each size (Panasonic 64K, Sanyo 128K),
    /// which is what games check to size their saves.
    fn chip_id(self) -> (u8, u8) {
        match self {
            FlashSize::K64 => (0x32, 0x1B),  // Panasonic MN63F805MNP
            FlashSize::K128 => (0x62, 0x13), // Sanyo LE26FV10N1TS
        }
    }
}

/// Where the command state machine is between the pieces of a multi-write command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Awaiting the first unlock write (`0xAA` to `0x5555`).
    Ready,
    /// Saw `0xAA` to `0x5555`; awaiting `0x55` to `0x2AAA`.
    Unlock,
    /// Unlocked; awaiting a command byte at `0x5555`.
    Command,
    /// After command `0xA0`: the next write programs a data byte.
    WriteByte,
    /// After command `0xB0`: the next write to `0x0000` selects the bank.
    BankSelect,
}

#[derive(Clone, Debug)]
pub struct Flash {
    data: Box<[u8]>,
    size: FlashSize,
    phase: Phase,
    /// True while an erase command is mid-sequence (a second unlock is required
    /// before the erase target is written).
    erase_armed: bool,
    /// Identify (autoselect) mode: reads return the chip ID.
    id_mode: bool,
    /// Active 64 KiB bank (128 KiB parts only).
    bank: usize,
    dirty: bool,
}

impl Flash {
    pub fn new(size: FlashSize) -> Self {
        Flash {
            data: vec![0xFF; size.bytes()].into_boxed_slice(),
            size,
            phase: Phase::Ready,
            erase_armed: false,
            id_mode: false,
            bank: 0,
            dirty: false,
        }
    }

    #[inline]
    fn offset(&self, addr: u32) -> usize {
        self.bank * BANK_SIZE + (addr as usize & (BANK_SIZE - 1))
    }

    pub fn read8(&self, addr: u32) -> u8 {
        if self.id_mode {
            let (manufacturer, device) = self.size.chip_id();
            match addr & 1 {
                0 => manufacturer,
                _ => device,
            }
        } else {
            self.data[self.offset(addr)]
        }
    }

    pub fn write8(&mut self, addr: u32, value: u8) {
        let cmd_addr = addr & 0xFFFF;
        match self.phase {
            Phase::WriteByte => {
                // Program a single byte, then return to idle.
                let i = self.offset(addr);
                if self.data[i] != value {
                    self.data[i] = value;
                    self.dirty = true;
                }
                self.phase = Phase::Ready;
                return;
            }
            Phase::BankSelect => {
                if cmd_addr == 0x0000 {
                    self.bank = (value as usize) & 1;
                }
                self.phase = Phase::Ready;
                return;
            }
            _ => {}
        }

        // Unlock sequence + command decode. Most commands are written to `0x5555`,
        // but a sector erase (`0x30`) is written to the target sector's address, so
        // the command phase accepts any address and lets the value disambiguate.
        match (self.phase, cmd_addr, value) {
            (Phase::Ready, 0x5555, 0xAA) => self.phase = Phase::Unlock,
            (Phase::Unlock, 0x2AAA, 0x55) => self.phase = Phase::Command,
            (Phase::Command, _, cmd) => self.run_command(addr, cmd),
            // Any other write breaks the sequence.
            _ => {
                self.phase = Phase::Ready;
                self.erase_armed = false;
            }
        }
    }

    fn run_command(&mut self, addr: u32, cmd: u8) {
        match cmd {
            0x90 => {
                self.id_mode = true;
                self.phase = Phase::Ready;
            }
            0xF0 => {
                self.id_mode = false;
                self.phase = Phase::Ready;
            }
            0xA0 => self.phase = Phase::WriteByte,
            0xB0 if self.size == FlashSize::K128 => self.phase = Phase::BankSelect,
            0x80 => {
                // Erase setup: a second unlock sequence follows, ending in the
                // erase target (chip `0x10` at `0x5555`, or sector `0x30`).
                self.erase_armed = true;
                self.phase = Phase::Ready;
            }
            0x10 if self.erase_armed => {
                for b in self.data.iter_mut() {
                    *b = 0xFF;
                }
                self.dirty = true;
                self.erase_armed = false;
                self.phase = Phase::Ready;
            }
            0x30 if self.erase_armed => {
                let base = self.offset(addr) & !(SECTOR_SIZE - 1);
                for b in self.data[base..base + SECTOR_SIZE].iter_mut() {
                    *b = 0xFF;
                }
                self.dirty = true;
                self.erase_armed = false;
                self.phase = Phase::Ready;
            }
            _ => {
                self.phase = Phase::Ready;
                self.erase_armed = false;
            }
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn load(&mut self, data: &[u8]) {
        let len = data.len().min(self.data.len());
        self.data[..len].copy_from_slice(&data[..len]);
        for b in self.data[len..].iter_mut() {
            *b = 0xFF;
        }
        self.dirty = false;
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the unlock sequence and a command byte.
    fn command(f: &mut Flash, cmd: u8) {
        f.write8(0x0E00_5555, 0xAA);
        f.write8(0x0E00_2AAA, 0x55);
        f.write8(0x0E00_5555, cmd);
    }

    #[test]
    fn identify_mode_reports_chip_id() {
        let mut f = Flash::new(FlashSize::K64);
        command(&mut f, 0x90);
        assert_eq!(f.read8(0x0E00_0000), 0x32); // manufacturer
        assert_eq!(f.read8(0x0E00_0001), 0x1B); // device
        command(&mut f, 0xF0); // exit
        assert_eq!(f.read8(0x0E00_0000), 0xFF); // back to data
    }

    #[test]
    fn program_a_byte() {
        let mut f = Flash::new(FlashSize::K64);
        command(&mut f, 0xA0);
        f.write8(0x0E00_1234, 0x42);
        assert_eq!(f.read8(0x0E00_1234), 0x42);
        assert!(f.dirty());
    }

    #[test]
    fn sector_erase_only_clears_its_sector() {
        let mut f = Flash::new(FlashSize::K64);
        command(&mut f, 0xA0);
        f.write8(0x0E00_0000, 0x00);
        command(&mut f, 0xA0);
        f.write8(0x0E00_2000, 0x00); // a different sector
        // Erase the sector containing 0x0000.
        command(&mut f, 0x80);
        f.write8(0x0E00_5555, 0xAA);
        f.write8(0x0E00_2AAA, 0x55);
        f.write8(0x0E00_0000, 0x30);
        assert_eq!(f.read8(0x0E00_0000), 0xFF); // erased
        assert_eq!(f.read8(0x0E00_2000), 0x00); // untouched
    }

    #[test]
    fn chip_erase_clears_everything() {
        let mut f = Flash::new(FlashSize::K64);
        command(&mut f, 0xA0);
        f.write8(0x0E00_0000, 0x00);
        command(&mut f, 0x80);
        f.write8(0x0E00_5555, 0xAA);
        f.write8(0x0E00_2AAA, 0x55);
        f.write8(0x0E00_5555, 0x10);
        assert_eq!(f.read8(0x0E00_0000), 0xFF);
    }

    #[test]
    fn bank_switch_selects_upper_64k_on_128k_part() {
        let mut f = Flash::new(FlashSize::K128);
        // Write 0x11 to bank 0 offset 0.
        command(&mut f, 0xA0);
        f.write8(0x0E00_0000, 0x11);
        // Switch to bank 1, write 0x22 at the same offset.
        command(&mut f, 0xB0);
        f.write8(0x0E00_0000, 0x01);
        command(&mut f, 0xA0);
        f.write8(0x0E00_0000, 0x22);
        assert_eq!(f.read8(0x0E00_0000), 0x22); // bank 1
        // Back to bank 0.
        command(&mut f, 0xB0);
        f.write8(0x0E00_0000, 0x00);
        assert_eq!(f.read8(0x0E00_0000), 0x11);
    }

    #[test]
    fn bank_command_ignored_on_64k_part() {
        let mut f = Flash::new(FlashSize::K64);
        command(&mut f, 0xB0);
        // The write that would select a bank is treated as a stray write.
        f.write8(0x0E00_0000, 0x01);
        command(&mut f, 0xA0);
        f.write8(0x0E00_0000, 0x33);
        assert_eq!(f.read8(0x0E00_0000), 0x33);
    }
}
