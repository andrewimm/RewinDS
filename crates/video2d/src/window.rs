//! Window masking — reducing the window registers to a per-pixel visibility mask.
//!
//! The compositor never sees raw window-register encoding: this module resolves,
//! for each pixel, which window region it belongs to (WIN0 > WIN1 > OBJ-window >
//! outside) and expands that region's enable bits into a [`WindowMask`]. When no
//! window is enabled in `DISPCNT`, every layer is visible everywhere.

use super::debug::provenance::WindowRegion;
use super::state::{LatchedState, ObjLine, WindowLine, WindowMask};

/// Expand the six per-region enable bits (BG0-3, OBJ, effects) into a mask.
fn mask_from_bits(bits: u16) -> WindowMask {
    WindowMask {
        bg: [
            bits & 1 != 0,
            bits & 2 != 0,
            bits & 4 != 0,
            bits & 8 != 0,
        ],
        obj: bits & (1 << 4) != 0,
        effects: bits & (1 << 5) != 0,
    }
}

/// Whether `coord` lies in the half-open span packed into a window bound register:
/// the first coordinate (leftmost/topmost) in the high byte, the second (rightmost
/// or bottom-most, *plus one*) in the low byte.
///
/// Per GBATEK, a second coordinate past the screen edge (`X2>240` / `Y2>160`), or a
/// first coordinate greater than the second (`X1>X2` / `Y1>Y2`), is a garbage value
/// that hardware reinterprets as the second coordinate being the screen edge: the
/// window then spans `[first, edge)`. Crucially it does **not** wrap around to zero —
/// an earlier version modelled `first > second` as a wrapping span, which hid whole
/// backgrounds whenever a game (e.g. Pokémon, Advance Wars) programmed such a bound.
fn in_span(coord: u16, bound: u16, edge: u16) -> bool {
    let first = bound >> 8;
    let mut second = bound & 0xFF;
    if second > edge || first > second {
        second = edge;
    }
    coord >= first && coord < second
}

