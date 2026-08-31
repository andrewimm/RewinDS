//! The CPU-visible video register block (`0x4000000`–`0x4000054`).
//!
//! This holds the raw register values as the CPU writes them. It is deliberately
//! separate from the [`LatchedState`](super::state::LatchedState) the renderer
//! reads: a mid-frame write lands here immediately, but only affects a scanline
//! that latches after it. `DISPSTAT`/`VCOUNT` are *not* here — those are timing
//! state, owned by [`super::timing::TimingState`].

/// The video registers, addressed by their `0x4000000`-relative offset.
#[derive(Clone, Copy, Debug, Default)]
pub struct Registers {
    /// `DISPCNT` (0x000): video mode, background/OBJ enables, forced blank.
    pub dispcnt: u16,
    /// `BG0CNT`..`BG3CNT` (0x008, 0x00A, 0x00C, 0x00E).
    pub bgcnt: [u16; 4],
    /// `BGxHOFS` horizontal scroll (0x010, 0x014, 0x018, 0x01C).
    pub bg_hofs: [u16; 4],
    /// `BGxVOFS` vertical scroll (0x012, 0x016, 0x01A, 0x01E).
    pub bg_vofs: [u16; 4],
    /// Affine parameters PA/PB/PC/PD for BG2 (`[0]`) and BG3 (`[1]`).
    pub bg_pa: [i16; 2],
    pub bg_pb: [i16; 2],
    pub bg_pc: [i16; 2],
    pub bg_pd: [i16; 2],
    /// Affine reference points `BGxX`/`BGxY`, 28-bit signed, for BG2/BG3.
    pub bg_ref_x: [i32; 2],
    pub bg_ref_y: [i32; 2],
    /// Window horizontal/vertical bounds `WIN0`/`WIN1` (0x040-0x046).
    pub win_h: [u16; 2],
    pub win_v: [u16; 2],
    /// `WININ`/`WINOUT` (0x048/0x04A).
    pub winin: u16,
    pub winout: u16,
    /// `MOSAIC` (0x04C).
    pub mosaic: u16,
    /// `BLDCNT`/`BLDALPHA`/`BLDY` (0x050/0x052/0x054).
    pub bldcnt: u16,
    pub bldalpha: u16,
    pub bldy: u16,
}

/// Merge a masked write into a register's current value.
fn merge(current: u16, value: u16, mask: u16) -> u16 {
    (current & !mask) | (value & mask)
}

/// Sign-extend a 28-bit affine reference (bit 27 is the sign) to `i32`.
fn sign_extend_28(raw: u32) -> i32 {
    let raw = raw & 0x0FFF_FFFF;
    if raw & 0x0800_0000 != 0 {
        (raw | 0xF000_0000) as i32
    } else {
        raw as i32
    }
}

impl Registers {
    /// The affine-BG index (0 = BG2, 1 = BG3) for a register offset in the two
    /// affine blocks (0x020-0x02F and 0x030-0x03F).
    fn affine_index(offset: u32) -> usize {
        if offset >= 0x030 {
            1
        } else {
            0
        }
    }

    /// Merge a masked write into the reference point `ref_value`, replacing either
    /// the low halfword or the high halfword, then re-sign-extending from bit 27.
    fn write_ref(ref_value: &mut i32, value: u16, mask: u16, high: bool) {
        let raw = *ref_value as u32;
        let updated = if high {
            let low = raw & 0xFFFF;
            let high_half = merge((raw >> 16) as u16, value, mask) as u32;
            (high_half << 16) | low
        } else {
            let low = merge(raw as u16, value, mask) as u32;
            (raw & 0xFFFF_0000) | low
        };
        *ref_value = sign_extend_28(updated);
    }

