//! Extended backgrounds — the DS's BG2/BG3 in modes 3-5 (and BG2 in mode 6). An
//! extended background is affine-addressed (2×2 matrix + per-scanline reference,
//! like an affine BG) but carries one of three payloads, selected by `BGxCNT` bits
//! 7 and 2:
//!
//!   - bit7 = 0: a **tiled** map of 16-bit entries — each entry a 10-bit tile
//!     number, H/V flip, and a 4-bit extended-palette slot; tiles are 8bpp. This
//!     is the common game-background type and is implemented here.
//!   - bit7 = 1, bit2 = 0: a **256-color bitmap** (8bpp paletted).
//!   - bit7 = 1, bit2 = 1: a **direct-color bitmap** (16bpp BGR555, bit15 = opaque).
//!
//! The two bitmap sub-types are not yet rendered (they land in Phase C) and read as
//! transparent for now.

use crate::debug::explain::CandidateExplanation;
use crate::debug::provenance::{
    AffineBgProvenance, AffineMatrix, AffineWrap, BackgroundId, BitmapBgProvenance, BitmapSample,
    RejectionReason, SourceProvenance,
};
use crate::debug::sink::ProvenanceSink;
use crate::memory::{PpuMemoryView, VramLayout, PALETTE_BASE, VRAM_BASE};
use crate::state::{
    AffineReference, CandidatePixel, Color15, LatchedState, LayerId, PixelFlags, Scratch,
};

const CHARBLOCK_SIZE: u32 = 0x4000;
const SCREENBLOCK_SIZE: u32 = 0x800;
/// The bitmap-data base for a bitmap extended background is `BGxCNT` bits 8-12 in
/// 16 KB units (the tiled sub-type reuses the same field in 2 KB screen-block units).
const BITMAP_BASE_UNIT: u32 = 0x4000;

fn size_pixels(size: u16) -> i32 {
    match size & 3 {
        0 => 128,
        1 => 256,
        2 => 512,
        _ => 1024,
    }
}

/// The `(width, height)` in pixels of a bitmap extended background — unlike the
/// square tiled sizes, size 2 is a 512×256 rectangle.
fn bitmap_size(size: u16) -> (i32, i32) {
    match size & 3 {
        0 => (128, 128),
        1 => (256, 256),
        2 => (512, 256),
        _ => (512, 512),
    }
}

fn background_id(bg: usize) -> BackgroundId {
    if bg == 2 {
        BackgroundId::Bg2
    } else {
        BackgroundId::Bg3
    }
}

/// Render extended background `bg` (2 or 3) for scanline `y`. Dispatches on the
/// `BGxCNT` sub-type; only the tiled sub-type produces pixels in this phase.
#[allow(clippy::too_many_arguments)]
pub fn render_extended_scanline<S: ProvenanceSink>(
    bg: usize,
    y: u16,
    state: &LatchedState,
    reference: AffineReference,
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
    // bit7: 0 = tiled map, 1 = bitmap; bit2 (within bitmap) picks direct vs 8bpp.
    if cnt & (1 << 7) == 0 {
        render_extended_tiled(bg, y, state, reference, mem, width, layout, scratch, sink);
    } else {
        let direct = cnt & (1 << 2) != 0;
        render_extended_bitmap(bg, y, state, reference, mem, width, direct, scratch, sink);
    }
}

