//! Direct boot: start a `.nds` image without the firmware sequence.
//!
//! Instead of emulating the full BIOS/firmware handshake, direct boot does what the
//! firmware would: copy the ARM9 and ARM7 binaries from the cartridge to their RAM
//! addresses, seed the CPU entry points and stacks, and jump. The header layout
//! follows GBATEK's "DS Cartridge Header". When a commercial ROM keeps its ARM9 boot
//! code in the encrypted secure area, [`crate::key1`] decrypts it first; the
//! KEY2 bus-transport cipher belongs to the (future) cartridge command controller,
//! not to loading the image.

/// The direct-boot fields of a `.nds` header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Gamecode at `[0Ch]` (the KEY1 idcode; `#### ` for homebrew).
    pub gamecode: u32,
    pub arm9_rom_offset: u32,
    pub arm9_entry: u32,
    pub arm9_ram_address: u32,
    pub arm9_size: u32,
    pub arm7_rom_offset: u32,
    pub arm7_entry: u32,
    pub arm7_ram_address: u32,
    pub arm7_size: u32,
}

/// Why a `.nds` image could not be booted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootError {
    /// The image is smaller than a `.nds` header.
    TooSmall,
    /// A binary's `rom_offset + size` runs past the end of the image.
    BinaryOutOfBounds,
    /// A binary's RAM destination is not a plausible load address.
    BadLoadAddress,
}

impl Header {
    /// The header size actually parsed (the direct-boot fields end at `0x40`).
    pub const MIN_LEN: usize = 0x160;

    /// Parse the direct-boot fields, validating that each binary lies within the
    /// image and targets a writable RAM region.
    pub fn parse(rom: &[u8]) -> Result<Header, BootError> {
        if rom.len() < Self::MIN_LEN {
            return Err(BootError::TooSmall);
        }
        let u32_at = |off: usize| u32::from_le_bytes(rom[off..off + 4].try_into().unwrap());
        let header = Header {
            gamecode: u32_at(0x0C),
            arm9_rom_offset: u32_at(0x20),
            arm9_entry: u32_at(0x24),
            arm9_ram_address: u32_at(0x28),
            arm9_size: u32_at(0x2C),
            arm7_rom_offset: u32_at(0x30),
            arm7_entry: u32_at(0x34),
            arm7_ram_address: u32_at(0x38),
            arm7_size: u32_at(0x3C),
        };

        for (offset, size) in [
            (header.arm9_rom_offset, header.arm9_size),
            (header.arm7_rom_offset, header.arm7_size),
        ] {
            let end = (offset as u64) + (size as u64);
            if end > rom.len() as u64 {
                return Err(BootError::BinaryOutOfBounds);
            }
        }
        for (addr, entry) in [
            (header.arm9_ram_address, header.arm9_entry),
            (header.arm7_ram_address, header.arm7_entry),
        ] {
            if !loadable(addr) || !loadable(entry) {
                return Err(BootError::BadLoadAddress);
            }
        }
        Ok(header)
    }
}

/// Whether `addr` is a plausible direct-boot RAM destination: Main RAM
/// (`0x0200_0000`) or the ARM7's WRAM window (`0x037F_8000`+).
fn loadable(addr: u32) -> bool {
    (0x0200_0000..0x0240_0000).contains(&addr) || (0x0370_0000..0x0400_0000).contains(&addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_direct_boot_fields() {
        let mut rom = vec![0u8; 0x8000];
        // ARM9: rom 0x4000, entry/ram 0x2000000, size 0x100.
        rom[0x20..0x24].copy_from_slice(&0x4000u32.to_le_bytes());
        rom[0x24..0x28].copy_from_slice(&0x0200_0000u32.to_le_bytes());
        rom[0x28..0x2C].copy_from_slice(&0x0200_0000u32.to_le_bytes());
        rom[0x2C..0x30].copy_from_slice(&0x100u32.to_le_bytes());
        // ARM7: rom 0x5000, entry/ram 0x2100000, size 0x80.
        rom[0x30..0x34].copy_from_slice(&0x5000u32.to_le_bytes());
        rom[0x34..0x38].copy_from_slice(&0x0210_0000u32.to_le_bytes());
        rom[0x38..0x3C].copy_from_slice(&0x0210_0000u32.to_le_bytes());
        rom[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());

        let h = Header::parse(&rom).unwrap();
        assert_eq!(h.arm9_ram_address, 0x0200_0000);
        assert_eq!(h.arm7_rom_offset, 0x5000);
        assert_eq!(h.arm7_size, 0x80);
    }

    #[test]
    fn rejects_truncated_and_out_of_bounds() {
        assert_eq!(Header::parse(&[0u8; 4]), Err(BootError::TooSmall));
        let mut rom = vec![0u8; 0x200];
        rom[0x24..0x28].copy_from_slice(&0x0200_0000u32.to_le_bytes());
        rom[0x28..0x2C].copy_from_slice(&0x0200_0000u32.to_le_bytes());
        rom[0x34..0x38].copy_from_slice(&0x0200_0000u32.to_le_bytes());
        rom[0x38..0x3C].copy_from_slice(&0x0200_0000u32.to_le_bytes());
        // ARM9 size runs past the image.
        rom[0x2C..0x30].copy_from_slice(&0x9000u32.to_le_bytes());
        assert_eq!(Header::parse(&rom), Err(BootError::BinaryOutOfBounds));
    }
}
