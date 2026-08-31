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
use crate::state::{CandidatePixel, LatchedState, LayerId, ObjLine, PixelFlags};

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
) -> (u16, u32) {
    let is_8bpp = matches!(sprite.color_mode, ObjColorMode::Bpp8);
    let units_per_tile = if is_8bpp { 2 } else { 1 };
    let tiles_wide = (sprite.width / 8) as u32;
    let (tile_col, tile_row) = (tx / 8, ty / 8);
    let row_stride = if one_dim { tiles_wide * units_per_tile } else { 32 };
    let tile_number =
        (sprite.tile_number as u32 + tile_row * row_stride + tile_col * units_per_tile) & 0x3FF;

    let tile_base = obj_tile_base + tile_number * 32;
    let (px, py) = (tx % 8, ty % 8);
    let byte_offset = if is_8bpp {
        tile_base + py * 8 + px
    } else {
        tile_base + py * 4 + px / 2
    };
    (tile_number as u16, byte_offset)
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
            let (tile_number, byte_offset) =
                texel_address(sprite, tx, ty, one_dim, layout.obj_tile_base);
            let texel = sample_texel(sprite, byte_offset, tx, mem);
            if texel == 0 {
                continue; // transparent texel contributes nothing
            }

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

            let entry = palette_entry(sprite, texel);
            let color = mem.palette15(entry);
            let candidate = CandidatePixel {
                color,
                layer: LayerId::Obj,
                priority: sprite.priority,
                flags: PixelFlags {
                    semi_transparent_obj: semi,
                    obj_window: false,
                    mosaic: sprite.mosaic,
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
        let (tile_1d, _) = texel_address(&sprite, 0, 8, true, 0x1_0000);
        assert_eq!(tile_1d, 2); // row stride = tiles_wide(2) * 1
        let (tile_2d, _) = texel_address(&sprite, 0, 8, false, 0x1_0000);
        assert_eq!(tile_2d, 32); // row stride = 32
    }
}
