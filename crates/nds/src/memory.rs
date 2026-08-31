//! The DS memory image and its two per-core address maps.
//!
//! The ARM9 and ARM7 see overlapping but distinct maps over one set of backing
//! stores. Main RAM is shared; the 32 KB Shared WRAM is split between the cores
//! by `WRAMCNT`; the ARM7 additionally has its own 64 KB WRAM; and the ARM9 alone
//! has the tightly coupled memories (ITCM/DTCM), whose placement comes from CP15.
//! This module resolves an `(core, address)` pair to a backing store and offset;
//! access width, timing, and coprocessor routing live in the bus adapter.
//!
//! Only the subset the M2.0 dual-core harness needs is modelled: Main RAM, Shared
//! WRAM (`WRAMCNT`), ARM7-WRAM, ITCM/DTCM, and stub BIOS regions. I/O, VRAM, the
//! cartridge, and mirroring refinements arrive with later milestones.

use crate::Cp15;

/// Which core is performing an access. The two cores have different maps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Core {
    /// The ARM946E-S (ARMv5TE), ~67 MHz.
    Arm9,
    /// The ARM7TDMI (ARMv4T), 33 MHz.
    Arm7,
}

impl Core {
    /// Index into per-core arrays (`Arm9` = 0, `Arm7` = 1).
    pub const fn index(self) -> usize {
        match self {
            Core::Arm9 => 0,
            Core::Arm7 => 1,
        }
    }
}

/// Whether an address falls in the `0x0600_0000` VRAM region (routed through the
/// bank-mapping engine rather than this module's stores).
pub const fn is_vram(addr: u32) -> bool {
    addr >> 24 == 0x06
}

pub const MAIN_RAM: usize = 4 * 1024 * 1024;
pub const SHARED_WRAM: usize = 32 * 1024;
pub const ARM7_WRAM: usize = 64 * 1024;
pub const ITCM: usize = 32 * 1024;
pub const DTCM: usize = 16 * 1024;
pub const ARM9_BIOS: usize = 32 * 1024;
pub const ARM7_BIOS: usize = 16 * 1024;
/// Standard palette RAM (Engine A/B BG+OBJ), `0x0500_0000`.
pub const PALETTE: usize = 2 * 1024;
/// Object attribute memory (Engine A/B), `0x0700_0000`.
pub const OAM: usize = 2 * 1024;

/// A backing store a resolved address lands in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Main,
    Shared,
    Arm7Wram,
    Itcm,
    Dtcm,
    Arm9Bios,
    Arm7Bios,
    Palette,
    Oam,
}

/// The DS memory image: all backing stores plus the `WRAMCNT` split.
pub struct Memory {
    pub main: Box<[u8]>,
    pub shared_wram: Box<[u8]>,
    pub arm7_wram: Box<[u8]>,
    pub itcm: Box<[u8]>,
    pub dtcm: Box<[u8]>,
    pub arm9_bios: Box<[u8]>,
    pub arm7_bios: Box<[u8]>,
    pub palette: Box<[u8]>,
    pub oam: Box<[u8]>,
    /// `WRAMCNT` (ARM9 `4000247h`): bits 0-1 select the Shared WRAM split.
    pub wramcnt: u8,
}

impl Default for Memory {
    fn default() -> Self {
        Memory::new()
    }
}

impl Memory {
    pub fn new() -> Self {
        Memory {
            main: vec![0; MAIN_RAM].into_boxed_slice(),
            shared_wram: vec![0; SHARED_WRAM].into_boxed_slice(),
            arm7_wram: vec![0; ARM7_WRAM].into_boxed_slice(),
            itcm: vec![0; ITCM].into_boxed_slice(),
            dtcm: vec![0; DTCM].into_boxed_slice(),
            arm9_bios: vec![0; ARM9_BIOS].into_boxed_slice(),
            arm7_bios: vec![0; ARM7_BIOS].into_boxed_slice(),
            palette: vec![0; PALETTE].into_boxed_slice(),
            oam: vec![0; OAM].into_boxed_slice(),
            wramcnt: 0,
        }
    }

    /// Load the ARM9 BIOS image (mapped read-only at `0xFFFF_0000`).
    pub fn load_bios9(&mut self, data: &[u8]) {
        let n = data.len().min(self.arm9_bios.len());
        self.arm9_bios[..n].copy_from_slice(&data[..n]);
    }