/// Resolve the window mask for every pixel of scanline `y`.
pub fn compute_line(
    y: u16,
    state: &LatchedState,
    obj: &ObjLine,
    width: usize,
    height: usize,
    out: &mut WindowLine,
) {
    let regs = &state.regs;
    let win0_on = regs.dispcnt & (1 << 13) != 0;
    let win1_on = regs.dispcnt & (1 << 14) != 0;
    let objwin_on = regs.dispcnt & (1 << 15) != 0;

    // With no window active, everything is visible and the region is "outside".
    if !win0_on && !win1_on && !objwin_on {
        out.mask.fill(WindowMask::all_visible());
        out.region.fill(WindowRegion::Outside);
        return;
    }

    let win0_row = win0_on && in_span(y, regs.win_v[0], height as u16);
    let win1_row = win1_on && in_span(y, regs.win_v[1], height as u16);
    let win0_mask = mask_from_bits(regs.winin & 0x3F);
    let win1_mask = mask_from_bits((regs.winin >> 8) & 0x3F);
    let outside_mask = mask_from_bits(regs.winout & 0x3F);
    let objwin_mask = mask_from_bits((regs.winout >> 8) & 0x3F);

    for x in 0..width {
        let (region, mask) = if win0_row && in_span(x as u16, regs.win_h[0], width as u16) {
            (WindowRegion::Win0, win0_mask)
        } else if win1_row && in_span(x as u16, regs.win_h[1], width as u16) {
            (WindowRegion::Win1, win1_mask)
        } else if objwin_on && obj.window[x] {
            (WindowRegion::ObjWindow, objwin_mask)
        } else {
            (WindowRegion::Outside, outside_mask)
        };
        out.mask[x] = mask;
        out.region[x] = region;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::TestMemory as Memory;
    use crate::debug::sink::NullSink;
    use crate::state::Color15;
    use crate::TestPpu as Ppu;

    #[test]
    fn in_span_normal_bounds() {
        // Normal span [10, 20).
        assert!(in_span(15, (10 << 8) | 20, 240));
        assert!(!in_span(20, (10 << 8) | 20, 240));
        assert!(!in_span(9, (10 << 8) | 20, 240));
    }

    #[test]
    fn in_span_garbage_bounds_clamp_to_edge_not_wrap() {
        // first (200) > second (30): hardware clamps second to the edge, giving
        // [200, 240) — it does NOT wrap around to also cover [0, 30).
        assert!(in_span(220, (200 << 8) | 30, 240)); // inside [200, 240)
        assert!(in_span(239, (200 << 8) | 30, 240)); // still inside up to the edge
        assert!(!in_span(10, (200 << 8) | 30, 240)); // would be covered by a wrap; must not be
        assert!(!in_span(100, (200 << 8) | 30, 240));
        // second past the edge is likewise clamped: [5, 240).
        assert!(in_span(200, (5 << 8) | 250, 240));
        assert!(!in_span(4, (5 << 8) | 250, 240));
    }

    #[test]
    fn no_active_window_leaves_everything_visible() {
        let state = LatchedState::default(); // DISPCNT window bits clear
        let obj = ObjLine::default();
        let mut line = WindowLine::default();
        compute_line(0, &state, &obj, 240, 160, &mut line);
        assert!(line.mask[0].bg.iter().all(|&b| b));
        assert!(line.mask[239].obj);
    }

    fn render0(ppu: &mut Ppu, mem: &Memory) {
        ppu.latch_for_scanline();
        let view = mem.view();
        ppu.render_scanline(0, &view, &mut NullSink);
    }

    /// WIN0 shows BG2 only inside its bounds; outside, BG2 is hidden (backdrop).
    #[test]
    fn win0_clips_background_to_its_region() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        // Mode 3, BG2, WIN0 enabled.
        ppu.write_dispcnt(0x0003 | (1 << 10) | (1 << 13));
        ppu.registers.win_h[0] = 120; // x in [0, 120)
        ppu.registers.win_v[0] = 160; // y in [0, 160)
        ppu.registers.winin = 0x0004; // WIN0: BG2 enabled
        ppu.registers.winout = 0x0000; // outside: BG2 disabled
        mem.palette[0..2].copy_from_slice(&0x7C00u16.to_le_bytes()); // backdrop blue
        mem.vram[0..2].copy_from_slice(&0x03E0u16.to_le_bytes()); // BG2 (0,0) green
        let off = 130 * 2;
        mem.vram[off..off + 2].copy_from_slice(&0x03E0u16.to_le_bytes()); // BG2 (130,0) green

        render0(&mut ppu, &mem);
        assert_eq!(ppu.framebuffer()[0], Color15(0x03E0)); // inside WIN0 -> BG2
        assert_eq!(ppu.framebuffer()[130], Color15(0x7C00)); // outside -> backdrop
    }

    /// Where WIN0 and WIN1 overlap, WIN0's enables win.
    #[test]
    fn win0_takes_precedence_over_win1() {
        let mut ppu = Ppu::new();
        let mut mem = Memory::default();
        ppu.write_dispcnt(0x0003 | (1 << 10) | (1 << 13) | (1 << 14));
        // Both windows cover x=0.
        ppu.registers.win_h[0] = 100;
        ppu.registers.win_v[0] = 160;
        ppu.registers.win_h[1] = 100;
        ppu.registers.win_v[1] = 160;
        ppu.registers.winin = 0x0004; // WIN0 BG2 on (low byte); WIN1 BG2 off (high byte)
        mem.palette[0..2].copy_from_slice(&0x7C00u16.to_le_bytes());
        mem.vram[0..2].copy_from_slice(&0x03E0u16.to_le_bytes());

        render0(&mut ppu, &mem);
        assert_eq!(ppu.framebuffer()[0], Color15(0x03E0)); // WIN0 region -> BG2 shown
    }
}
