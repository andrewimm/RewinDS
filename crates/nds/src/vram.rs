//! VRAM and its bank-mapping engine.
//!
//! The DS has nine VRAM blocks (A–I, 656 KB total) that firmware routes to
//! different purposes through the `VRAMCNT_x` registers — each with an `MST`
//! (what it is used as), an `OFS` (where within that use), and an enable bit. A
//! CPU access to the `0x0600_0000` region resolves to whichever enabled block is
//! mapped at that address.
//!
//! M2.2 models the CPU-visible mappings the 2D bring-up needs: plain "LCDC"
//! access (`MST` 0), and 2D Engine A background (`MST` 1) and object (`MST` 2)
//! VRAM. Texture, extended-palette, Engine B, and ARM7 mappings are later work.
//! Per hardware, 8-bit writes to VRAM are ignored.

/// Block sizes, indexed A=0 … I=8.
const SIZES: [usize; 9] = [
    0x20000, 0x20000, 0x20000, 0x20000, // A–D 128 KB
    0x10000, // E 64 KB
    0x4000, 0x4000, // F, G 16 KB
    0x8000, // H 32 KB
    0x4000, // I 16 KB
];

/// The fixed "LCDC" address of each block (`MST` 0).
const LCDC_BASE: [u32; 9] = [
    0x0680_0000, 0x0682_0000, 0x0684_0000, 0x0686_0000, // A–D
    0x0688_0000, // E
    0x0689_0000, 0x0689_4000, // F, G
    0x0689_8000, // H
    0x068A_0000, // I
];

/// VRAM plus the nine `VRAMCNT` registers.
pub struct Vram {
    banks: [Box<[u8]>; 9],
    vramcnt: [u8; 9],
}

impl Default for Vram {
    fn default() -> Self {
        Vram::new()
    }
}

impl Vram {
    pub fn new() -> Self {
        Vram {
            banks: std::array::from_fn(|i| vec![0u8; SIZES[i]].into_boxed_slice()),
            vramcnt: [0; 9],
        }
    }

    /// Set a block's `VRAMCNT` (bank 0=A … 8=I).
    pub fn set_control(&mut self, bank: usize, value: u8) {
        self.vramcnt[bank] = value;
    }

    pub fn control(&self, bank: usize) -> u8 {
        self.vramcnt[bank]
    }

    /// Read a 16-bit value directly from a block's storage (bank 0=A … 8=I),
    /// bypassing the CPU map — used by the display controller (VRAM display mode).
    pub fn block_read16(&self, bank: usize, offset: usize) -> u16 {
        let s = &self.banks[bank];
        let mask = s.len() - 1;
        u16::from_le_bytes([s[offset & mask], s[(offset + 1) & mask]])
    }

    /// Assemble a contiguous view of the 2D Engine A background VRAM region
    /// (`0x0600_0000`-`0x0607_FFFF`) into `out`, so the shared renderer — which
    /// reads a flat slice offset from the region base — sees the banked memory as
    /// one image. Blocks routed elsewhere contribute nothing.
    pub fn assemble_engine_a_bg(&self, out: &mut [u8]) {
        out.fill(0);
        for bank in 0..9 {
            if let Some((base, size)) = self.mapped_range(bank) {
                if (0x0600_0000..0x0608_0000).contains(&base) {
                    let off = (base - 0x0600_0000) as usize;
                    let n = (size as usize)
                        .min(self.banks[bank].len())
                        .min(out.len().saturating_sub(off));
                    out[off..off + n].copy_from_slice(&self.banks[bank][..n]);
                }
            }
        }
    }

    pub fn read(&self, addr: u32, bytes: u32) -> u32 {
        match self.resolve(addr) {
            Some((bank, off)) => {
                let s = &self.banks[bank];
                let mask = s.len() - 1;
                match bytes {
                    1 => s[off & mask] as u32,
                    2 => u16::from_le_bytes([s[off & mask], s[(off + 1) & mask]]) as u32,
                    _ => u32::from_le_bytes([
                        s[off & mask],
                        s[(off + 1) & mask],
                        s[(off + 2) & mask],
                        s[(off + 3) & mask],
                    ]),
                }
            }
            None => 0,
        }
    }

