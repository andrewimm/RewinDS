//! Text (tiled, scrollable) backgrounds — video modes 0 and 1.
//!
//! A text background is a grid of 8×8 tiles indexed by a tilemap. Sampling a
//! screen pixel means: scroll and wrap to a background coordinate, pick the
//! tilemap entry (across up to four 32×32-tile screenblocks), decode the tile
//! number/flip/palette from it, read the tile's texel (4bpp or 8bpp), and resolve
//! it through the background palette. Texel 0 is transparent.

use crate::debug::explain::CandidateExplanation;
use crate::debug::provenance::{
    BackgroundId, BgColorMode, RejectionReason, SourceProvenance, TextBgProvenance,
};
use crate::debug::sink::ProvenanceSink;
use crate::memory::{PpuMemoryView, VramLayout, PALETTE_BASE, VRAM_BASE};
use crate::state::{CandidatePixel, LatchedState, LayerId, PixelFlags, Scratch};

/// Bytes per screenblock (32×32 tilemap entries × 2 bytes).
const SCREENBLOCK_SIZE: u32 = 0x800;
/// Bytes per character block (the tile-data base granularity).
const CHARBLOCK_SIZE: u32 = 0x4000;

/// The tile grid `(width, height)` for a `BGxCNT` screen-size field.
fn map_dimensions(size: u16) -> (usize, usize) {
    match size & 3 {
        0 => (32, 32),
        1 => (64, 32),
        2 => (32, 64),
        _ => (64, 64),
    }
}

fn background_id(bg: usize) -> BackgroundId {
    match bg {
        0 => BackgroundId::Bg0,
        1 => BackgroundId::Bg1,
        2 => BackgroundId::Bg2,
        _ => BackgroundId::Bg3,
    }
}