/// A bitmap extended background: VRAM sampled directly through the affine matrix,
/// either as 16bpp direct color (`direct`, bit15 = opaque) or 8bpp palette indices.
#[allow(clippy::too_many_arguments)]
fn render_extended_bitmap<S: ProvenanceSink>(
    bg: usize,
    y: u16,
    state: &LatchedState,
    reference: AffineReference,
    mem: &PpuMemoryView,
    width: usize,
    direct: bool,
    scratch: &mut Scratch,
    sink: &mut S,
) {
    let k = bg - 2;
    let cnt = state.regs.bgcnt[bg];
    let priority = (cnt & 0x3) as u8;
    let base = ((cnt >> 8) & 0x1F) as u32 * BITMAP_BASE_UNIT;
    let wrap = cnt & (1 << 13) != 0;
    let (w, h) = bitmap_size((cnt >> 14) & 0x3);

    let pa = state.regs.bg_pa[k] as i32;
    let pc = state.regs.bg_pc[k] as i32;
    let pb = state.regs.bg_pb[k] as i32;
    let pd = state.regs.bg_pd[k] as i32;
    let layer = LayerId::bg(bg);
    let (mosaic_x, mosaic_y, mosaic) = super::bg_mosaic_factors(state, bg);

    let vback = y as i32 % mosaic_y as i32;
    let ref_x = reference.x - pb * vback;
    let ref_y = reference.y - pd * vback;

    for x in 0..width {
        let x_src = (x - x % mosaic_x) as i32;
        let tex_x = (ref_x + pa * x_src) >> 8;
        let tex_y = (ref_y + pc * x_src) >> 8;

        let in_range = if wrap {
            true
        } else {
            (0..w).contains(&tex_x) && (0..h).contains(&tex_y)
        };
        let (sx, sy) = if wrap {
            (tex_x.rem_euclid(w), tex_y.rem_euclid(h))
        } else {
            (tex_x, tex_y)
        };

        let mut vram_offset = 0usize;
        let mut index = 0u8;
        let mut opaque = false;
        let mut color = Color15(0);
        if in_range {
            let pixel = (sy * w + sx) as u32;
            if direct {
                vram_offset = (base + pixel * 2) as usize;
                let raw = mem.vram16(vram_offset);
                opaque = raw & 0x8000 != 0;
                color = Color15(raw & 0x7FFF);
            } else {
                vram_offset = (base + pixel) as usize;
                index = mem.vram_u8(vram_offset);
                opaque = index != 0;
                color = mem.palette15(index as usize);
            }
        }
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
            let rejection = if !in_range {
                Some(RejectionReason::OutOfBounds)
            } else if !opaque {
                Some(RejectionReason::TransparentPixel {
                    palette_index: index,
                })
            } else {
                None
            };
            let (raw, palette_address) = if direct {
                (BitmapSample::Direct(color), None)
            } else {
                (BitmapSample::Indexed(index), Some(PALETTE_BASE + index as u32 * 2))
            };
            sink.record_candidate(x as u16, layer, || CandidateExplanation {
                candidate,
                provenance: SourceProvenance::BitmapBg(BitmapBgProvenance {
                    video_mode: state.mode,
                    frame: 0,
                    source_x: sx.max(0) as u16,
                    source_y: sy.max(0) as u16,
                    vram_address: VRAM_BASE + vram_offset as u32,
                    raw,
                    palette_address,
                    resolved: color,
                    priority,
                }),
                visible_after_window: opaque,
                rejection_reason: rejection,
            });
        }
    }
}

