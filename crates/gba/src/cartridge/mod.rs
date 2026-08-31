//! The GamePak cartridge: its ROM and save (backup) memory.
//!
//! A GBA cartridge is a flat ROM (mapped at `0x08000000`) plus at most one backup
//! chip — SRAM, Flash, or EEPROM — used for saves. There are no bank-switching
//! mappers. The save type is declared by an ID string in the ROM and detected at
//! load; the ROM is a plain byte array read directly by the bus, while the backup
//! is a stateful device (see [`Backup`]).

mod backup;
mod eeprom;
mod flash;
mod sram;

pub use backup::Backup;
pub use eeprom::Eeprom;
pub use flash::{Flash, FlashSize};
pub use sram::Sram;

/// The kind of save chip a cartridge carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveType {
    None,
    Sram,
    Flash64,
    Flash128,
    /// EEPROM (512 B, 6-bit address). The default until a 14-bit command upgrades it.
    Eeprom512,
    /// EEPROM (8 KiB, 14-bit address).
    Eeprom8k,
}

impl SaveType {
    /// The chip's size in bytes (0 when absent/unmodeled).
    pub fn backup_size(self) -> usize {
        match self {
            SaveType::None => 0,
            SaveType::Sram => sram::SRAM_SIZE,
            SaveType::Flash64 => 0x1_0000,
            SaveType::Flash128 => 0x2_0000,
            SaveType::Eeprom512 => 0x200,
            SaveType::Eeprom8k => 0x2000,
        }
    }

    /// A stable short name, used in the `.sav.meta` sidecar and diagnostics.
    pub fn name(self) -> &'static str {
        match self {
            SaveType::None => "NONE",
            SaveType::Sram => "SRAM",
            SaveType::Flash64 => "FLASH512",
            SaveType::Flash128 => "FLASH1M",
            SaveType::Eeprom512 => "EEPROM512",
            SaveType::Eeprom8k => "EEPROM8K",
        }
    }

    /// Parse a name produced by [`SaveType::name`] (for a sidecar override).
    pub fn from_name(s: &str) -> Option<SaveType> {
        Some(match s {
            "NONE" => SaveType::None,
            "SRAM" => SaveType::Sram,
            "FLASH512" => SaveType::Flash64,
            "FLASH1M" => SaveType::Flash128,
            "EEPROM512" => SaveType::Eeprom512,
            "EEPROM8K" => SaveType::Eeprom8k,
            _ => return None,
        })
    }

    fn make_backup(self) -> Backup {
        match self {
            SaveType::None => Backup::None,
            SaveType::Sram => Backup::sram(),
            SaveType::Flash64 => Backup::flash(FlashSize::K64),
            SaveType::Flash128 => Backup::flash(FlashSize::K128),
            SaveType::Eeprom512 | SaveType::Eeprom8k => Backup::eeprom(),
        }
    }
}

/// Scan a ROM for its save-type ID string. The Nintendo SDK writes one of a fixed
/// set of strings; longer/more-specific names are checked first.
pub fn detect_save_type(rom: &[u8]) -> SaveType {
    let has = |needle: &[u8]| rom.windows(needle.len()).any(|w| w == needle);
    if has(b"EEPROM_V") {
        // Size (512 B vs 8 KiB) is only known once the game issues a read of a
        // given address width; default to the smaller until then.
        SaveType::Eeprom512
    } else if has(b"FLASH1M_V") {
        SaveType::Flash128
    } else if has(b"FLASH512_V") || has(b"FLASH_V") {
        SaveType::Flash64
    } else if has(b"SRAM_V") || has(b"SRAM_F_V") {
        SaveType::Sram
    } else {
        SaveType::None
    }
}

/// A loaded cartridge: flat ROM plus its backup chip.
#[derive(Clone, Debug)]
pub struct Cartridge {
    /// The ROM image, read directly by the bus over `0x08000000..=0x0DFFFFFF`.
    pub rom: Vec<u8>,
    /// The save chip, behind the `0x0E000000` region.
    pub backup: Backup,
    save_type: SaveType,
}

impl Default for Cartridge {
    fn default() -> Self {
        // A bare cartridge (no ROM loaded — e.g. a test fixture) exposes plain
        // SRAM so the save region is usable without a detection pass.
        Cartridge { rom: Vec::new(), backup: Backup::sram(), save_type: SaveType::Sram }
    }
}

impl Cartridge {
    /// Load a ROM, detecting its save type and provisioning a matching backup.
    pub fn load_rom(&mut self, rom: Vec<u8>) {
        self.save_type = detect_save_type(&rom);
        self.backup = self.save_type.make_backup();
        self.rom = rom;
    }

