//! OBJ (sprite) rendering: OAM evaluation followed by rasterization.
//!
//! The two stages are deliberately separate (spec §19): [`evaluate`] decides which
//! sprites are visible on a scanline under the hardware budget, and [`rasterize`]
//! samples them into the OBJ candidate line and the OBJ-window mask. Sprites are
//! drawn in every video mode, independent of the background mode.

pub mod evaluate;
pub mod rasterize;

use super::debug::sink::ProvenanceSink;
use super::memory::PpuMemoryView;
use super::state::{LatchedState, Scratch};

/// Evaluate and rasterize this scanline's sprites into `scratch`.
pub fn generate<S: ProvenanceSink>(
    y: u16,
    state: &LatchedState,
    mem: &PpuMemoryView,
    scratch: &mut Scratch,
    sink: &mut S,
) {
    if state.forced_blank() || !state.obj_enabled() {
        return;
    }
    evaluate::evaluate_scanline(y, state, mem, &mut scratch.sprites);
    rasterize::rasterize(y, state, mem, &scratch.sprites, &mut scratch.obj, sink);
}

#[cfg(test)]
mod tests {
    use super::evaluate::evaluate_scanline;
    use crate::bus::Memory;
    use crate::ppu::debug::provenance::SourceProvenance;
    use crate::ppu::debug::sink::NullSink;
    use crate::ppu::memory::PpuMemoryView;
    use crate::ppu::state::{Color15, LayerId};
    use crate::ppu::Ppu;

    /// OBJ enabled, 1D tile mapping, all backgrounds off.
    const OBJ_1D: u16 = (1 << 12) | (1 << 6);

    fn set_oam(mem: &mut Memory, index: usize, attr0: u16, attr1: u16, attr2: u16) {
        let b = index * 8;
        mem.oam[b..b + 2].copy_from_slice(&attr0.to_le_bytes());
        mem.oam[b + 2..b + 4].copy_from_slice(&attr1.to_le_bytes());
        mem.oam[b + 4..b + 6].copy_from_slice(&attr2.to_le_bytes());
    }

    fn set_pal(mem: &mut Memory, entry: usize, color: u16) {
        mem.palette[entry * 2..entry * 2 + 2].copy_from_slice(&color.to_le_bytes());
    }

    fn render0(ppu: &mut Ppu, mem: &Memory) {
        ppu.latch_for_scanline();
        let view = PpuMemoryView::new(mem);
        ppu.render_scanline(0, &view, &mut NullSink);
    }

    /// A basic 8×8 4bpp sprite draws its texels and leaves the backdrop elsewhere.
    #[test]
    fn sprite_4bpp_draws_texels_over_backdrop() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt(OBJ_1D);
        set_pal(&mut mem, 0, 0x7C00); // backdrop blue
        set_pal(&mut mem, 256 + 5, 0x03E0); // OBJ green
        set_pal(&mut mem, 256 + 3, 0x001F); // OBJ red
        set_oam(&mut mem, 0, 0, 0, 1); // (0,0) 8×8 4bpp, tile 1
        mem.vram[0x10020] = 0x35; // texel 0 = 5, texel 1 = 3