    /// Load the ARM7 BIOS image (mapped read-only at `0x0000_0000`).
    pub fn load_bios7(&mut self, data: &[u8]) {
        let n = data.len().min(self.arm7_bios.len());
        self.arm7_bios[..n].copy_from_slice(&data[..n]);
    }

    /// The 0x1048-byte KEY1 (Blowfish) key table embedded in the ARM7 BIOS at
    /// `0x30..0x1078` (GBATEK "DS Encryption by Gamecode/Idcode (KEY1)"). All-zero
    /// until a BIOS is loaded, which callers treat as "no keytable".
    pub fn key1_keytable(&self) -> &[u8] {
        &self.arm7_bios[crate::key1::KEYTABLE_BIOS_OFFSET
            ..crate::key1::KEYTABLE_BIOS_OFFSET + crate::key1::KEYTABLE_LEN]
    }

    // --- reads --------------------------------------------------------------

    pub fn read8(&self, core: Core, addr: u32, instruction: bool, cp15: &Cp15) -> u8 {
        match self.map(core, addr, instruction, cp15) {
            Some((slot, off)) => {
                let (s, mask) = self.slot(slot);
                s[off & mask]
            }
            None => 0, // open bus
        }
    }

    pub fn read16(&self, core: Core, addr: u32, instruction: bool, cp15: &Cp15) -> u16 {
        match self.map(core, addr, instruction, cp15) {
            Some((slot, off)) => {
                let (s, mask) = self.slot(slot);
                u16::from_le_bytes([s[off & mask], s[(off + 1) & mask]])
            }
            None => 0,
        }
    }

    pub fn read32(&self, core: Core, addr: u32, instruction: bool, cp15: &Cp15) -> u32 {
        match self.map(core, addr, instruction, cp15) {
            Some((slot, off)) => {
                let (s, mask) = self.slot(slot);
                u32::from_le_bytes([
                    s[off & mask],
                    s[(off + 1) & mask],
                    s[(off + 2) & mask],
                    s[(off + 3) & mask],
                ])
            }
            None => 0,
        }
    }

    // --- writes -------------------------------------------------------------

    pub fn write8(&mut self, core: Core, addr: u32, value: u8, cp15: &Cp15) {
        if let Some((slot, off)) = self.map(core, addr, false, cp15) {
            // 8-bit writes to palette and OAM are ignored on the DS (as with VRAM).
            if matches!(slot, Slot::Palette | Slot::Oam) {
                return;
            }
            if let Some((s, mask)) = self.slot_mut(slot) {
                s[off & mask] = value;
            }
        }
    }

    pub fn write16(&mut self, core: Core, addr: u32, value: u16, cp15: &Cp15) {
        if let Some((slot, off)) = self.map(core, addr, false, cp15) {
            if let Some((s, mask)) = self.slot_mut(slot) {
                let b = value.to_le_bytes();
                s[off & mask] = b[0];
                s[(off + 1) & mask] = b[1];
            }
        }
    }

    pub fn write32(&mut self, core: Core, addr: u32, value: u32, cp15: &Cp15) {
        if let Some((slot, off)) = self.map(core, addr, false, cp15) {
            if let Some((s, mask)) = self.slot_mut(slot) {
                let b = value.to_le_bytes();
                s[off & mask] = b[0];
                s[(off + 1) & mask] = b[1];
                s[(off + 2) & mask] = b[2];
                s[(off + 3) & mask] = b[3];
            }
        }
    }

    // --- resolution ---------------------------------------------------------

