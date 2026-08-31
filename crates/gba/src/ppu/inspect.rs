//! Semantic inspection accessors — the queryable surface a debugger or agent uses
//! to ask about video state without a screenshot.
//!
//! The rendering-based accessors re-run the target scanline (single-segment, from
//! the current registers) with no instrumentation, leaving the per-layer scratch
//! populated for readout. They are deterministic: the same state yields the same
//! answer, and the same pixels the framebuffer shows.

use super::debug::explain::WindowExplanation;
use super::debug::provenance::{BackgroundId, ObjProvenance, SourceProvenance};
use super::debug::sink::NullSink;
use super::memory::PpuMemoryView;
use super::obj::evaluate::SpriteInstance;
use super::state::{CandidatePixel, HEIGHT, WIDTH};
use super::Ppu;

/// A background's live configuration, for `backgrounds()`.
#[derive(Clone, Copy, Debug)]
pub struct BackgroundSummary {
    pub id: BackgroundId,
    pub enabled: bool,
    pub priority: u8,
    /// The kind of background this is in the current video mode.
    pub kind: BackgroundKind,
}

/// How a background is being generated in the current mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundKind {
    Disabled,
    Text,
    Affine,
    Bitmap,
}

fn background_id(index: usize) -> BackgroundId {
    match index {
        0 => BackgroundId::Bg0,
        1 => BackgroundId::Bg1,
        2 => BackgroundId::Bg2,
        _ => BackgroundId::Bg3,
    }
}

impl Ppu {
    /// The current video mode (`DISPCNT` bits 0-2).
    pub fn current_mode(&self) -> u8 {
        (self.registers.dispcnt & 0x7) as u8
    }

    /// The kind of background `index` is in the current mode.
    fn background_kind(&self, index: usize) -> BackgroundKind {
        if self.registers.dispcnt & (1 << (8 + index)) == 0 {
            return BackgroundKind::Disabled;
        }
        match (self.current_mode(), index) {
            (0, _) => BackgroundKind::Text,
            (1, 0) | (1, 1) => BackgroundKind::Text,
            (1, 2) => BackgroundKind::Affine,
            (2, 2) | (2, 3) => BackgroundKind::Affine,
            (3..=5, 2) => BackgroundKind::Bitmap,
            _ => BackgroundKind::Disabled,
        }
    }

    /// A summary of all four backgrounds' current configuration.
    pub fn backgrounds(&self) -> [BackgroundSummary; 4] {
        std::array::from_fn(|index| {
            let kind = self.background_kind(index);
            BackgroundSummary {
                id: background_id(index),
                enabled: kind != BackgroundKind::Disabled,
                priority: (self.registers.bgcnt[index] & 0x3) as u8,
                kind,
            }
        })
    }

    /// The backgrounds actively producing pixels in the current mode.
    pub fn active_backgrounds(&self) -> Vec<BackgroundId> {
        self.backgrounds()
            .iter()
            .filter(|b| b.enabled)
            .map(|b| b.id)
            .collect()
    }

    /// Re-render scanline `y` (single segment, current registers) into the scratch
    /// buffers for readout.
    fn populate_scratch(&mut self, y: u16, mem: &PpuMemoryView<'_>) {
        self.affine.bg2 = self.affine_reference_for_line(0, y);
        self.affine.bg3 = self.affine_reference_for_line(1, y);
        self.latch_for_scanline();
        self.render_scanline(y, mem, &mut NullSink);
    }

    /// The candidate a background layer contributes at `(x, y)`, or `None` if it is
    /// transparent, disabled, or out of range.
    pub fn layer_pixel(
        &mut self,
        bg_index: usize,
        x: u16,
        y: u16,
        mem: &PpuMemoryView<'_>,
    ) -> Option<CandidatePixel> {
        if bg_index >= 4 || x as usize >= WIDTH || y as usize >= HEIGHT {
            return None;
        }
        self.populate_scratch(y, mem);
        self.scratch.bg[bg_index].pixels[x as usize]
    }

    /// The OBJ candidate at `(x, y)`, or `None` if no sprite covers it.
    pub fn obj_pixel(&mut self, x: u16, y: u16, mem: &PpuMemoryView<'_>) -> Option<CandidatePixel> {
        if x as usize >= WIDTH || y as usize >= HEIGHT {
            return None;
        }
        self.populate_scratch(y, mem);
        self.scratch.obj.pixels[x as usize]
    }

    /// The window decision at `(x, y)`.
    pub fn window_at(&mut self, x: u16, y: u16, mem: &PpuMemoryView<'_>) -> WindowExplanation {
        let x = (x as usize).min(WIDTH - 1);
        let y = (y as usize).min(HEIGHT - 1) as u16;
        self.populate_scratch(y, mem);
        let mask = self.scratch.window.mask[x];
        WindowExplanation {
            region: self.scratch.window.region[x],
            layers_enabled: [mask.bg[0], mask.bg[1], mask.bg[2], mask.bg[3], mask.obj],
            effects_enabled: mask.effects,
        }
    }

