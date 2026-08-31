//! A narrow, read-only view of the graphics memory a renderer samples.
//!
//! Render code never touches the full bus: it borrows only VRAM, palette RAM, and
//! OAM, immutably. That keeps renderers unable to mutate guest state, lets
//! provenance report exact source addresses, and decouples rendering from bus
//! timing (which does not apply to the PPU's own fetches).

use super::state::Color15;

/// Base guest address of palette RAM.
pub const PALETTE_BASE: u32 = 0x0500_0000;
/// Base guest address of VRAM.
pub const VRAM_BASE: u32 = 0x0600_0000;
/// Base guest address of OAM.
pub const OAM_BASE: u32 = 0x0700_0000;

/// An immutable view of the three graphics memory regions.
pub struct PpuMemoryView<'a> {
    pub vram: &'a [u8],
    pub palette: &'a [u8],
    pub oam: &'a [u8],
}

impl<'a> PpuMemoryView<'a> {
    /// Build a view from the three region slices. The machine crate owns the
    /// backing storage (its bus `Memory`, or the DS's banked VRAM) and supplies
    /// the slices, keeping this renderer free of any bus dependency.
    pub fn new(vram: &'a [u8], palette: &'a [u8], oam: &'a [u8]) -> Self {
        PpuMemoryView { vram, palette, oam }
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