    /// The detected (or overridden) save type. For EEPROM the reported size tracks
    /// the chip's live width, which is only pinned down once the game issues its
    /// first (6- or 14-bit) command — so the `.sav`/`.meta` reflect reality.
    pub fn save_type(&self) -> SaveType {
        if let Backup::Eeprom(e) = &self.backup {
            return if e.size() > 0x200 { SaveType::Eeprom8k } else { SaveType::Eeprom512 };
        }
        self.save_type
    }

    /// The base of the EEPROM window in the upper GamePak region. Carts of 16 MiB or
    /// less answer across all of `0x0D000000..=0x0DFFFFFF`; larger carts (whose ROM
    /// reaches into that region) confine EEPROM to the top 256 bytes.
    fn eeprom_window_start(&self) -> u32 {
        if self.rom.len() > 0x0100_0000 {
            0x0DFF_FF00
        } else {
            0x0D00_0000
        }
    }

    /// Whether `addr` (already region-`0x0D`) selects the EEPROM chip.
    pub fn is_eeprom_at(&self, addr: u32) -> bool {
        matches!(self.backup, Backup::Eeprom(_))
            && addr >> 24 == 0x0D
            && addr >= self.eeprom_window_start()
    }

    /// Clock one serial bit out of the EEPROM (result in D0).
    pub fn eeprom_read(&mut self) -> u16 {
        match &mut self.backup {
            Backup::Eeprom(e) => e.read_bit(),
            _ => 1,
        }
    }

    /// Clock one serial bit into the EEPROM (from D0 of a DMA write).
    pub fn eeprom_write(&mut self, bit: bool) {
        if let Backup::Eeprom(e) = &mut self.backup {
            e.write_bit(bit);
        }
    }

    /// Force a save type, replacing the backup with a fresh chip of that type.
    /// Used to apply a sidecar override before restoring saved contents.
    pub fn set_save_type(&mut self, save_type: SaveType) {
        self.save_type = save_type;
        self.backup = save_type.make_backup();
    }

    /// The backup contents to persist to a `.sav` (empty when there is no chip).
    pub fn backup_bytes(&self) -> &[u8] {
        self.backup.bytes()
    }

    /// Restore backup contents from a loaded `.sav`.
    pub fn load_backup(&mut self, data: &[u8]) {
        self.backup.load(data);
    }

    /// Whether the backup has unsaved writes since the last flush.
    pub fn backup_dirty(&self) -> bool {
        self.backup.dirty()
    }

    pub fn clear_backup_dirty(&mut self) {
        self.backup.clear_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rom_with(id: &[u8]) -> Vec<u8> {
        let mut rom = vec![0u8; 0x200];
        rom[0x100..0x100 + id.len()].copy_from_slice(id);
        rom
    }

    #[test]
    fn detects_each_save_type() {
        assert_eq!(detect_save_type(&rom_with(b"SRAM_V113")), SaveType::Sram);
        assert_eq!(detect_save_type(&rom_with(b"FLASH_V123")), SaveType::Flash64);
        assert_eq!(detect_save_type(&rom_with(b"FLASH512_V130")), SaveType::Flash64);
        assert_eq!(detect_save_type(&rom_with(b"FLASH1M_V102")), SaveType::Flash128);
        assert_eq!(detect_save_type(&rom_with(b"EEPROM_V120")), SaveType::Eeprom512);
        assert_eq!(detect_save_type(&rom_with(b"no id here")), SaveType::None);
    }

    #[test]
    fn load_rom_provisions_matching_backup() {
        let mut c = Cartridge::default();
        c.load_rom(rom_with(b"FLASH1M_V102"));
        assert_eq!(c.save_type(), SaveType::Flash128);
        assert_eq!(c.backup_bytes().len(), 0x2_0000);
    }

    #[test]
    fn none_backup_reads_all_ones_and_drops_writes() {
        let mut c = Cartridge::default();
        c.load_rom(rom_with(b"no save"));
        assert_eq!(c.save_type(), SaveType::None);
        c.backup.write8(0x0E00_0000, 0x42);
        assert_eq!(c.backup.read8(0x0E00_0000), 0xFF);
    }

    #[test]
    fn save_type_override_swaps_the_backup() {
        let mut c = Cartridge::default();
        c.load_rom(rom_with(b"no id"));
        c.set_save_type(SaveType::Sram);
        c.backup.write8(0x0E00_0000, 0x7E);
        assert_eq!(c.backup.read8(0x0E00_0000), 0x7E);
    }
}
