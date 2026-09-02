//! Sprite rasterization: turn the evaluated sprite list into the OBJ candidate
//! line and the OBJ-window mask.
//!
//! Sprites are sampled in OAM order; for visible color, the first opaque texel at
//! a pixel wins (lower OAM index is in front). Object-window sprites contribute
//! only to the window mask, not to color. Rotation/scaling sprites map each
//! bounding-box pixel through the affine matrix before sampling.

use super::evaluate::{SpriteInstance, SpriteList};
use crate::debug::explain::CandidateExplanation;
use crate::debug::provenance::{ObjColorMode, ObjMode, ObjProvenance, SourceProvenance};
use crate::debug::sink::ProvenanceSink;
use crate::memory::{PpuMemoryView, VramLayout, PALETTE_BASE, VRAM_BASE};
use crate::state::{CandidatePixel, Color15, LatchedState, LayerId, ObjLine, PixelFlags};

/// Palette entries 0..=255 are backgrounds; OBJ palettes start at entry 256.
const OBJ_PALETTE_BASE: usize = 256;

/// Read affine parameter group `group`'s PA/PB/PC/PD from OAM. The four 16-bit
/// values are the fourth halfword of each of the group's four OBJ entries.
fn affine_params(mem: &PpuMemoryView, group: u8) -> (i32, i32, i32, i32) {
    let base = group as usize * 32;
    let read = |off: usize| mem.oam16(base + off) as i16 as i32;
    (read(0x06), read(0x0E), read(0x16), read(0x1E))
}

/// Where a texel `(tx, ty)` within a sprite lives: its tile number and the byte
/// address of the texel, honoring 1D/2D mapping and color depth.
fn texel_address(
    sprite: &SpriteInstance,
    tx: u32,
    ty: u32,
    one_dim: bool,
    obj_tile_base: u32,
    obj_tile_boundary: u32,
) -> (u16, u32) {
    let is_8bpp = matches!(sprite.color_mode, ObjColorMode::Bpp8);
    let units_per_tile = if is_8bpp { 2 } else { 1 };
    let tiles_wide = (sprite.width / 8) as u32;
    let (tile_col, tile_row) = (tx / 8, ty / 8);
    let base = sprite.tile_number as u32;
    let (tile_number, tile_base) = if one_dim {
        // 1D: the base tile number scales by the mapping boundary; tiles within the
        // sprite step 32 bytes (4bpp) / 64 (8bpp) in row-major order. With the GBA's
        // 32-byte boundary this reduces to the old `(base + within) * 32`.
        let within = (tile_row * tiles_wide + tile_col) * units_per_tile;
        (
            (base + within) & 0x3FF,
            obj_tile_base + base * obj_tile_boundary + within * 32,
        )
    } else {
        // 2D: a 32-tile-wide grid at 32-byte granularity (the GBA layout).
        let tn = (base + tile_row * 32 + tile_col * units_per_tile) & 0x3FF;
        (tn, obj_tile_base + tn * 32)
    };
    let (px, py) = (tx % 8, ty % 8);
    let byte_offset = if is_8bpp {
        tile_base + py * 8 + px
    } else {
        tile_base + py * 4 + px / 2
    };
    (tile_number as u16, byte_offset)
}

/// Byte offset of a direct-color bitmap-OBJ texel `(tx, ty)` within the sprite. In 1D
/// mapping the sprite's bitmap is stored contiguously (`base = tile_number * boundary`,
/// row stride = sprite width); in 2D the sprite is a window into a fixed-width (128- or
/// 256-dot) bitmap, its base split into X/Y by the tile-number mask (GBATEK).
fn bitmap_byte_offset(sprite: &SpriteInstance, tx: u32, ty: u32, layout: VramLayout) -> u32 {
    let tile = sprite.tile_number as u32;
    let (base, stride) = if layout.obj_bitmap_1d {
        (tile * layout.obj_bitmap_boundary, sprite.width as u32)
    } else {
        let (mask_x, src_width) = if layout.obj_bitmap_wide { (0x1F, 256) } else { (0x0F, 128) };
        ((tile & mask_x) * 0x10 + (tile & !mask_x) * 0x80, src_width)
    };
    layout.obj_tile_base + base + (ty * stride + tx) * 2
}