    /// The sprites evaluated as visible on scanline `y`, in OAM order (subject to
    /// the per-scanline budget).
    pub fn sprites_on_scanline(&mut self, y: u16, mem: &PpuMemoryView<'_>) -> Vec<SpriteInstance> {
        if y as usize >= HEIGHT {
            return Vec::new();
        }
        self.populate_scratch(y, mem);
        self.scratch.sprites.clone()
    }

    /// The provenance of the sprite that owns pixel `(x, y)`, if any.
    pub fn sprite_at(&mut self, x: u16, y: u16, mem: &PpuMemoryView<'_>) -> Option<ObjProvenance> {
        let explanation = self.explain_current_pixel(x, y, mem).ok()?;
        explanation
            .candidates
            .iter()
            .find_map(|c| match c.provenance {
                SourceProvenance::Obj(p) => Some(p),
                _ => None,
            })
    }

    /// The guest memory addresses feeding pixel `(x, y)` — the top candidate's
    /// sampled sources, for handing to a memory writer query.
    pub fn source_memory(&mut self, x: u16, y: u16, mem: &PpuMemoryView<'_>) -> Vec<u32> {
        match self.explain_current_pixel(x, y, mem) {
            Ok(explanation) => {
                let top = explanation.resolved.top.layer;
                explanation
                    .candidates
                    .iter()
                    .find(|c| c.candidate.layer == top)
                    .map(|c| c.provenance.source_addresses())
                    .unwrap_or_default()
            }
            Err(_) => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::Memory;
    use crate::ppu::memory::VRAM_BASE;

    fn set_oam(mem: &mut Memory, index: usize, attr0: u16, attr1: u16, attr2: u16) {
        let b = index * 8;
        mem.oam[b..b + 2].copy_from_slice(&attr0.to_le_bytes());
        mem.oam[b + 2..b + 4].copy_from_slice(&attr1.to_le_bytes());
        mem.oam[b + 4..b + 6].copy_from_slice(&attr2.to_le_bytes());
    }

    #[test]
    fn mode_and_backgrounds_report_configuration() {
        let mut ppu = Ppu::new();
        ppu.write_dispcnt((1 << 8) | (1 << 10)); // mode 0, BG0 and BG2 on
        ppu.registers.bgcnt[0] = 2; // BG0 priority 2
        assert_eq!(ppu.current_mode(), 0);

        let bgs = ppu.backgrounds();
        assert!(bgs[0].enabled && bgs[0].kind == BackgroundKind::Text && bgs[0].priority == 2);
        assert!(!bgs[1].enabled);
        assert!(bgs[2].enabled && bgs[2].kind == BackgroundKind::Text);
        assert_eq!(ppu.active_backgrounds().len(), 2);
    }

    #[test]
    fn bitmap_mode_marks_bg2_as_bitmap() {
        let mut ppu = Ppu::new();
        ppu.write_dispcnt(0x0003 | (1 << 10));
        assert_eq!(ppu.backgrounds()[2].kind, BackgroundKind::Bitmap);
    }

    #[test]
    fn sprite_at_and_source_memory_resolve_a_pixel() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt((1 << 12) | (1 << 6)); // OBJ enabled, 1D mapping
        mem.palette[(256 + 5) * 2..(256 + 5) * 2 + 2].copy_from_slice(&0x03E0u16.to_le_bytes());
        set_oam(&mut mem, 3, 0, 0, 1); // sprite 3, tile 1 at (0,0)
        mem.vram[0x10020] = 0x05;

        let view = PpuMemoryView::new(&mem.vram, &mem.palette, &mem.oam);
        let sprite = ppu.sprite_at(0, 0, &view).expect("sprite covers pixel");
        assert_eq!(sprite.oam_index, 3);

        let addrs = ppu.source_memory(0, 0, &view);
        assert!(addrs.contains(&(VRAM_BASE + 0x10020)));
    }

    #[test]
    fn sprites_on_scanline_lists_visible_sprites() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt((1 << 12) | (1 << 6));
        // A zeroed OAM entry is a valid 8×8 sprite at the origin, so disable all,
        // then place two.
        for i in 0..128 {
            set_oam(&mut mem, i, 1 << 9, 0, 0);
        }
        set_oam(&mut mem, 0, 0, 0, 1); // on line 0
        set_oam(&mut mem, 1, 100, 0, 1); // Y=100, not on line 0

        let view = PpuMemoryView::new(&mem.vram, &mem.palette, &mem.oam);
        let sprites = ppu.sprites_on_scanline(0, &view);
        assert_eq!(sprites.len(), 1);
        assert_eq!(sprites[0].oam_index, 0);
    }
}