    /// Apply a masked 16-bit write to the video register at `offset` (in the
    /// `0x008..=0x054` block; `DISPCNT`/`DISPSTAT`/`VCOUNT` are handled elsewhere).
    pub fn write16(&mut self, offset: u32, value: u16, mask: u16) {
        match offset {
            0x008 | 0x00A | 0x00C | 0x00E => {
                let i = ((offset - 0x008) / 2) as usize;
                self.bgcnt[i] = merge(self.bgcnt[i], value, mask);
            }
            0x010 | 0x014 | 0x018 | 0x01C => {
                let i = ((offset - 0x010) / 4) as usize;
                self.bg_hofs[i] = merge(self.bg_hofs[i], value, mask) & 0x01FF;
            }
            0x012 | 0x016 | 0x01A | 0x01E => {
                let i = ((offset - 0x012) / 4) as usize;
                self.bg_vofs[i] = merge(self.bg_vofs[i], value, mask) & 0x01FF;
            }
            0x020 | 0x030 => {
                let i = Self::affine_index(offset);
                self.bg_pa[i] = merge(self.bg_pa[i] as u16, value, mask) as i16;
            }
            0x022 | 0x032 => {
                let i = Self::affine_index(offset);
                self.bg_pb[i] = merge(self.bg_pb[i] as u16, value, mask) as i16;
            }
            0x024 | 0x034 => {
                let i = Self::affine_index(offset);
                self.bg_pc[i] = merge(self.bg_pc[i] as u16, value, mask) as i16;
            }
            0x026 | 0x036 => {
                let i = Self::affine_index(offset);
                self.bg_pd[i] = merge(self.bg_pd[i] as u16, value, mask) as i16;
            }
            0x028 | 0x038 => Self::write_ref(&mut self.bg_ref_x[Self::affine_index(offset)], value, mask, false),
            0x02A | 0x03A => Self::write_ref(&mut self.bg_ref_x[Self::affine_index(offset)], value, mask, true),
            0x02C | 0x03C => Self::write_ref(&mut self.bg_ref_y[Self::affine_index(offset)], value, mask, false),
            0x02E | 0x03E => Self::write_ref(&mut self.bg_ref_y[Self::affine_index(offset)], value, mask, true),
            // Window bounds are grouped by axis, not by window: both horizontal
            // registers precede both vertical ones (WIN0H, WIN1H, WIN0V, WIN1V).
            0x040 => self.win_h[0] = merge(self.win_h[0], value, mask),
            0x042 => self.win_h[1] = merge(self.win_h[1], value, mask),
            0x044 => self.win_v[0] = merge(self.win_v[0], value, mask),
            0x046 => self.win_v[1] = merge(self.win_v[1], value, mask),
            0x048 => self.winin = merge(self.winin, value, mask),
            0x04A => self.winout = merge(self.winout, value, mask),
            0x04C => self.mosaic = merge(self.mosaic, value, mask),
            0x050 => self.bldcnt = merge(self.bldcnt, value, mask),
            0x052 => self.bldalpha = merge(self.bldalpha, value, mask),
            0x054 => self.bldy = merge(self.bldy, value, mask),
            _ => {}
        }
    }

    /// Read back the video register at `offset`. Write-only registers (scroll,
    /// affine parameters and reference points, window bounds, `MOSAIC`, `BLDY`)
    /// read as 0.
    pub fn read16(&self, offset: u32) -> u16 {
        match offset {
            0x008 | 0x00A | 0x00C | 0x00E => self.bgcnt[((offset - 0x008) / 2) as usize],
            0x048 => self.winin,
            0x04A => self.winout,
            0x050 => self.bldcnt,
            0x052 => self.bldalpha,
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Window bound registers are laid out by axis (WIN0H, WIN1H, WIN0V, WIN1V),
    /// not interleaved by window. A prior mapping swapped WIN1H with WIN0V, which
    /// blanked whole scanlines whenever a game (e.g. FireRed's letterboxed intro)
    /// used WIN1's horizontal bound.
    #[test]
    fn window_bounds_route_by_axis() {
        let mut r = Registers::default();
        r.write16(0x040, 0x1122, 0xFFFF); // WIN0H
        r.write16(0x042, 0x3344, 0xFFFF); // WIN1H
        r.write16(0x044, 0x5566, 0xFFFF); // WIN0V
        r.write16(0x046, 0x7788, 0xFFFF); // WIN1V
        assert_eq!(r.win_h, [0x1122, 0x3344]);
        assert_eq!(r.win_v, [0x5566, 0x7788]);
    }
}
