//! Affine (rotation/scaling) backgrounds — BG2 in mode 1, BG2 and BG3 in mode 2.
//!
//! An affine background maps each screen pixel through a 2×2 matrix (PA–PD) plus a
//! per-scanline reference point to a texture coordinate. Unlike text backgrounds,
//! the tilemap holds single-byte tile indices, tiles are always 8bpp, and there is
//! no per-tile flip (orientation comes from the matrix). Coordinates outside the
//! map either wrap or become transparent, per `BGxCNT` bit 13.

use crate::debug::explain::CandidateExplanation;
use crate::debug::provenance::{
    AffineBgProvenance, AffineMatrix, AffineWrap, BackgroundId, RejectionReason, SourceProvenance,
};
use crate::debug::sink::ProvenanceSink;
use crate::memory::{PpuMemoryView, VramLayout, PALETTE_BASE, VRAM_BASE};
use crate::state::{
    AffineReference, CandidatePixel, LatchedState, LayerId, PixelFlags, Scratch,
};

const CHARBLOCK_SIZE: u32 = 0x4000;
const SCREENBLOCK_SIZE: u32 = 0x800;

/// The square map dimension in pixels for an affine `BGxCNT` screen-size field.
fn affine_size_pixels(size: u16) -> i32 {
    match size & 3 {
        0 => 128,
        1 => 256,
        2 => 512,
        _ => 1024,
    }
}

fn background_id(bg: usize) -> BackgroundId {
    if bg == 2 {
        BackgroundId::Bg2
    } else {
        BackgroundId::Bg3
    }
}