/// Sample a sprite texel; returns the raw palette value (0 = transparent).
fn sample_texel(sprite: &SpriteInstance, byte_offset: u32, tx: u32, mem: &PpuMemoryView) -> u8 {
    let byte = mem.vram_u8(byte_offset as usize);
    match sprite.color_mode {
        ObjColorMode::Bpp8 => byte,
        ObjColorMode::Bpp4 { .. } => {
            if tx & 1 == 0 {
                byte & 0xF
            } else {
                byte >> 4
            }
        }
        // Bitmap OBJs are direct-color: sampled in the rasterize loop, never here.
        ObjColorMode::Bitmap { .. } => 0,
    }
}

/// The background-palette entry for a sprite texel (OBJ palettes are the upper
/// half of palette RAM).
fn palette_entry(sprite: &SpriteInstance, texel: u8) -> usize {
    match sprite.color_mode {
        ObjColorMode::Bpp8 => OBJ_PALETTE_BASE + texel as usize,
        ObjColorMode::Bpp4 { palette_bank } => {
            OBJ_PALETTE_BASE + palette_bank as usize * 16 + texel as usize
        }
        // Bitmap OBJs carry no palette; direct color is resolved in the rasterize loop.
        ObjColorMode::Bitmap { .. } => OBJ_PALETTE_BASE,
    }
}

/// The texture coordinate `(tx, ty)` a bounding-box pixel samples, and whether it
/// falls inside the sprite. Applies the affine matrix or the H/V flips.
fn texture_coord(sprite: &SpriteInstance, col: u16, line: i32, mem: &PpuMemoryView) -> Option<(u32, u32)> {
    if let Some(group) = sprite.affine {
        let (pa, pb, pc, pd) = affine_params(mem, group);
        let dx = col as i32 - sprite.box_width as i32 / 2;
        let dy = line - sprite.box_height as i32 / 2;
        let tx = ((pa * dx + pb * dy) >> 8) + sprite.width as i32 / 2;
        let ty = ((pc * dx + pd * dy) >> 8) + sprite.height as i32 / 2;
        if (0..sprite.width as i32).contains(&tx) && (0..sprite.height as i32).contains(&ty) {
            Some((tx as u32, ty as u32))
        } else {
            None
        }
    } else {
        let ix = if sprite.hflip {
            sprite.width as i32 - 1 - col as i32
        } else {
            col as i32
        };
        let iy = if sprite.vflip {
            sprite.height as i32 - 1 - line
        } else {
            line
        };
        Some((ix as u32, iy as u32))
    }
}