        render0(&mut ppu, &mem);
        assert_eq!(ppu.framebuffer()[0], Color15(0x03E0));
        assert_eq!(ppu.framebuffer()[1], Color15(0x001F));
        assert_eq!(ppu.framebuffer()[2], Color15(0x7C00)); // backdrop
    }

    /// Horizontal flip mirrors the sprite's texels across its width.
    #[test]
    fn sprite_hflip_mirrors_texels() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt(OBJ_1D);
        set_pal(&mut mem, 256 + 5, 0x03E0);
        set_oam(&mut mem, 0, 0, 1 << 12, 1); // H-flip
        mem.vram[0x10023] = 0x50; // texel 7 (byte 3 high nibble) = 5

        render0(&mut ppu, &mem);
        assert_eq!(ppu.framebuffer()[0], Color15(0x03E0)); // flipped: texel 7 at x=0
    }

    /// An 8bpp sprite uses the whole byte as a 256-color OBJ palette index.
    #[test]
    fn sprite_8bpp_indexes_full_byte() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt(OBJ_1D);
        set_pal(&mut mem, 256 + 200, 0x7FFF);
        set_oam(&mut mem, 0, 1 << 13, 0, 1); // 8bpp
        mem.vram[0x10020] = 200; // tile 1 (base 0x10020), texel (0,0)

        render0(&mut ppu, &mem);
        assert_eq!(ppu.framebuffer()[0], Color15(0x7FFF));
    }

    /// Where two sprites overlap, the lower OAM index is drawn in front.
    #[test]
    fn lower_oam_index_wins_overlap() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt(OBJ_1D);
        set_pal(&mut mem, 256 + 5, 0x03E0); // sprite 0 green
        set_pal(&mut mem, 256 + 6, 0x001F); // sprite 1 red
        set_oam(&mut mem, 0, 0, 0, 1); // sprite 0 -> tile 1
        set_oam(&mut mem, 1, 0, 0, 2); // sprite 1 -> tile 2, same position
        mem.vram[0x10020] = 0x05; // tile 1 texel 0 = 5
        mem.vram[0x10040] = 0x06; // tile 2 texel 0 = 6

        render0(&mut ppu, &mem);
        assert_eq!(ppu.framebuffer()[0], Color15(0x03E0)); // sprite 0 wins
    }

    /// A higher-priority sprite (lower priority value) wins over an overlapping
    /// sprite with a lower OAM index but a worse priority.
    #[test]
    fn higher_priority_sprite_wins_over_lower_oam_index() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt(OBJ_1D);
        set_pal(&mut mem, 256 + 5, 0x03E0); // sprite 0 green
        set_pal(&mut mem, 256 + 6, 0x001F); // sprite 1 red
        set_oam(&mut mem, 0, 0, 0, 1 | (3 << 10)); // sprite 0, priority 3
        set_oam(&mut mem, 1, 0, 0, 2); // sprite 1, priority 0
        mem.vram[0x10020] = 0x05;
        mem.vram[0x10040] = 0x06;

        render0(&mut ppu, &mem);
        assert_eq!(ppu.framebuffer()[0], Color15(0x001F)); // sprite 1 (priority 0) wins
    }

    /// OBJ priority competes with background priority.
    #[test]
    fn obj_priority_competes_with_background() {
        let scene = |obj_priority: u16| {
            let mut ppu = Ppu::new();
            let mut mem = Memory::default();
            // Mode 3, BG2 enabled at priority 2, plus OBJ.
            ppu.write_dispcnt(0x0003 | (1 << 10) | (1 << 12) | (1 << 6));
            ppu.registers.bgcnt[2] = 2; // BG2 priority 2
            mem.vram[0..2].copy_from_slice(&0x7C00u16.to_le_bytes()); // BG2 pixel (0,0) blue
            set_pal(&mut mem, 256 + 5, 0x03E0); // OBJ green
            // Sprite uses tile 512 (0x14000) to avoid the bitmap region.
            set_oam(&mut mem, 0, 0, 0, (512 & 0x3FF) | (obj_priority << 10));
            mem.vram[0x14000] = 0x05;
            render0(&mut ppu, &mem);
            ppu.framebuffer()[0]
        };
        assert_eq!(scene(0), Color15(0x03E0)); // OBJ priority 0 beats BG2 priority 2
        assert_eq!(scene(3), Color15(0x7C00)); // OBJ priority 3 loses to BG2 priority 2
    }

    /// An identity-matrix affine sprite maps screen offset straight to texture.
    #[test]
    fn affine_identity_sprite_samples_directly() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt(OBJ_1D);
        set_pal(&mut mem, 256 + 5, 0x03E0);
        // Sprite 0: affine (bit 8), group 0, 8×8, tile 1.
        set_oam(&mut mem, 0, 1 << 8, 0, 1);
        // Disable sprites 1-3 (their 4th OAM words hold PB/PC/PD).
        set_oam(&mut mem, 1, 1 << 9, 0, 0);
        set_oam(&mut mem, 2, 1 << 9, 0, 0);
        set_oam(&mut mem, 3, 1 << 9, 0, 0);
        // Identity matrix: PA = PD = 1.0 (256), PB = PC = 0.
        mem.oam[0x06..0x08].copy_from_slice(&256u16.to_le_bytes()); // PA
        mem.oam[0x1E..0x20].copy_from_slice(&256u16.to_le_bytes()); // PD
        mem.vram[0x10020] = 0x05; // tile 1 texel (0,0) = 5

        render0(&mut ppu, &mem);
        assert_eq!(ppu.framebuffer()[0], Color15(0x03E0));
    }

    /// The per-scanline budget rejects sprites once exhausted.
    #[test]
    fn hardware_budget_limits_sprites_per_line() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt(OBJ_1D); // budget 1210, normal sprites
        // 40 horizontal 64×32 sprites at (0,0): each costs 64 cycles.
        for i in 0..40 {
            set_oam(&mut mem, i, 1 << 14, 3 << 14, 1); // shape 1, size 3 -> 64×32
        }
        ppu.latch_for_scanline();
        let view = PpuMemoryView::new(&mem);
        let mut out = Vec::new();
        evaluate_scanline(0, &ppu.latched, &view, &mut out);
        // floor(1210 / 64) = 18 sprites fit.
        assert_eq!(out.len(), 18);
    }

    /// The explanation reports the sprite's OAM entry and tile/palette sources.
    #[test]
    fn explain_reports_obj_sources() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt(OBJ_1D);
        set_pal(&mut mem, 256 + 5, 0x03E0);
        set_oam(&mut mem, 7, 0, 0, 1); // sprite 7, tile 1
        mem.vram[0x10020] = 0x05;

        let view = PpuMemoryView::new(&mem);
        let explanation = ppu.explain_current_pixel(0, 0, &view).unwrap();
        let obj = explanation.candidate_for(LayerId::Obj).expect("OBJ candidate");
        match obj.provenance {
            SourceProvenance::Obj(p) => {
                assert_eq!(p.oam_index, 7);
                assert_eq!(p.oam_address, 0x0700_0000 + 7 * 8);
                assert_eq!(p.tile_number, 1);
                assert_eq!(p.tile_byte_address, 0x0600_0000 + 0x10020);
                assert_eq!(p.palette_address, 0x0500_0000 + (256 + 5) * 2);
            }
            _ => panic!("expected OBJ provenance"),
        }
    }
}
