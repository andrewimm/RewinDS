//! A narrow, read-only view of the graphics memory a renderer samples.
//!
//! Render code never touches the full bus: it borrows only VRAM, palette RAM, and
//! OAM, immutably. That keeps renderers unable to mutate guest state, lets
//! provenance report exact source addresses, and decouples rendering from bus
//! timing (which does not apply to the PPU's own fetches).

use super::state::Color15;

/// Where a background's tile/map data and the OBJ tiles sit within the VRAM slice.
///
/// The GBA has one contiguous VRAM region with OBJ tiles at a fixed `0x1_0000`
/// offset and no global base. The DS Engine A adds a `DISPCNT`-derived character
/// and screen base to every background and keeps OBJ tiles in a separate region;
/// the DS caller assembles a contiguous view and reports the offsets here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VramLayout {
    /// Added to every background's `BGxCNT` character-block base.
    pub bg_char_base: u32,
    /// Added to every background's `BGxCNT` screen-block base.
    pub bg_screen_base: u32,
    /// Byte offset of the OBJ tile region within the VRAM slice.
    pub obj_tile_base: u32,
    /// Bytes an OAM tile-number step spans for 1D-mapped sprites. The GBA fixes this
    /// at 32; the DS scales it by `DISPCNT[20:21]` (32/64/128/256). Only the sprite's
    /// base tile number is scaled by this — tiles *within* a sprite stay 32-byte
    /// (4bpp) packed.
    pub obj_tile_boundary: u32,
    /// DS: 8bpp text backgrounds resolve through the extended palette (`DISPCNT` bit
    /// 30) instead of the standard palette. Always false on the GBA.
    pub bg_ext_palette: bool,
    /// DS: 8bpp sprites resolve through the OBJ extended palette (`DISPCNT` bit 31).
    /// Always false on the GBA.
    pub obj_ext_palette: bool,
    /// DS direct-color bitmap OBJ mapping (`DISPCNT` bit 6): 1D (`true`) vs 2D (`false`).
    /// 1D stores each sprite's bitmap contiguously; 2D windows into a fixed-width bitmap.
    pub obj_bitmap_1d: bool,
    /// DS 1D bitmap-OBJ boundary in bytes (`DISPCNT` bit 22: 128 or 256; Engine B is 128).
    /// The base texel address is `tile_number * boundary`.
    pub obj_bitmap_boundary: u32,
    /// DS 2D bitmap-OBJ source width (`DISPCNT` bit 5): `false` = 128 dots, `true` = 256.
    /// Sets the 2D tile-number X mask (0x0F / 0x1F) and the underlying bitmap row stride.
    pub obj_bitmap_wide: bool,
    /// Mask applied to `BGxCNT >> 2` for the character-base block. The GBA uses 2 bits
    /// (`0x3`, blocks 0-3); the DS uses 4 (`0xF`) — its BGxCNT bits 4-5 are the char
    /// base MSBs, which the GBA requires to be zero, so `0xF` stays GBA-identical.
    pub bg_char_base_mask: u32,
    /// Which console's `DISPCNT` mode field to interpret. The two consoles number
    /// their BG modes differently — the GBA's modes 3-5 are bitmap framebuffers, while
    /// the DS's 3-5 are text + affine/extended tiled backgrounds — so the per-mode
    /// layer dispatch forks on this.
    pub mode_semantics: ModeSemantics,
}

/// Whether a [`VramLayout`]'s mode field follows GBA or DS conventions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeSemantics {
    Gba,
    Ds,
}

impl VramLayout {
    /// The GBA layout: no global BG base, OBJ tiles at `0x1_0000`, 32-byte boundary,
    /// no extended palettes.
    pub const fn gba() -> Self {
        VramLayout {
            bg_char_base: 0,
            bg_screen_base: 0,
            obj_tile_base: 0x1_0000,
            obj_tile_boundary: 32,
            bg_ext_palette: false,
            obj_ext_palette: false,
            obj_bitmap_1d: false,
            obj_bitmap_boundary: 128,
            obj_bitmap_wide: false,
            bg_char_base_mask: 0x3,
            mode_semantics: ModeSemantics::Gba,
        }
    }
}

impl Default for VramLayout {
    fn default() -> Self {
        VramLayout::gba()
    }
}

/// Base guest address of palette RAM.
pub const PALETTE_BASE: u32 = 0x0500_0000;
/// Base guest address of VRAM.
pub const VRAM_BASE: u32 = 0x0600_0000;
/// Base guest address of OAM.
pub const OAM_BASE: u32 = 0x0700_0000;

