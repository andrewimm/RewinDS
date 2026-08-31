//! The backup-memory device: the save chip behind the GamePak SRAM region.

use super::eeprom::Eeprom;
use super::flash::{Flash, FlashSize};
use super::sram::Sram;

/// A cartridge's save chip. SRAM and Flash sit in the `0x0E000000` byte-bus region
/// and are driven through [`read8`](Backup::read8)/[`write8`](Backup::write8);
/// EEPROM instead rides the upper GamePak window as a bit stream (see [`Eeprom`]).
#[derive(Clone, Debug)]
pub enum Backup {
    /// No save chip: reads float to all-ones, writes are dropped.
    None,
    Sram(Sram),
    Flash(Flash),
    Eeprom(Eeprom),
}

impl Backup {
    pub fn sram() -> Self {
        Backup::Sram(Sram::new())
    }

    pub fn flash(size: FlashSize) -> Self {
        Backup::Flash(Flash::new(size))
    }

    pub fn eeprom() -> Self {
        Backup::Eeprom(Eeprom::new())
    }

    /// Read a byte from the `0x0E000000` region (an 8-bit bus). An absent chip —
    /// and EEPROM, which does not answer here — reads all-ones.
    pub fn read8(&self, addr: u32) -> u8 {
        match self {
            Backup::None | Backup::Eeprom(_) => 0xFF,
            Backup::Sram(s) => s.read8(addr),
            Backup::Flash(f) => f.read8(addr),
        }
    }

    pub fn write8(&mut self, addr: u32, value: u8) {
        match self {
            Backup::None | Backup::Eeprom(_) => {}
            Backup::Sram(s) => s.write8(addr, value),
            Backup::Flash(f) => f.write8(addr, value),
        }
    }

    /// The raw chip contents, for writing a `.sav`. Empty when absent.
    pub fn bytes(&self) -> &[u8] {
        match self {
            Backup::None => &[],
            Backup::Sram(s) => s.bytes(),
            Backup::Flash(f) => f.bytes(),
            Backup::Eeprom(e) => e.bytes(),
        }
    }

    /// Restore contents from a loaded `.sav`.
    pub fn load(&mut self, data: &[u8]) {
        match self {
            Backup::None => {}
            Backup::Sram(s) => s.load(data),
            Backup::Flash(f) => f.load(data),
            Backup::Eeprom(e) => e.load(data),
        }
    }

    /// Whether the chip has unsaved writes since the last flush.
    pub fn dirty(&self) -> bool {
        match self {
            Backup::None => false,
            Backup::Sram(s) => s.dirty(),
            Backup::Flash(f) => f.dirty(),
            Backup::Eeprom(e) => e.dirty(),
        }
    }

    pub fn clear_dirty(&mut self) {
        match self {
            Backup::None => {}
            Backup::Sram(s) => s.clear_dirty(),
            Backup::Flash(f) => f.clear_dirty(),
            Backup::Eeprom(e) => e.clear_dirty(),
        }
    }
}