    /// Resolve `(core, addr)` to a backing store and offset. `instruction`
    /// selects instruction-fetch rules (ITCM may serve fetches, DTCM never does).
    /// `None` is open bus (or an unmodelled region, e.g. I/O, handled by the bus).
    fn map(&self, core: Core, addr: u32, instruction: bool, cp15: &Cp15) -> Option<(Slot, usize)> {
        // The ARM9's tightly coupled memories take priority over the main map.
        if core == Core::Arm9 {
            if cp15.itcm_enabled() && addr < cp15.itcm_size() {
                return Some((Slot::Itcm, addr as usize));
            }
            if !instruction && cp15.dtcm_enabled() {
                let base = cp15.dtcm_base();
                if addr >= base && addr < base.wrapping_add(cp15.dtcm_size()) {
                    return Some((Slot::Dtcm, (addr - base) as usize));
                }
            }
        }

        match addr >> 24 {
            0x02 => Some((Slot::Main, addr as usize)),
            0x03 => self.map_wram(core, addr),
            0x05 if core == Core::Arm9 => Some((Slot::Palette, addr as usize)),
            0x07 if core == Core::Arm9 => Some((Slot::Oam, addr as usize)),
            0x00 if core == Core::Arm7 => Some((Slot::Arm7Bios, addr as usize)),
            0xFF if core == Core::Arm9 && addr >= 0xFFFF_0000 => {
                Some((Slot::Arm9Bios, (addr - 0xFFFF_0000) as usize))
            }
            // I/O (0x04) and everything unmapped resolve to open bus here; the bus
            // adapter special-cases the handful of M2.0 I/O registers.
            _ => None,
        }
    }

    /// Map a `0x03xx_xxxx` address to Shared WRAM (per `WRAMCNT`) or, on the ARM7,
    /// to its own WRAM (either the dedicated `0x0380_0000` window or, when the
    /// core holds no Shared WRAM, the mirror the low window falls back to).
    fn map_wram(&self, core: Core, addr: u32) -> Option<(Slot, usize)> {
        match core {
            Core::Arm9 => {
                let (base, len) = self.shared_alloc(Core::Arm9)?;
                Some((Slot::Shared, base + (addr as usize % len)))
            }
            Core::Arm7 => {
                if addr >= 0x0380_0000 {
                    Some((Slot::Arm7Wram, addr as usize))
                } else if let Some((base, len)) = self.shared_alloc(Core::Arm7) {
                    Some((Slot::Shared, base + (addr as usize % len)))
                } else {
                    // With no Shared WRAM, the ARM7's low window mirrors ARM7-WRAM.
                    Some((Slot::Arm7Wram, addr as usize))
                }
            }
        }
    }

    /// The Shared WRAM sub-slice `(base_offset, len)` a core owns under the
    /// current `WRAMCNT`, or `None` when it holds none.
    fn shared_alloc(&self, core: Core) -> Option<(usize, usize)> {
        const K16: usize = 16 * 1024;
        const K32: usize = 32 * 1024;
        match (self.wramcnt & 3, core) {
            (0, Core::Arm9) => Some((0, K32)),
            (0, Core::Arm7) => None,
            (1, Core::Arm9) => Some((K16, K16)), // second half
            (1, Core::Arm7) => Some((0, K16)),   // first half
            (2, Core::Arm9) => Some((0, K16)),   // first half
            (2, Core::Arm7) => Some((K16, K16)), // second half
            (3, Core::Arm9) => None,
            (3, Core::Arm7) => Some((0, K32)),
            _ => unreachable!(),
        }
    }

    /// A backing store slice and its power-of-two address mask (for mirroring).
    fn slot(&self, slot: Slot) -> (&[u8], usize) {
        let s: &[u8] = match slot {
            Slot::Main => &self.main,
            Slot::Shared => &self.shared_wram,
            Slot::Arm7Wram => &self.arm7_wram,
            Slot::Itcm => &self.itcm,
            Slot::Dtcm => &self.dtcm,
            Slot::Arm9Bios => &self.arm9_bios,
            Slot::Arm7Bios => &self.arm7_bios,
            Slot::Palette => &self.palette,
            Slot::Oam => &self.oam,
        };
        (s, s.len() - 1)
    }