/// The tiled extended background: an affine map of 16-bit entries into 8bpp tiles.
#[allow(clippy::too_many_arguments)]
fn render_extended_tiled<S: ProvenanceSink>(
    bg: usize,
    y: u16,
    state: &LatchedState,
    reference: AffineReference,
    mem: &PpuMemoryView,
    width: usize,
    layout: VramLayout,
    scratch: &mut Scratch,
    sink: &mut S,
) {
    let k = bg - 2; // affine parameter index: BG2 -> 0, BG3 -> 1
    let cnt = state.regs.bgcnt[bg];
    let priority = (cnt & 0x3) as u8;
    let char_base =
        layout.bg_char_base + ((cnt >> 2) as u32 & layout.bg_char_base_mask) * CHARBLOCK_SIZE;
    let screen_base = layout.bg_screen_base + ((cnt >> 8) & 0x1F) as u32 * SCREENBLOCK_SIZE;
    let wrap = cnt & (1 << 13) != 0;
    let size_px = size_pixels((cnt >> 14) & 0x3);
    let map_tiles = (size_px / 8) as u32;

    let pa = state.regs.bg_pa[k] as i32;
    let pc = state.regs.bg_pc[k] as i32;
    let pb = state.regs.bg_pb[k] as i32;
    let pd = state.regs.bg_pd[k] as i32;
    let layer = LayerId::bg(bg);
    let (mosaic_x, mosaic_y, mosaic) = super::bg_mosaic_factors(state, bg);

    let vback = y as i32 % mosaic_y as i32;
    let ref_x = reference.x - pb * vback;
    let ref_y = reference.y - pd * vback;

    for x in 0..width {
        let x_src = (x - x % mosaic_x) as i32;
        let tex_x = (ref_x + pa * x_src) >> 8;
        let tex_y = (ref_y + pc * x_src) >> 8;

        let in_range = if wrap {
            true
        } else {
            (0..size_px).contains(&tex_x) && (0..size_px).contains(&tex_y)
        };
        let (sx, sy) = if wrap {
            (tex_x.rem_euclid(size_px), tex_y.rem_euclid(size_px))
        } else {
            (tex_x, tex_y)
        };

        let mut map_offset = 0usize;
        let mut entry = 0u16;
        let mut tile_number = 0u16;
        let mut tile_byte_offset = 0usize;
        let mut texel = 0u8;
        let mut subpal = 0usize;
        if in_range {
            let tile_x = (sx / 8) as u32;
            let tile_y = (sy / 8) as u32;
            map_offset = (screen_base + (tile_y * map_tiles + tile_x) * 2) as usize;
            entry = mem.vram16(map_offset);
            tile_number = entry & 0x3FF;
            subpal = ((entry >> 12) & 0xF) as usize;
            // Flip within the 8×8 tile per the entry's H/V-flip bits.
            let px = if entry & (1 << 10) != 0 { 7 - (sx % 8) } else { sx % 8 } as u32;
            let py = if entry & (1 << 11) != 0 { 7 - (sy % 8) } else { sy % 8 } as u32;
            tile_byte_offset = (char_base + tile_number as u32 * 64 + py * 8 + px) as usize;
            texel = mem.vram_u8(tile_byte_offset);
        }
        let opaque = in_range && texel != 0;
        // 8bpp: the extended palette (when DISPCNT enables it) selects a 256-color
        // sub-palette by the entry's slot bits; otherwise the standard palette.
        let color = if layout.bg_ext_palette {
            mem.bg_ext15(bg, subpal, texel as usize)
        } else {
            mem.palette15(texel as usize)
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
            let rejection = if !in_range {
                Some(RejectionReason::OutOfBounds)
            } else if !opaque {
                Some(RejectionReason::TransparentPixel {
                    palette_index: texel,
                })
            } else {
                None
            };
            sink.record_candidate(x as u16, layer, || CandidateExplanation {
                candidate,
                provenance: SourceProvenance::AffineBg(AffineBgProvenance {
                    bg: background_id(bg),
                    screen_x: x as u16,
                    screen_y: y,
                    reference,
                    matrix: AffineMatrix {
                        pa: state.regs.bg_pa[k],
                        pb: state.regs.bg_pb[k],
                        pc: state.regs.bg_pc[k],
                        pd: state.regs.bg_pd[k],
                    },
                    transformed_x: tex_x,
                    transformed_y: tex_y,
                    wrap: if wrap {
                        AffineWrap::Wrap
                    } else {
                        AffineWrap::Transparent
                    },
                    in_range,
                    map_address: VRAM_BASE + map_offset as u32,
                    map_entry: (entry & 0xFF) as u8,
                    tile_number,
                    tile_address: VRAM_BASE + char_base + tile_number as u32 * 64,
                    tile_byte_address: VRAM_BASE + tile_byte_offset as u32,
                    palette_index: texel,
                    palette_address: PALETTE_BASE + texel as u32 * 2,
                }),
                visible_after_window: opaque,
                rejection_reason: rejection,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::debug::sink::NullSink;
    use crate::memory::{ModeSemantics, TestMemory as Memory};
    use crate::state::Color15;
    use crate::TestPpu as Ppu;

    fn set_palette(mem: &mut Memory, index: usize, color: u16) {
        mem.palette[index * 2..index * 2 + 2].copy_from_slice(&color.to_le_bytes());
    }

    fn ds_ppu() -> Ppu {
        let mut ppu = Ppu::new();
        ppu.layout.mode_semantics = ModeSemantics::Ds;
        ppu.layout.bg_char_base_mask = 0xF;
        ppu
    }

    fn render(ppu: &mut Ppu, mem: &Memory, y: u16) {
        ppu.affine.bg2 = ppu.affine_reference_for_line(0, y);
        ppu.affine.bg3 = ppu.affine_reference_for_line(1, y);
        ppu.latch_for_scanline();
        let view = mem.view();
        ppu.render_scanline(y, &view, &mut NullSink);
    }

    /// A tiled extended BG2 (mode 5) samples a 16-bit map entry under the identity
    /// matrix and reads an 8bpp texel through the standard palette.
    #[test]
    fn extended_tiled_identity_maps_a_texel() {
        let mut ppu = ds_ppu();
        let mut mem = Memory::default();
        ppu.write_dispcnt(5 | (1 << 10)); // DS mode 5, BG2 enabled
        ppu.registers.bgcnt[2] = 1 << 2; // tiled (bit7=0), char base 1, screen base 0
        ppu.registers.bg_pa[0] = 256; // identity
        ppu.registers.bg_pd[0] = 256;
        set_palette(&mut mem, 0, 0x7C00); // backdrop
        set_palette(&mut mem, 5, 0x03E0); // green
        // Map entry (0,0): 16-bit -> tile 2, no flip, subpal 0.
        mem.vram[0..2].copy_from_slice(&2u16.to_le_bytes());
        // Tile 2 at char base 1 (0x4000) + 2*64 = 0x4080; texel (0,0) = 5.
        mem.vram[0x4080] = 5;
        render(&mut ppu, &mem, 0);
        assert_eq!(ppu.framebuffer()[0], Color15(0x03E0), "BG2 texel resolves via its palette");
    }

    /// The entry's horizontal-flip bit mirrors the sampled column within the tile.
    #[test]
    fn extended_tiled_honors_horizontal_flip() {
        let mut ppu = ds_ppu();
        let mut mem = Memory::default();
        ppu.write_dispcnt(5 | (1 << 10));
        ppu.registers.bgcnt[2] = 1 << 2;
        ppu.registers.bg_pa[0] = 256;
        ppu.registers.bg_pd[0] = 256;
        set_palette(&mut mem, 0, 0x7C00);
        set_palette(&mut mem, 9, 0x7FFF);
        // Entry (0,0) -> tile 1 with H-flip (bit 10).
        mem.vram[0..2].copy_from_slice(&(1u16 | (1 << 10)).to_le_bytes());
        // Tile 1 at 0x4040; texel (7,0) = 9. With H-flip, screen x=0 reads texel 7.
        mem.vram[0x4040 + 7] = 9;
        render(&mut ppu, &mem, 0);
        assert_eq!(ppu.framebuffer()[0], Color15(0x7FFF), "H-flip mirrors the tile column");
    }

    /// A direct-color bitmap extended BG reads a 16bpp BGR555 pixel straight from
    /// VRAM; bit 15 is the opacity flag.
    #[test]
    fn extended_direct_bitmap_reads_raw_color() {
        let mut ppu = ds_ppu();
        let mut mem = Memory::default();
        ppu.write_dispcnt(5 | (1 << 10));
        // BG2CNT: bitmap (bit7), direct color (bit2), base 0, size 0 (128×128).
        ppu.registers.bgcnt[2] = (1 << 7) | (1 << 2);
        ppu.registers.bg_pa[0] = 256;
        ppu.registers.bg_pd[0] = 256;
        set_palette(&mut mem, 0, 0x7C00); // backdrop
        // Pixel (0,0): opaque (bit15) green.
        mem.vram[0..2].copy_from_slice(&(0x8000u16 | 0x03E0).to_le_bytes());
        render(&mut ppu, &mem, 0);
        assert_eq!(ppu.framebuffer()[0], Color15(0x03E0), "direct pixel shows its raw color");
        // A pixel with bit15 clear is transparent → backdrop shows.
        mem.vram[2..4].copy_from_slice(&0x03E0u16.to_le_bytes());
        render(&mut ppu, &mem, 0);
        assert_eq!(ppu.framebuffer()[1], Color15(0x7C00), "bit15-clear pixel is transparent");
    }

    /// A 256-color bitmap extended BG reads 8bpp palette indices from VRAM.
    #[test]
    fn extended_paletted_bitmap_resolves_index() {
        let mut ppu = ds_ppu();
        let mut mem = Memory::default();
        ppu.write_dispcnt(5 | (1 << 10));
        // BG2CNT: bitmap (bit7), paletted (bit2=0), base 0, size 0.
        ppu.registers.bgcnt[2] = 1 << 7;
        ppu.registers.bg_pa[0] = 256;
        ppu.registers.bg_pd[0] = 256;
        set_palette(&mut mem, 0, 0x7C00);
        set_palette(&mut mem, 5, 0x001F);
        mem.vram[0] = 5; // pixel (0,0) -> palette index 5
        render(&mut ppu, &mem, 0);
        assert_eq!(ppu.framebuffer()[0], Color15(0x001F), "8bpp index resolves via palette");
    }
}