/// Render text background `bg`'s pixels for scanline `y` into its scratch line.
#[allow(clippy::too_many_arguments)]
pub fn render_text_scanline<S: ProvenanceSink>(
    bg: usize,
    y: u16,
    state: &LatchedState,
    mem: &PpuMemoryView,
    width: usize,
    layout: VramLayout,
    scratch: &mut Scratch,
    sink: &mut S,
) {
    if !state.bg_enabled(bg) {
        return;
    }
    let cnt = state.regs.bgcnt[bg];
    let priority = (cnt & 0x3) as u8;
    let char_base = layout.bg_char_base + ((cnt >> 2) as u32 & layout.bg_char_base_mask) * CHARBLOCK_SIZE;
    let is_8bpp = cnt & (1 << 7) != 0;
    let screen_base = layout.bg_screen_base + ((cnt >> 8) & 0x1F) as u32 * SCREENBLOCK_SIZE;
    let (tiles_w, tiles_h) = map_dimensions((cnt >> 14) & 0x3);
    let (bg_w, bg_h) = (tiles_w * 8, tiles_h * 8);
    let screenblocks_wide = tiles_w / 32;
    let hofs = state.regs.bg_hofs[bg] as usize;
    let vofs = state.regs.bg_vofs[bg] as usize;
    let (mosaic_x, mosaic_y, mosaic) = super::bg_mosaic_factors(state, bg);

    // The vertical coordinate is constant across the scanline; mosaic snaps it to
    // the top of its block.
    let y_snapped = y as usize - (y as usize % mosaic_y);
    let source_y = (y_snapped + vofs) & (bg_h - 1);
    let tile_y = source_y / 8;
    let in_tile_y = source_y % 8;
    let layer = LayerId::bg(bg);

    for x in 0..width {
        let x_snapped = x - (x % mosaic_x);
        let source_x = (x_snapped + hofs) & (bg_w - 1);
        let tile_x = source_x / 8;
        let in_tile_x = source_x % 8;

        // Locate the tilemap entry: which screenblock, then which cell in it.
        let screenblock = (tile_y / 32) * screenblocks_wide + (tile_x / 32);
        let cell = (tile_y % 32) * 32 + (tile_x % 32);
        let map_offset = screen_base as usize + screenblock * SCREENBLOCK_SIZE as usize + cell * 2;
        let entry = mem.vram_u16(map_offset);

        let tile_number = (entry & 0x3FF) as u32;
        let hflip = entry & (1 << 10) != 0;
        let vflip = entry & (1 << 11) != 0;
        let palette_bank = ((entry >> 12) & 0xF) as u8;

        // Flip the texel coordinate within the 8×8 tile.
        let tx = if hflip { 7 - in_tile_x } else { in_tile_x };
        let ty = if vflip { 7 - in_tile_y } else { in_tile_y };

        // Read the raw texel value (nibble for 4bpp, byte for 8bpp).
        let (bytes_per_tile, tile_byte_offset, texel) = if is_8bpp {
            let offset = (char_base + tile_number * 64) as usize + ty * 8 + tx;
            (64u32, offset, mem.vram_u8(offset))
        } else {
            let offset = (char_base + tile_number * 32) as usize + ty * 4 + tx / 2;
            let byte = mem.vram_u8(offset);
            let nibble = if tx & 1 == 0 { byte & 0xF } else { byte >> 4 };
            (32u32, offset, nibble)
        };

        let opaque = texel != 0;
        let (palette_entry, color_mode) = if is_8bpp {
            (texel as usize, BgColorMode::Bpp8)
        } else {
            (palette_bank as usize * 16 + texel as usize, BgColorMode::Bpp4 { palette_bank })
        };
        let color = if is_8bpp && layout.bg_ext_palette {
            // Extended palette: the tilemap entry's bits 12-15 select the 256-color
            // sub-palette within the BG's 8 KB slot. BG0/BG1 may borrow slot 2/3
            // (BGxCNT bit 13); BG2/BG3 use their own slot.
            let slot = if bg < 2 && cnt & (1 << 13) != 0 { bg + 2 } else { bg };
            mem.bg_ext15(slot, palette_bank as usize, texel as usize)
        } else {
            mem.palette15(palette_entry)
        };

        let flags = PixelFlags {
            mosaic,
            ..PixelFlags::default()
        };
        if opaque {
            scratch.bg[bg].pixels[x] = Some(CandidatePixel {
                color,
                layer,
                priority,
                flags,
            });
        }

        if sink.wants(x as u16) {
            let candidate = CandidatePixel {
                color,
                layer,
                priority,
                flags,
            };
            let tile_address = VRAM_BASE + char_base + tile_number * bytes_per_tile;
            sink.record_candidate(x as u16, layer, || CandidateExplanation {
                candidate,
                provenance: SourceProvenance::TextBg(TextBgProvenance {
                    bg: background_id(bg),
                    screen_x: x as u16,
                    screen_y: y,
                    source_x: source_x as u16,
                    source_y: source_y as u16,
                    map_address: VRAM_BASE + map_offset as u32,
                    map_entry: entry,
                    tile_number: tile_number as u16,
                    tile_address,
                    tile_x: tx as u8,
                    tile_y: ty as u8,
                    tile_byte_address: VRAM_BASE + tile_byte_offset as u32,
                    palette_index: texel,
                    palette_address: PALETTE_BASE + palette_entry as u32 * 2,
                    horizontal_flip: hflip,
                    vertical_flip: vflip,
                    color_mode,
                }),
                visible_after_window: opaque,
                rejection_reason: (!opaque)
                    .then_some(RejectionReason::TransparentPixel { palette_index: texel }),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::memory::TestMemory as Memory;
    use crate::debug::provenance::SourceProvenance;
    use crate::debug::sink::NullSink;
    use crate::memory::{PALETTE_BASE, VRAM_BASE};
    use crate::state::{Color15, LayerId};
    use crate::TestPpu as Ppu;

    /// Write a 15-bit color into background palette entry `index`.
    fn set_palette(mem: &mut Memory, index: usize, color: u16) {
        mem.palette[index * 2..index * 2 + 2].copy_from_slice(&color.to_le_bytes());
    }

    /// Write a tilemap entry (halfword) at VRAM byte offset `off`.
    fn set_map_entry(mem: &mut Memory, off: usize, entry: u16) {
        mem.vram[off..off + 2].copy_from_slice(&entry.to_le_bytes());
    }

    fn render_line0(ppu: &mut Ppu, mem: &Memory) {
        ppu.latch_for_scanline();
        let view = mem.view();
        ppu.render_scanline(0, &view, &mut NullSink);
    }

    /// Mode 0, BG0, 4bpp: a tilemap entry points at a tile whose two low texels
    /// resolve through the palette; a zero texel is transparent (backdrop shows).
    #[test]
    fn mode0_4bpp_tile_lookup_and_transparency() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt(1 << 8); // mode 0 (bits 0-2 = 0), BG0 enabled
        ppu.registers.bgcnt[0] = 1 << 2; // char base block 1 (0x4000), screen base 0

        set_palette(&mut mem, 0, 0x7C00); // backdrop = blue
        set_palette(&mut mem, 3, 0x03E0); // green
        set_palette(&mut mem, 5, 0x001F); // red
        set_map_entry(&mut mem, 0, 1); // cell (0,0) -> tile 1, no flip, bank 0
        // Tile 1 row 0 byte 0: low nibble (texel 0) = 3, high nibble (texel 1) = 5.
        mem.vram[0x4020] = 0x53;
        // Byte 1 (texels 2,3) left 0 -> transparent.

        render_line0(&mut ppu, &mem);
        assert_eq!(ppu.framebuffer()[0], Color15(0x03E0)); // texel 3
        assert_eq!(ppu.framebuffer()[1], Color15(0x001F)); // texel 5
        assert_eq!(ppu.framebuffer()[2], Color15(0x7C00)); // texel 0 -> backdrop
    }

    /// Horizontal flip mirrors the eight texels of a tile row.
    #[test]
    fn hflip_mirrors_the_tile_row() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt(1 << 8);
        ppu.registers.bgcnt[0] = 1 << 2;
        set_palette(&mut mem, 7, 0x03E0);
        // Entry with H-flip (bit 10) set, tile 1.
        set_map_entry(&mut mem, 0, 1 | (1 << 10));
        // Put texel 7 = 7 (the rightmost texel of the row): byte 3 high nibble.
        mem.vram[0x4020 + 3] = 0x70;

        render_line0(&mut ppu, &mem);
        // With H-flip, the rightmost texel (7) is sampled at screen x = 0.
        assert_eq!(ppu.framebuffer()[0], Color15(0x03E0));
    }

    /// 8bpp uses the whole byte as a 256-color palette index.
    #[test]
    fn mode0_8bpp_uses_full_byte_index() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt(1 << 8);
        ppu.registers.bgcnt[0] = (1 << 2) | (1 << 7); // char base 1, 8bpp
        set_palette(&mut mem, 200, 0x7FFF);
        set_map_entry(&mut mem, 0, 1); // tile 1 -> 8bpp base 0x4000 + 64
        mem.vram[0x4040] = 200; // tile 1, row 0, texel 0

        render_line0(&mut ppu, &mem);
        assert_eq!(ppu.framebuffer()[0], Color15(0x7FFF));
    }

    /// A horizontal scroll offset shifts which background texel lands at each x.
    #[test]
    fn horizontal_scroll_shifts_sampling() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt(1 << 8);
        ppu.registers.bgcnt[0] = 1 << 2;
        ppu.registers.bg_hofs[0] = 1; // scroll right by one texel
        set_palette(&mut mem, 5, 0x001F);
        set_map_entry(&mut mem, 0, 1);
        mem.vram[0x4020] = 0x50; // texel 0 = 0 (transparent), texel 1 = 5

        render_line0(&mut ppu, &mem);
        // With hofs=1, screen x=0 samples source texel 1 (value 5).
        assert_eq!(ppu.framebuffer()[0], Color15(0x001F));
    }

    /// A higher-priority background wins where both are opaque; where it is
    /// transparent, the lower-priority background shows.
    #[test]
    fn priority_resolves_between_two_text_backgrounds() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt((1 << 8) | (1 << 9)); // BG0 and BG1 enabled
        // BG0: char base 1, screen base 0, priority 1.
        ppu.registers.bgcnt[0] = (1 << 2) | 1;
        // BG1: char base 2, screen base block 1 (0x800), priority 0.
        ppu.registers.bgcnt[1] = (2 << 2) | (1 << 8);

        set_palette(&mut mem, 3, 0x03E0); // BG0 green
        set_palette(&mut mem, 4, 0x7FE0); // BG1 yellow
        set_map_entry(&mut mem, 0, 1); // BG0 cell (0,0) -> tile 1
        set_map_entry(&mut mem, 0x800, 1); // BG1 cell (0,0) -> tile 1
        mem.vram[0x4020] = 0x33; // BG0 texels 0,1 = 3 (green)
        mem.vram[0x8020] = 0x04; // BG1 texel 0 = 4 (yellow), texel 1 = 0 (transparent)

        render_line0(&mut ppu, &mem);
        // x=0: BG1 (priority 0) beats BG0 (priority 1).
        assert_eq!(ppu.framebuffer()[0], Color15(0x7FE0));
        // x=1: BG1 transparent -> BG0 green shows.
        assert_eq!(ppu.framebuffer()[1], Color15(0x03E0));
    }

    /// Alpha blending combines the top and second backgrounds by their
    /// coefficients.
    #[test]
    fn alpha_blend_combines_two_backgrounds() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt((1 << 8) | (1 << 9)); // BG0 and BG1
        ppu.registers.bgcnt[0] = 1 << 2; // BG0 char base 1, priority 0 (top)
        ppu.registers.bgcnt[1] = (2 << 2) | (1 << 8) | 1; // BG1 char base 2, screen 1, priority 1
        // Alpha blend: BG0 first target, BG1 second target, eva = evb = 8.
        ppu.registers.bldcnt = (1 << 6) | (1 << 0) | (1 << 9);
        ppu.registers.bldalpha = 8 | (8 << 8);
        set_palette(&mut mem, 3, 0x001F); // BG0 red
        set_palette(&mut mem, 4, 0x7C00); // BG1 blue
        set_map_entry(&mut mem, 0, 1); // BG0 tile 1
        set_map_entry(&mut mem, 0x800, 1); // BG1 tile 1
        mem.vram[0x4020] = 0x03; // BG0 texel 0 = 3
        mem.vram[0x8020] = 0x04; // BG1 texel 0 = 4

        render_line0(&mut ppu, &mem);
        // red(31,0,0)*8/16 + blue(0,0,31)*8/16 = (15,0,15).
        assert_eq!(ppu.framebuffer()[0], Color15(15 | (15 << 10)));
    }

    /// The explanation reports the exact map, tile-byte, and palette addresses.
    #[test]
    fn explain_reports_text_source_addresses() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt(1 << 8);
        ppu.registers.bgcnt[0] = 1 << 2; // char base 1, screen base 0, 4bpp
        set_palette(&mut mem, 3, 0x03E0);
        set_map_entry(&mut mem, 0, 1);
        mem.vram[0x4020] = 0x03; // texel 0 = 3

        let view = mem.view();
        let explanation = ppu.explain_current_pixel(0, 0, &view).unwrap();
        let bg0 = explanation.candidate_for(LayerId::Bg0).expect("BG0 candidate");
        match bg0.provenance {
            SourceProvenance::TextBg(p) => {
                assert_eq!(p.map_address, VRAM_BASE); // screenblock 0, cell 0
                assert_eq!(p.tile_number, 1);
                assert_eq!(p.tile_byte_address, VRAM_BASE + 0x4020);
                assert_eq!(p.palette_index, 3);
                assert_eq!(p.palette_address, PALETTE_BASE + 3 * 2);
                assert!(!p.horizontal_flip);
            }
            _ => panic!("expected text provenance"),
        }
    }
}
