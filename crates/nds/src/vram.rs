//! VRAM and its bank-mapping engine.
//!
//! The DS has nine VRAM blocks (A–I, 656 KB total) that firmware routes to
//! different purposes through the `VRAMCNT_x` registers — each with an `MST`
//! (what it is used as), an `OFS` (where within that use), and an enable bit. A
//! CPU access to the `0x0600_0000` region resolves to whichever enabled block is
//! mapped at that address.
//!
//! The mapping is **core-aware**: the ARM9 sees the LCDC windows and the 2D-engine
//! background/object banks; the ARM7 sees only banks C/D allocated to it as work RAM
//! (`MST` 2), mapped into its own `0x0600_0000` space — never the engine banks. Per
//! hardware, 8-bit writes to VRAM are ignored.

use crate::memory::Core;

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
    0x0680_0000,
    0x0682_0000,
    0x0684_0000,
    0x0686_0000, // A–D
    0x0688_0000, // E
    0x0689_0000,
    0x0689_4000, // F, G
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

    /// `VRAMSTAT` (`0x4000240`, ARM7): bit 0/1 report whether bank C/D is currently
    /// allocated to the ARM7 as work RAM (enabled with `MST == 2`).
    pub fn vramstat(&self) -> u8 {
        let to_arm7 = |cnt: u8| (cnt & 0x80 != 0 && cnt & 0x7 == 2) as u8;
        to_arm7(self.vramcnt[2]) | (to_arm7(self.vramcnt[3]) << 1)
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
        self.assemble_region(out, 0x0600_0000, 0x0608_0000);
    }

    /// Assemble the Engine-A OBJ region (`0x0640_0000`-`0x0643_FFFF`, `MST 2`) into
    /// `out`, offset from the region base, for the shared renderer's OBJ tile fetch.
    pub fn assemble_engine_a_obj(&self, out: &mut [u8]) {
        self.assemble_region(out, 0x0640_0000, 0x0644_0000);
    }

    /// Assemble the Engine-B BG region (`0x0620_0000`, max 128 KB: banks C/H/I) into
    /// `out`, offset from the region base.
    pub fn assemble_engine_b_bg(&self, out: &mut [u8]) {
        self.assemble_region(out, 0x0620_0000, 0x0622_0000);
    }

    /// Assemble the Engine-B OBJ region (`0x0660_0000`, max 128 KB: banks D/I) into
    /// `out`, offset from the region base.
    pub fn assemble_engine_b_obj(&self, out: &mut [u8]) {
        self.assemble_region(out, 0x0660_0000, 0x0662_0000);
    }

    /// Assemble the Engine-A BG extended palette (32 KB = 4 slots × 8 KB) from the
    /// banks routed to it: E (`MST 4`) fills all four slots; F/G (`MST 4`) fill slots
    /// 0-1 (OFS=0) or 2-3 (OFS=1). Extended-palette VRAM is not CPU-addressable, so
    /// this reads the banks directly rather than through the address map.
    pub fn assemble_bg_ext_a(&self, out: &mut [u8]) {
        out.fill(0);
        for bank in [4usize, 5, 6] {
            let cnt = self.vramcnt[bank];
            if cnt & 0x80 == 0 || cnt & 7 != 4 {
                continue;
            }
            let ofs = ((cnt >> 3) & 3) as usize;
            let (dst, len) = if bank == 4 { (0, 0x8000) } else { ((ofs & 1) * 0x4000, 0x4000) };
            copy_into(out, dst, &self.banks[bank], len);
        }
    }

    /// Assemble the Engine-A OBJ extended palette (8 KB) from banks F/G (`MST 5`).
    pub fn assemble_obj_ext_a(&self, out: &mut [u8]) {
        out.fill(0);
        for bank in [5usize, 6] {
            let cnt = self.vramcnt[bank];
            if cnt & 0x80 != 0 && cnt & 7 == 5 {
                copy_into(out, 0, &self.banks[bank], 0x2000);
            }
        }
    }

    /// Assemble the Engine-B BG extended palette (32 KB) from bank H (`MST 2`).
    pub fn assemble_bg_ext_b(&self, out: &mut [u8]) {
        out.fill(0);
        let cnt = self.vramcnt[7];
        if cnt & 0x80 != 0 && cnt & 7 == 2 {
            copy_into(out, 0, &self.banks[7], 0x8000);
        }
    }

    /// Assemble the 3D texture image (512 KB = 4 slots × 128 KB) from banks A–D routed
    /// to texture (`MST 3`), each at `0x2_0000 * OFS`. Texture VRAM is not
    /// CPU-addressable, so this reads the banks directly. The rasterizer indexes into
    /// this by the `TEXIMAGE_PARAM` VRAM offset.
    pub fn assemble_texture_image(&self, out: &mut [u8]) {
        out.fill(0);
        for bank in 0..4usize {
            let cnt = self.vramcnt[bank];
            if cnt & 0x80 == 0 || cnt & 7 != 3 {
                continue;
            }
            let ofs = ((cnt >> 3) & 3) as usize;
            copy_into(out, ofs * 0x2_0000, &self.banks[bank], 0x2_0000);
        }
    }

    /// Assemble the 3D texture palette (`0x18000` = 6 slots × 16 KB) from banks routed
    /// to texture palette (`MST 3`): E fills slots 0-3 (its low 32 KB at offset 0);
    /// F/G each fill one 16 KB slot selected by OFS (slot `(OFS.0) + (OFS.1)·4`).
    pub fn assemble_texture_palette(&self, out: &mut [u8]) {
        out.fill(0);
        // Bank E: 64 KB at slot 0.
        if self.vramcnt[4] & 0x80 != 0 && self.vramcnt[4] & 7 == 3 {
            copy_into(out, 0, &self.banks[4], 0x1_0000);
        }
        for bank in [5usize, 6] {
            let cnt = self.vramcnt[bank];
            if cnt & 0x80 == 0 || cnt & 7 != 3 {
                continue;
            }
            let ofs = ((cnt >> 3) & 3) as usize;
            let slot = (ofs & 1) + ((ofs >> 1) & 1) * 4;
            copy_into(out, slot * 0x4000, &self.banks[bank], 0x4000);
        }
    }

    /// Assemble the Engine-B OBJ extended palette (8 KB) from bank I (`MST 3`).
    pub fn assemble_obj_ext_b(&self, out: &mut [u8]) {
        out.fill(0);
        let cnt = self.vramcnt[8];
        if cnt & 0x80 != 0 && cnt & 7 == 3 {
            copy_into(out, 0, &self.banks[8], 0x2000);
        }
    }

    /// Gather every enabled bank whose mapped base falls in `[lo, hi)` into `out`,
    /// each at `base - lo`. Banks routed elsewhere contribute nothing.
    fn assemble_region(&self, out: &mut [u8], lo: u32, hi: u32) {
        out.fill(0);
        for bank in 0..9 {
            // Engine assembly is the ARM9's view of VRAM.
            if let Some((base, size)) = self.mapped_range(Core::Arm9, bank) {
                if (lo..hi).contains(&base) {
                    let off = (base - lo) as usize;
                    let n = (size as usize)
                        .min(self.banks[bank].len())
                        .min(out.len().saturating_sub(off));
                    out[off..off + n].copy_from_slice(&self.banks[bank][..n]);
                }
            }
        }
    }

    pub fn read(&self, core: Core, addr: u32, bytes: u32) -> u32 {
        match self.resolve(core, addr) {
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

    pub fn write(&mut self, core: Core, addr: u32, value: u32, bytes: u32) {
        // 8-bit writes to VRAM are ignored on the DS.
        if bytes == 1 {
            return;
        }
        if let Some((bank, off)) = self.resolve(core, addr) {
            let s = &mut self.banks[bank];
            let mask = s.len() - 1;
            let b = value.to_le_bytes();
            for i in 0..bytes as usize {
                s[(off + i) & mask] = b[i];
            }
        }
    }

    /// Resolve a CPU VRAM address to `(bank, offset)` for `core`, or `None` if no
    /// enabled block is mapped there in that core's view.
    fn resolve(&self, core: Core, addr: u32) -> Option<(usize, usize)> {
        for bank in 0..9 {
            if let Some((base, size)) = self.mapped_range(core, bank) {
                if addr >= base && addr < base + size {
                    return Some((bank, (addr - base) as usize));
                }
            }
        }
        None
    }

    /// The CPU address range a block occupies under its current `VRAMCNT`, in `core`'s
    /// view. `None` if disabled or in a mode not visible to that core.
    fn mapped_range(&self, core: Core, bank: usize) -> Option<(u32, u32)> {
        let cnt = self.vramcnt[bank];
        if cnt & 0x80 == 0 {
            return None; // disabled
        }
        let mst = cnt & 7;
        let ofs = ((cnt >> 3) & 3) as u32;
        let size = SIZES[bank] as u32;
        // The ARM7 sees only banks C/D allocated to it as work RAM (MST 2), mapped into
        // its own 0x0600_0000..0x0640_0000 space (OFS picks the low/high 128 KB half). It
        // never sees the engine-mapped banks or the LCDC windows — those are the ARM9's.
        if core == Core::Arm7 {
            return match (bank, mst) {
                (2 | 3, 2) => Some((0x0600_0000 + 0x2_0000 * (ofs & 1), size)),
                _ => None,
            };
        }
        // ARM9 view. Banks C/D at MST 2 are the ARM7's work RAM, so they resolve to
        // `None` here (the `2 =>` arm below covers only the Engine-OBJ banks).
        let range = match mst {
            // Plain LCDC access.
            0 => LCDC_BASE[bank],
            // 2D Engine A BG (banks A–G), and 2D Engine B BG for banks H, I.
            1 => match bank {
                0..=3 => 0x0600_0000 + 0x20000 * ofs, // A–D
                4 => 0x0600_0000,                     // E
                5 | 6 => 0x0600_0000 + 0x4000 * (ofs & 1) + 0x10000 * (ofs >> 1), // F, G
                7 => 0x0620_0000,                     // H -> Engine B BG
                8 => 0x0620_8000,                     // I -> Engine B BG
                _ => return None,
            },
            // 2D Engine A OBJ (banks A, B, E–G), and 2D Engine B OBJ for bank I.
            2 => match bank {
                0 | 1 => 0x0640_0000 + 0x20000 * (ofs & 1), // A, B
                4 => 0x0640_0000,                           // E
                5 | 6 => 0x0640_0000 + 0x4000 * (ofs & 1) + 0x10000 * (ofs >> 1), // F, G
                8 => 0x0660_0000,                           // I -> Engine B OBJ
                _ => return None,
            },
            // 2D Engine B, via banks C (BG) and D (OBJ).
            4 => match bank {
                2 => 0x0620_0000, // C -> Engine B BG
                3 => 0x0660_0000, // D -> Engine B OBJ
                _ => return None, // E/F/G MST 4 = BG extended palette (Phase 3)
            },
            // Texture / extended palette / ARM7 — deferred.
            _ => return None,
        };
        Some((range, size))
    }
}

/// Copy up to `len` bytes of `src` into `out` at `dst`, clamped to both lengths.
fn copy_into(out: &mut [u8], dst: usize, src: &[u8], len: usize) {
    let n = len.min(src.len()).min(out.len().saturating_sub(dst));
    out[dst..dst + n].copy_from_slice(&src[..n]);
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENABLE: u8 = 0x80;

    #[test]
    fn lcdc_maps_each_block_to_its_fixed_address() {
        let mut vram = Vram::new();
        vram.set_control(2, ENABLE); // block C, MST 0 (LCDC) -> 0x0684_0000
        vram.write(Core::Arm9, 0x0684_0000, 0xABCD, 2);
        assert_eq!(vram.read(Core::Arm9, 0x0684_0000, 2), 0xABCD);
        // Nothing is mapped at block A's LCDC address (A disabled).
        assert_eq!(vram.read(Core::Arm9, 0x0680_0000, 2), 0);
    }

    #[test]
    fn engine_a_bg_places_banks_by_offset() {
        let mut vram = Vram::new();
        // Block A -> Engine-A BG at OFS 0 (0x0600_0000); block B -> OFS 1
        // (0x0602_0000). They are disjoint 128 KB windows.
        vram.set_control(0, ENABLE | 1); // MST 1, OFS 0
        vram.set_control(1, ENABLE | 1 | (1 << 3)); // MST 1, OFS 1
        vram.write(Core::Arm9, 0x0600_0000, 0x1111_1111, 4);
        vram.write(Core::Arm9, 0x0602_0000, 0x2222_2222, 4);
        assert_eq!(vram.read(Core::Arm9, 0x0600_0000, 4), 0x1111_1111);
        assert_eq!(vram.read(Core::Arm9, 0x0602_0000, 4), 0x2222_2222);
    }

    #[test]
    fn engine_a_obj_maps_to_the_obj_window() {
        let mut vram = Vram::new();
        vram.set_control(0, ENABLE | 2); // block A, MST 2 (OBJ), OFS 0 -> 0x0640_0000
        vram.write(Core::Arm9, 0x0640_0100, 0xDEAD_BEEF, 4);
        assert_eq!(vram.read(Core::Arm9, 0x0640_0100, 4), 0xDEAD_BEEF);
    }

    #[test]
    fn arm7_sees_only_its_wram_banks_not_the_engine_banks() {
        let mut vram = Vram::new();
        // Bank A -> Engine-A BG (ARM9's); bank C -> ARM7 work RAM (MST 2), OFS 0.
        vram.set_control(0, ENABLE | 1); // A: MST 1 (Engine A BG) @ 0x0600_0000
        vram.set_control(2, ENABLE | 2); // C: MST 2 (ARM7 WRAM) @ 0x0600_0000
        // The ARM9 writes into bank A at 0x0600_0000; the ARM7 writes into bank C at the
        // same address in its own view. The two must not alias.
        vram.write(Core::Arm9, 0x0600_0000, 0xAAAA_AAAA, 4);
        vram.write(Core::Arm7, 0x0600_0000, 0x7777_7777, 4);
        assert_eq!(vram.read(Core::Arm9, 0x0600_0000, 4), 0xAAAA_AAAA, "ARM9 sees bank A");
        assert_eq!(vram.read(Core::Arm7, 0x0600_0000, 4), 0x7777_7777, "ARM7 sees bank C");
        // The ARM7 cannot see the Engine-A OBJ window at all (no ARM7 mapping there).
        vram.set_control(1, ENABLE | 2); // B: MST 2 (Engine A OBJ) @ 0x0640_0000
        vram.write(Core::Arm9, 0x0640_0000, 0x1234_5678, 4);
        assert_eq!(vram.read(Core::Arm7, 0x0640_0000, 4), 0, "ARM7 has no OBJ-window view");
    }

    #[test]
    fn eight_bit_writes_are_ignored() {
        let mut vram = Vram::new();
        vram.set_control(0, ENABLE); // A -> LCDC 0x0680_0000
        vram.write(Core::Arm9, 0x0680_0000, 0xFF, 1);
        assert_eq!(vram.read(Core::Arm9, 0x0680_0000, 1), 0);
    }
}