    pub fn write(&mut self, addr: u32, value: u32, bytes: u32) {
        // 8-bit writes to VRAM are ignored on the DS.
        if bytes == 1 {
            return;
        }
        if let Some((bank, off)) = self.resolve(addr) {
            let s = &mut self.banks[bank];
            let mask = s.len() - 1;
            let b = value.to_le_bytes();
            for i in 0..bytes as usize {
                s[(off + i) & mask] = b[i];
            }
        }
    }

    /// Resolve a CPU VRAM address to `(bank, offset)`, or `None` if no enabled
    /// block is mapped there.
    fn resolve(&self, addr: u32) -> Option<(usize, usize)> {
        for bank in 0..9 {
            if let Some((base, size)) = self.mapped_range(bank) {
                if addr >= base && addr < base + size {
                    return Some((bank, (addr - base) as usize));
                }
            }
        }
        None
    }

    /// The CPU address range a block occupies under its current `VRAMCNT`, for the
    /// mappings M2.2 models. `None` if disabled or in an unmodelled mode.
    fn mapped_range(&self, bank: usize) -> Option<(u32, u32)> {
        let cnt = self.vramcnt[bank];
        if cnt & 0x80 == 0 {
            return None; // disabled
        }
        let mst = cnt & 7;
        let ofs = ((cnt >> 3) & 3) as u32;
        let size = SIZES[bank] as u32;
        let range = match mst {
            // Plain LCDC access.
            0 => LCDC_BASE[bank],
            // 2D Engine A, BG VRAM.
            1 => match bank {
                0..=3 => 0x0600_0000 + 0x20000 * ofs, // A–D
                4 => 0x0600_0000,                      // E
                5 | 6 => 0x0600_0000 + 0x4000 * (ofs & 1) + 0x10000 * (ofs >> 1), // F, G
                _ => return None,
            },
            // 2D Engine A, OBJ VRAM.
            2 => match bank {
                0 | 1 => 0x0640_0000 + 0x20000 * (ofs & 1), // A, B
                4 => 0x0640_0000,                            // E
                5 | 6 => 0x0640_0000 + 0x4000 * (ofs & 1) + 0x10000 * (ofs >> 1), // F, G
                _ => return None,
            },
            // Texture / extended palette / Engine B / ARM7 — deferred.
            _ => return None,
        };
        Some((range, size))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENABLE: u8 = 0x80;

    #[test]
    fn lcdc_maps_each_block_to_its_fixed_address() {
        let mut vram = Vram::new();
        vram.set_control(2, ENABLE); // block C, MST 0 (LCDC) -> 0x0684_0000
        vram.write(0x0684_0000, 0xABCD, 2);
        assert_eq!(vram.read(0x0684_0000, 2), 0xABCD);
        // Nothing is mapped at block A's LCDC address (A disabled).
        assert_eq!(vram.read(0x0680_0000, 2), 0);
    }

    #[test]
    fn engine_a_bg_places_banks_by_offset() {
        let mut vram = Vram::new();
        // Block A -> Engine-A BG at OFS 0 (0x0600_0000); block B -> OFS 1
        // (0x0602_0000). They are disjoint 128 KB windows.
        vram.set_control(0, ENABLE | 1); // MST 1, OFS 0
        vram.set_control(1, ENABLE | 1 | (1 << 3)); // MST 1, OFS 1
        vram.write(0x0600_0000, 0x1111_1111, 4);
        vram.write(0x0602_0000, 0x2222_2222, 4);
        assert_eq!(vram.read(0x0600_0000, 4), 0x1111_1111);
        assert_eq!(vram.read(0x0602_0000, 4), 0x2222_2222);
    }

    #[test]
    fn engine_a_obj_maps_to_the_obj_window() {
        let mut vram = Vram::new();
        vram.set_control(0, ENABLE | 2); // block A, MST 2 (OBJ), OFS 0 -> 0x0640_0000
        vram.write(0x0640_0100, 0xDEAD_BEEF, 4);
        assert_eq!(vram.read(0x0640_0100, 4), 0xDEAD_BEEF);
    }

    #[test]
    fn eight_bit_writes_are_ignored() {
        let mut vram = Vram::new();
        vram.set_control(0, ENABLE); // A -> LCDC 0x0680_0000
        vram.write(0x0680_0000, 0xFF, 1);
        assert_eq!(vram.read(0x0680_0000, 1), 0);
    }
}
