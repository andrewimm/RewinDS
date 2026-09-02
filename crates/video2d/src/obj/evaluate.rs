//! OAM evaluation: which sprites are visible on a scanline, in OAM order, subject
//! to the per-scanline processing budget.
//!
//! Evaluation is kept separate from rasterization (spec §19) so the hardware
//! sprite-count/cycle limit and its rejections can be modeled and inspected
//! without entangling them with texel sampling.

use crate::debug::provenance::{ObjColorMode, ObjMode};
use crate::memory::{ModeSemantics, PpuMemoryView, VramLayout};
use crate::state::LatchedState;

/// A sprite found visible on the scanline being evaluated.
#[derive(Clone, Copy, Debug)]
pub struct SpriteInstance {
    pub oam_index: u8,
    /// Bounding-box left edge, as the raw 9-bit OAM X (0..=511); wrapping to the
    /// screen happens during rasterization.
    pub x: i32,
    /// Bounding-box top edge, as the raw 8-bit OAM Y (0..=255).
    pub y: i32,
    /// Sprite dimensions in pixels.
    pub width: u16,
    pub height: u16,
    /// Bounding-box dimensions (twice the sprite for double-size affine sprites).
    pub box_width: u16,
    pub box_height: u16,
    pub tile_number: u16,
    pub priority: u8,
    pub palette_bank: u8,
    pub color_mode: ObjColorMode,
    pub obj_mode: ObjMode,
    pub hflip: bool,
    pub vflip: bool,
    pub mosaic: bool,
    /// The affine parameter group (0..=31) for a rotation/scaling sprite, or
    /// `None` for a normal sprite.
    pub affine: Option<u8>,
    /// The three raw OAM attributes, kept for provenance.
    pub attr0: u16,
    pub attr1: u16,
    pub attr2: u16,
}

/// The sprites visible on one scanline.
pub type SpriteList = Vec<SpriteInstance>;

/// The (width, height) in pixels for an OBJ shape/size pair.
pub fn sprite_dimensions(shape: u16, size: u16) -> (u16, u16) {
    match (shape, size) {
        (0, 0) => (8, 8),
        (0, 1) => (16, 16),
        (0, 2) => (32, 32),
        (0, 3) => (64, 64),
        (1, 0) => (16, 8),
        (1, 1) => (32, 8),
        (1, 2) => (32, 16),
        (1, 3) => (64, 32),
        (2, 0) => (8, 16),
        (2, 1) => (8, 32),
        (2, 2) => (16, 32),
        (2, 3) => (32, 64),
        // Prohibited shape 3 is treated as 8×8.
        _ => (8, 8),
    }
}

/// The per-scanline OBJ rendering budget in cycles, and the per-sprite cost model.
/// A normal sprite costs its bounding-box width; an affine sprite costs
/// `10 + 2*width`.
fn budget(state: &LatchedState) -> u32 {
    if state.hblank_interval_free() {
        954
    } else {
        1210
    }
}

fn sprite_cost(box_width: u16, affine: bool) -> u32 {
    if affine {
        10 + 2 * box_width as u32
    } else {
        box_width as u32
    }
}

/// Evaluate OAM against scanline `y`, appending the visible sprites (in OAM order,
/// truncated by the cycle budget) to `out`.
pub fn evaluate_scanline(
    y: u16,
    state: &LatchedState,
    mem: &PpuMemoryView,
    layout: &VramLayout,
    out: &mut SpriteList,
) {
    out.clear();
    let mut spent = 0u32;
    let limit = budget(state);
    // OBJ mode 3 is prohibited on the GBA but selects the direct-color bitmap OBJ on the DS.
    let is_ds = layout.mode_semantics == ModeSemantics::Ds;

    for index in 0..128u8 {
        let base = index as usize * 8;
        let attr0 = mem.oam16(base);
        let attr1 = mem.oam16(base + 2);
        let attr2 = mem.oam16(base + 4);

        let is_affine = attr0 & (1 << 8) != 0;
        // For a normal sprite, bit 9 is the disable flag.
        if !is_affine && attr0 & (1 << 9) != 0 {
            continue;
        }
        let shape = (attr0 >> 14) & 0x3;
        // A DS bitmap OBJ (mode 3) composites as an ordinary OBJ carrying its own alpha
        // (see `color_mode` below); its mode field is only the bitmap selector.
        let bitmap = (attr0 >> 10) & 0x3 == 3 && is_ds;
        let obj_mode = match (attr0 >> 10) & 0x3 {
            1 => ObjMode::SemiTransparent,
            2 => ObjMode::ObjWindow,
            3 if !is_ds => continue, // prohibited on the GBA
            _ => ObjMode::Normal,
        };
        if shape == 3 {
            continue; // prohibited shape
        }

        let size = (attr1 >> 14) & 0x3;
        let (width, height) = sprite_dimensions(shape, size);
        // For an affine sprite, bit 9 is the double-size flag.
        let double = is_affine && attr0 & (1 << 9) != 0;
        let (box_width, box_height) = if double {
            (width * 2, height * 2)
        } else {
            (width, height)
        };

        // Vertical coverage, with the 8-bit Y wrap.
        let obj_y = (attr0 & 0xFF) as i32;
        let line = (y as i32 - obj_y) & 0xFF;
        if line >= box_height as i32 {
            continue;
        }

        // Charge the sprite against the scanline budget; once exhausted, remaining
        // sprites are rejected by the hardware limit.
        spent += sprite_cost(box_width, is_affine);
        if spent > limit {
            break;
        }

        // Bitmap OBJs are direct-color and reuse attr2 bits 12-15 as the alpha value
        // (not a palette bank); the color-depth bit (attr0 bit 13) is ignored for them.
        let color_mode = if bitmap {
            ObjColorMode::Bitmap {
                alpha: ((attr2 >> 12) & 0xF) as u8,
            }
        } else if attr0 & (1 << 13) != 0 {
            ObjColorMode::Bpp8
        } else {
            ObjColorMode::Bpp4 {
                palette_bank: ((attr2 >> 12) & 0xF) as u8,
            }
        };

        out.push(SpriteInstance {
            oam_index: index,
            x: (attr1 & 0x1FF) as i32,
            y: obj_y,
            width,
            height,
            box_width,
            box_height,
            tile_number: attr2 & 0x3FF,
            priority: ((attr2 >> 10) & 0x3) as u8,
            palette_bank: ((attr2 >> 12) & 0xF) as u8,
            color_mode,
            obj_mode,
            hflip: !is_affine && attr1 & (1 << 12) != 0,
            vflip: !is_affine && attr1 & (1 << 13) != 0,
            mosaic: attr0 & (1 << 12) != 0,
            affine: if is_affine {
                Some(((attr1 >> 9) & 0x1F) as u8)
            } else {
                None
            },
            attr0,
            attr1,
            attr2,
        });
    }
}