    /// The writable backing store for `slot`, or `None` for read-only regions
    /// (the BIOS stubs).
    fn slot_mut(&mut self, slot: Slot) -> Option<(&mut [u8], usize)> {
        let s: &mut [u8] = match slot {
            Slot::Main => &mut self.main,
            Slot::Shared => &mut self.shared_wram,
            Slot::Arm7Wram => &mut self.arm7_wram,
            Slot::Itcm => &mut self.itcm,
            Slot::Dtcm => &mut self.dtcm,
            Slot::Palette => &mut self.palette,
            Slot::Oam => &mut self.oam,
            Slot::Arm9Bios | Slot::Arm7Bios => return None,
        };
        let mask = s.len() - 1;
        Some((s, mask))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_ram_is_shared_and_mirrors() {
        let cp15 = Cp15::new();
        let mut m = Memory::new();
        m.write32(Core::Arm9, 0x0200_0100, 0xDEAD_BEEF, &cp15);
        // The ARM7 sees the same Main RAM.
        assert_eq!(m.read32(Core::Arm7, 0x0200_0100, false, &cp15), 0xDEAD_BEEF);
        // Main RAM mirrors every 4 MB.
        assert_eq!(m.read32(Core::Arm9, 0x0240_0100, false, &cp15), 0xDEAD_BEEF);
    }

    #[test]
    fn wramcnt_splits_shared_wram() {
        let cp15 = Cp15::new();
        let mut m = Memory::new();
        // Mode 2: ARM9 = first 16K, ARM7 = second 16K — disjoint.
        m.wramcnt = 2;
        m.write32(Core::Arm9, 0x0300_0000, 0x1111_1111, &cp15);
        m.write32(Core::Arm7, 0x0300_0000, 0x2222_2222, &cp15);
        assert_eq!(m.read32(Core::Arm9, 0x0300_0000, false, &cp15), 0x1111_1111);
        assert_eq!(m.read32(Core::Arm7, 0x0300_0000, false, &cp15), 0x2222_2222);

        // Mode 0: all 32K to the ARM9; the ARM7's low window mirrors ARM7-WRAM,
        // so it does not see the ARM9's Shared WRAM.
        m.wramcnt = 0;
        m.write32(Core::Arm9, 0x0300_0000, 0xAAAA_AAAA, &cp15);
        assert_ne!(m.read32(Core::Arm7, 0x0300_0000, false, &cp15), 0xAAAA_AAAA);
    }

    #[test]
    fn arm7_wram_is_private() {
        let cp15 = Cp15::new();
        let mut m = Memory::new();
        m.write32(Core::Arm7, 0x0380_0000, 0xCAFE_0007, &cp15);
        assert_eq!(m.read32(Core::Arm7, 0x0380_0000, false, &cp15), 0xCAFE_0007);
        // The ARM9 has no ARM7-WRAM in its map.
        assert_eq!(m.read32(Core::Arm9, 0x0380_0000, false, &cp15), 0);
    }

    #[test]
    fn tcm_appears_only_when_cp15_enables_it() {
        let mut cp15 = Cp15::new();
        let mut m = Memory::new();
        // Enable ITCM (control bit 18) with a 32 KB virtual size (N=6 in bits 1-5).
        cp15.write(0, 9, 1, 1, 0x0C); // ITCM size register
        cp15.write(0, 1, 0, 0, 1 << 18); // control: ITCM enable
        m.write32(Core::Arm9, 0x0000_0000, 0x1DC5_1DC5, &cp15);
        assert_eq!(m.read32(Core::Arm9, 0x0000_0000, true, &cp15), 0x1DC5_1DC5);
        // The ARM7 has no ITCM; address 0 is its BIOS (read-only, zero).
        assert_eq!(m.read32(Core::Arm7, 0x0000_0000, true, &cp15), 0);
    }

    #[test]
    fn dtcm_relocates_to_its_configured_base() {
        let mut cp15 = Cp15::new();
        let mut m = Memory::new();
        // DTCM base 0x0300_0000, 16 KB virtual size (N=5), enabled (control bit 16).
        cp15.write(0, 9, 1, 0, 0x0300_0000 | 0x0A);
        cp15.write(0, 1, 0, 0, 1 << 16);
        m.write32(Core::Arm9, 0x0300_0000, 0x0D7C_0D7C, &cp15);
        // A data read at the DTCM base hits DTCM, not the Shared WRAM underneath.
        assert_eq!(m.read32(Core::Arm9, 0x0300_0000, false, &cp15), 0x0D7C_0D7C);
        // DTCM never serves instruction fetches — a fetch there falls through to
        // the map underneath (empty here, since the ARM9 holds no Shared WRAM by
        // default), so it does not read the DTCM contents.
        assert_ne!(
            m.read32(Core::Arm9, 0x0300_0000, true, &cp15),
            m.read32(Core::Arm9, 0x0300_0000, false, &cp15)
        );
    }
}