/// Rasterize the evaluated sprites for scanline `y` into the OBJ line.
#[allow(clippy::too_many_arguments)]
pub fn rasterize<S: ProvenanceSink>(
    y: u16,
    state: &LatchedState,
    mem: &PpuMemoryView,
    width: usize,
    layout: VramLayout,
    sprites: &SpriteList,
    obj: &mut ObjLine,
    sink: &mut S,
) {
    let one_dim = state.obj_one_dim_mapping();
    let (mosaic_x, mosaic_y) = state.obj_mosaic();

    for sprite in sprites {
        let line = (y as i32 - sprite.y) & 0xFF;
        let is_window = sprite.obj_mode == ObjMode::ObjWindow;
        let semi = sprite.obj_mode == ObjMode::SemiTransparent;
        // OBJ mosaic snaps the object-relative coordinate to its block origin.
        let line_src = if sprite.mosaic {
            line - line % mosaic_y as i32
        } else {
            line
        };

        for col in 0..sprite.box_width {
            let screen_x = (sprite.x + col as i32) & 0x1FF;
            if screen_x >= width as i32 {
                continue;
            }
            let sx = screen_x as usize;

            let col_src = if sprite.mosaic {
                col - col % mosaic_x as u16
            } else {
                col
            };
            let Some((tx, ty)) = texture_coord(sprite, col_src, line_src, mem) else {
                continue;
            };

            // Sample the sprite pixel. A direct-color bitmap OBJ reads a 16-bit BGR555
            // texel whose bit 15 is the opacity flag; a paletted OBJ indexes the OBJ
            // palette. Both paths yield the final color, an intrinsic per-object alpha
            // (bitmap only), and the source addresses kept for provenance.
            let (color, obj_alpha, texel, entry, tile_number, byte_offset) = if let ObjColorMode::Bitmap {
                alpha,
            } = sprite.color_mode
            {
                let byte_offset = bitmap_byte_offset(sprite, tx, ty, layout);
                let raw = mem.vram_u16(byte_offset as usize);
                if raw & 0x8000 == 0 {
                    continue; // bit 15 clear → transparent texel
                }
                (Color15(raw & 0x7FFF), Some(alpha), 0u8, 0usize, sprite.tile_number, byte_offset)
            } else {
                let (tile_number, byte_offset) = texel_address(
                    sprite,
                    tx,
                    ty,
                    one_dim,
                    layout.obj_tile_base,
                    layout.obj_tile_boundary,
                );
                let texel = sample_texel(sprite, byte_offset, tx, mem);
                if texel == 0 {
                    continue; // transparent texel contributes nothing
                }
                let entry = palette_entry(sprite, texel);
                let color = if matches!(sprite.color_mode, ObjColorMode::Bpp8) && layout.obj_ext_palette {
                    // Extended OBJ palette: attr2 bits 12-15 select the 256-color
                    // sub-palette (an 8bpp sprite otherwise uses the whole byte as an
                    // index into the single standard OBJ palette).
                    mem.obj_ext15(sprite.palette_bank as usize, texel as usize)
                } else {
                    mem.palette15(entry)
                };
                (color, None, texel, entry, tile_number, byte_offset)
            };

            // Object-window sprites only mark the mask.
            if is_window {
                obj.window[sx] = true;
                continue;
            }

            // Among overlapping sprites, the lowest priority value wins; ties go to
            // the lower OAM index. Since sprites are processed in OAM order, an
            // already-placed pixel is only displaced by a strictly better priority.
            if let Some(existing) = obj.pixels[sx] {
                if sprite.priority >= existing.priority {
                    continue;
                }
            }

            let candidate = CandidatePixel {
                color,
                layer: LayerId::Obj,
                priority: sprite.priority,
                flags: PixelFlags {
                    semi_transparent_obj: semi,
                    obj_window: false,
                    mosaic: sprite.mosaic,
                    obj_alpha,
                    ..PixelFlags::default()
                },
            };
            obj.pixels[sx] = Some(candidate);

            if sink.wants(sx as u16) {
                sink.record_candidate(sx as u16, LayerId::Obj, || CandidateExplanation {
                    candidate,
                    provenance: SourceProvenance::Obj(ObjProvenance {
                        oam_index: sprite.oam_index,
                        oam_address: 0x0700_0000 + sprite.oam_index as u32 * 8,
                        attr0: sprite.attr0,
                        attr1: sprite.attr1,
                        attr2: sprite.attr2,
                        obj_mode: sprite.obj_mode,
                        color_mode: sprite.color_mode,
                        local_x: tx as u16,
                        local_y: ty as u16,
                        tile_number,
                        tile_address: VRAM_BASE + layout.obj_tile_base + tile_number as u32 * 32,
                        tile_byte_address: VRAM_BASE + byte_offset,
                        palette_index: texel,
                        palette_address: PALETTE_BASE + entry as u32 * 2,
                        priority: sprite.priority,
                        hflip: sprite.hflip,
                        vflip: sprite.vflip,
                    }),
                    visible_after_window: true,
                    rejection_reason: None,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::TestMemory as Memory;
    use crate::debug::provenance::ObjColorMode;
    use crate::debug::sink::NullSink;
    use crate::registers::Registers;
    use crate::state::{LatchedState, ObjLine};

    fn square_sprite(size: u16, obj_mode: ObjMode) -> SpriteInstance {
        SpriteInstance {
            oam_index: 0,
            x: 0,
            y: 0,
            width: size,
            height: size,
            box_width: size,
            box_height: size,
            tile_number: 0,
            priority: 0,
            palette_bank: 0,
            color_mode: ObjColorMode::Bpp4 { palette_bank: 0 },
            obj_mode,
            hflip: false,
            vflip: false,
            mosaic: false,
            affine: None,
            attr0: 0,
            attr1: 0,
            attr2: 0,
        }
    }

    fn bitmap_sprite(size: u16, alpha: u8) -> SpriteInstance {
        SpriteInstance {
            color_mode: ObjColorMode::Bitmap { alpha },
            ..square_sprite(size, ObjMode::Normal)
        }
    }

    fn ds_bitmap_layout(one_dim: bool, wide: bool) -> VramLayout {
        VramLayout {
            mode_semantics: crate::memory::ModeSemantics::Ds,
            obj_bitmap_1d: one_dim,
            obj_bitmap_wide: wide,
            ..VramLayout::gba()
        }
    }

    /// A DS direct-color bitmap OBJ renders its 15-bit color; bit 15 is the opacity flag.
    #[test]
    fn bitmap_obj_renders_direct_color_and_honors_opacity_flag() {
        let mut mem = Memory::default();
        let base = 0x1_0000; // obj_tile_base, tile_number 0
        // Texel (0,0): opaque red (bit 15 set). Texel (1,0): red but bit 15 clear.
        mem.vram[base..base + 2].copy_from_slice(&(0x8000u16 | 0x001F).to_le_bytes());
        mem.vram[base + 2..base + 4].copy_from_slice(&0x001Fu16.to_le_bytes());
        let view = mem.view();
        let regs = Registers { dispcnt: 1 << 12, ..Default::default() };
        let state = LatchedState::from_registers(&regs);
        let sprites = vec![bitmap_sprite(8, 15)];
        let mut obj = ObjLine::default();

        rasterize(0, &state, &view, 240, ds_bitmap_layout(true, false), &sprites, &mut obj, &mut NullSink);
        assert_eq!(obj.pixels[0].unwrap().color, Color15(0x001F), "opaque red, bit 15 stripped");
        assert!(obj.pixels[1].is_none(), "bit-15-clear texel is transparent");
    }

    /// 2D bitmap mapping windows into a fixed-width bitmap: the tile number splits into
    /// X/Y (maskX 0x0F at 128-dot width) and the row stride is the source width.
    #[test]
    fn bitmap_2d_addressing_splits_tile_number_and_strides_by_source_width() {
        let layout = ds_bitmap_layout(false, false); // 2D, 128-dot width → 256-byte stride
        let s = bitmap_sprite(16, 15); // tile_number 0
        assert_eq!(bitmap_byte_offset(&s, 3, 0, layout), layout.obj_tile_base + 6); // 3 texels right
        assert_eq!(bitmap_byte_offset(&s, 0, 1, layout), layout.obj_tile_base + 256); // next row
        let s2 = SpriteInstance { tile_number: 0x11, ..s }; // X=1, Y=1
        // base = (0x11 & 0x0F)*0x10 + (0x11 & !0x0F)*0x80 = 0x10 + 0x800.
        assert_eq!(bitmap_byte_offset(&s2, 0, 0, layout), layout.obj_tile_base + 0x810);
    }

    /// An object-window sprite marks the window mask but contributes no color.
    #[test]
    fn obj_window_sprite_sets_mask_not_color() {
        let mut mem = Memory::default();
        mem.vram[0x10000] = 0x05; // tile 0, texel (0,0) opaque
        let view = mem.view();
        let regs = Registers {
            dispcnt: (1 << 12) | (1 << 6),
            ..Default::default()
        };
        let state = LatchedState::from_registers(&regs);
        let sprites = vec![square_sprite(8, ObjMode::ObjWindow)];
        let mut obj = ObjLine::default();

        rasterize(0, &state, &view, 240, VramLayout::gba(), &sprites, &mut obj, &mut NullSink);
        assert!(obj.window[0]); // mask set
        assert!(obj.pixels[0].is_none()); // no visible color
    }

    /// 1D mapping advances tile rows by the sprite width in tiles; 2D by 32.
    #[test]
    fn tile_row_stride_differs_between_1d_and_2d() {
        let sprite = square_sprite(16, ObjMode::Normal); // 16×16 = 2×2 tiles, 4bpp
        // Texel (0, 8) is the first texel of tile row 1.
        let (tile_1d, _) = texel_address(&sprite, 0, 8, true, 0x1_0000, 32);
        assert_eq!(tile_1d, 2); // row stride = tiles_wide(2) * 1
        let (tile_2d, _) = texel_address(&sprite, 0, 8, false, 0x1_0000, 32);
        assert_eq!(tile_2d, 32); // row stride = 32
    }
}