/// An immutable view of the graphics memory regions. `bg_ext_palette` and
/// `obj_ext_palette` are the DS's VRAM-mapped extended palettes (empty on the GBA);
/// they are only consulted when the caller's [`VramLayout`] enables them.
pub struct PpuMemoryView<'a> {
    pub vram: &'a [u8],
    pub palette: &'a [u8],
    pub oam: &'a [u8],
    /// DS BG extended palette: 4 slots × 16 sub-palettes × 256 colors (32 KB).
    pub bg_ext_palette: &'a [u8],
    /// DS OBJ extended palette: 16 sub-palettes × 256 colors (8 KB).
    pub obj_ext_palette: &'a [u8],
}

impl<'a> PpuMemoryView<'a> {
    /// Build a view from the three standard region slices (no extended palettes —
    /// the GBA case). The machine crate owns the backing storage and supplies the
    /// slices, keeping this renderer free of any bus dependency.
    pub fn new(vram: &'a [u8], palette: &'a [u8], oam: &'a [u8]) -> Self {
        PpuMemoryView {
            vram,
            palette,
            oam,
            bg_ext_palette: &[],
            obj_ext_palette: &[],
        }
    }

    /// Attach the DS extended palettes to a view.
    pub fn with_ext_palettes(mut self, bg: &'a [u8], obj: &'a [u8]) -> Self {
        self.bg_ext_palette = bg;
        self.obj_ext_palette = obj;
        self
    }

    /// A DS BG extended-palette color: `slot` (0-3) selects the 8 KB block, `subpal`
    /// (0-15, from the tilemap entry) the 256-color sub-palette, `index` the color.
    #[inline]
    pub fn bg_ext15(&self, slot: usize, subpal: usize, index: usize) -> Color15 {
        self.ext15(self.bg_ext_palette, slot * 0x2000 + subpal * 0x200 + index * 2)
    }

    /// A DS OBJ extended-palette color: `subpal` (0-15, from OAM attr2) selects the
    /// 256-color sub-palette, `index` the color.
    #[inline]
    pub fn obj_ext15(&self, subpal: usize, index: usize) -> Color15 {
        self.ext15(self.obj_ext_palette, subpal * 0x200 + index * 2)
    }

    #[inline]
    fn ext15(&self, pal: &[u8], off: usize) -> Color15 {
        Color15(u16::from_le_bytes([
            pal.get(off).copied().unwrap_or(0),
            pal.get(off + 1).copied().unwrap_or(0),
        ]))
    }

    /// Read a little-endian halfword from VRAM at byte offset `off`.
    #[inline]
    pub fn vram16(&self, off: usize) -> u16 {
        u16::from_le_bytes([self.vram[off], self.vram[off + 1]])
    }

    /// Read a VRAM byte, returning 0 past the end of the region. Tiled backgrounds
    /// can compute addresses beyond VRAM when misconfigured; this keeps that from
    /// panicking (hardware would read stale/other memory).
    #[inline]
    pub fn vram_u8(&self, off: usize) -> u8 {
        self.vram.get(off).copied().unwrap_or(0)
    }

    /// Read a little-endian VRAM halfword, guarded like [`Self::vram_u8`].
    #[inline]
    pub fn vram_u16(&self, off: usize) -> u16 {
        u16::from_le_bytes([self.vram_u8(off), self.vram_u8(off + 1)])
    }

    /// Read palette entry `index` as a 15-bit color (each entry is 2 bytes).
    #[inline]
    pub fn palette15(&self, index: usize) -> Color15 {
        let off = index * 2;
        Color15(u16::from_le_bytes([self.palette[off], self.palette[off + 1]]))
    }

    /// Read a little-endian OAM halfword at byte offset `off`.
    #[inline]
    pub fn oam16(&self, off: usize) -> u16 {
        u16::from_le_bytes([self.oam[off], self.oam[off + 1]])
    }
}

/// A standalone owner of the three graphics regions (GBA sizes), for tests that
/// used to build a view from the bus `Memory`. Mutate the public fields, then
/// borrow a [`PpuMemoryView`] with [`Self::view`].
#[cfg(test)]
pub struct TestMemory {
    pub vram: Vec<u8>,
    pub palette: Vec<u8>,
    pub oam: Vec<u8>,
}

#[cfg(test)]
impl Default for TestMemory {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl TestMemory {
    pub fn new() -> Self {
        TestMemory {
            vram: vec![0; 0x1_8000],
            palette: vec![0; 0x400],
            oam: vec![0; 0x400],
        }
    }

    pub fn view(&self) -> PpuMemoryView<'_> {
        PpuMemoryView::new(&self.vram, &self.palette, &self.oam)
    }
}