/// Render affine background `bg` (2 or 3) for scanline `y`, given the internal
/// reference point already advanced to this line.
#[allow(clippy::too_many_arguments)]
pub fn render_affine_scanline<S: ProvenanceSink>(
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
    let k = bg - 2; // affine parameter index: BG2 -> 0, BG3 -> 1
    let cnt = state.regs.bgcnt[bg];
    let priority = (cnt & 0x3) as u8;
    let char_base = layout.bg_char_base + ((cnt >> 2) & 0x3) as u32 * CHARBLOCK_SIZE;
    let screen_base = layout.bg_screen_base + ((cnt >> 8) & 0x1F) as u32 * SCREENBLOCK_SIZE;
    let wrap = cnt & (1 << 13) != 0;
    let size_px = affine_size_pixels((cnt >> 14) & 0x3);
    let map_tiles = (size_px / 8) as u32;

    let pa = state.regs.bg_pa[k] as i32;
    let pc = state.regs.bg_pc[k] as i32;
    let pb = state.regs.bg_pb[k] as i32;
    let pd = state.regs.bg_pd[k] as i32;
    let layer = LayerId::bg(bg);
    let (mosaic_x, mosaic_y, mosaic) = super::bg_mosaic_factors(state, bg);

    // Mosaic snaps in screen space before the transform: vertically by undoing the
    // per-line PB/PD advances back to the block's top line, horizontally by
    // snapping the screen x fed into the matrix.
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
        let mut tile_index = 0u8;
        let mut tile_byte_offset = 0usize;
        let mut texel = 0u8;
        if in_range {
            let tile_x = (sx / 8) as u32;
            let tile_y = (sy / 8) as u32;
            let px = (sx % 8) as u32;
            let py = (sy % 8) as u32;
            map_offset = (screen_base + tile_y * map_tiles + tile_x) as usize;
            tile_index = mem.vram_u8(map_offset);
            tile_byte_offset = (char_base + tile_index as u32 * 64 + py * 8 + px) as usize;
            texel = mem.vram_u8(tile_byte_offset);
        }
        let opaque = in_range && texel != 0;
        let color = mem.palette15(texel as usize);
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
                    map_entry: tile_index,
                    tile_number: tile_index as u16,
                    tile_address: VRAM_BASE + char_base + tile_index as u32 * 64,
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
    use crate::memory::TestMemory as Memory;
    use crate::debug::provenance::{AffineWrap, SourceProvenance};
    use crate::debug::sink::NullSink;
    use crate::memory::VRAM_BASE;
    use crate::state::{Color15, LayerId, WIDTH};
    use crate::TestPpu as Ppu;

    fn set_palette(mem: &mut Memory, index: usize, color: u16) {
        mem.palette[index * 2..index * 2 + 2].copy_from_slice(&color.to_le_bytes());
    }

    /// A mode-2 BG2 identity scene: char base block 1, an 8bpp tile 1 whose first
    /// row's texels 0 and 1 are palette indices 5 (green) and 7 (red).
    fn identity_scene() -> (Ppu, Memory) {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt(0x0002 | (1 << 10)); // mode 2, BG2 enabled
        ppu.registers.bgcnt[2] = 1 << 2; // char base block 1, screen base 0, size 0
        ppu.registers.bg_pa[0] = 256; // 1.0
        ppu.registers.bg_pd[0] = 256; // 1.0
        set_palette(&mut mem, 0, 0x7C00); // backdrop = blue
        set_palette(&mut mem, 5, 0x03E0); // green
        set_palette(&mut mem, 7, 0x001F); // red
        mem.vram[0] = 1; // map cell (0,0) -> tile 1
        mem.vram[0x4040] = 5; // tile 1 texel (0,0)
        mem.vram[0x4041] = 7; // tile 1 texel (1,0)
        (ppu, mem)
    }

    fn render(ppu: &mut Ppu, mem: &Memory, y: u16) {
        ppu.affine.bg2 = ppu.affine_reference_for_line(0, y);
        ppu.affine.bg3 = ppu.affine_reference_for_line(1, y);
        ppu.latch_for_scanline();
        let view = mem.view();
        ppu.render_scanline(y, &view, &mut NullSink);
    }

    /// The identity matrix maps screen (x, 0) straight to texture (x, 0).
    #[test]
    fn identity_maps_screen_to_texture() {
        let (mut ppu, mem) = identity_scene();
        render(&mut ppu, &mem, 0);
        assert_eq!(ppu.framebuffer()[0], Color15(0x03E0)); // texel (0,0)
        assert_eq!(ppu.framebuffer()[1], Color15(0x001F)); // texel (1,0)
    }

    /// Outside a non-wrapping map, the pixel is transparent and the backdrop shows.
    #[test]
    fn out_of_range_is_transparent_without_wrap() {
        let (mut ppu, mem) = identity_scene();
        ppu.registers.bg_ref_x[0] = 200 << 8; // 200 >= 128 (size 0) -> out of range
        render(&mut ppu, &mem, 0);
        assert_eq!(ppu.framebuffer()[0], Color15(0x7C00)); // backdrop
    }

    /// With the wrap bit, an out-of-range coordinate folds back into the map.
    #[test]
    fn wrap_folds_coordinate_back_into_map() {
        let (mut ppu, mem) = identity_scene();
        ppu.registers.bgcnt[2] |= 1 << 13; // wraparound
        ppu.registers.bg_ref_x[0] = 128 << 8; // 128 wraps to 0 (size 0 = 128px)
        render(&mut ppu, &mem, 0);
        assert_eq!(ppu.framebuffer()[0], Color15(0x03E0)); // wrapped to texel (0,0)
    }

    /// The reference advances by PD per line, so a later scanline samples a lower
    /// texture row.
    #[test]
    fn reference_advances_vertically_per_line() {
        let (mut ppu, mem) = identity_scene();
        let mut mem = mem;
        set_palette(&mut mem, 9, 0x7FFF);
        // Tile 1 texel (0, 3): 8bpp offset 0x4040 + 3*8 + 0 = 0x4058.
        mem.vram[0x4058] = 9;
        render(&mut ppu, &mem, 3); // line 3, pd = 1.0 -> texture_y = 3
        assert_eq!(ppu.framebuffer()[3 * WIDTH], Color15(0x7FFF));
    }

    /// The explanation reports the transformed coordinate, matrix, and addresses.
    #[test]
    fn explain_reports_affine_transform_and_sources() {
        let (mut ppu, mem) = identity_scene();
        let view = mem.view();
        let explanation = ppu.explain_current_pixel(1, 0, &view).unwrap();
        let bg2 = explanation.candidate_for(LayerId::Bg2).expect("BG2 candidate");
        match bg2.provenance {
            SourceProvenance::AffineBg(p) => {
                assert!(p.in_range);
                assert_eq!(p.transformed_x, 1);
                assert_eq!(p.transformed_y, 0);
                assert_eq!(p.matrix.pa, 256);
                assert_eq!(p.tile_number, 1);
                assert_eq!(p.tile_byte_address, VRAM_BASE + 0x4041);
                assert_eq!(p.palette_index, 7);
                assert!(matches!(p.wrap, AffineWrap::Transparent));
            }
            _ => panic!("expected affine provenance"),
        }
    }
}
