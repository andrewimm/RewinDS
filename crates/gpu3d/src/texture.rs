//! Texture sampling: decode `TEXIMAGE_PARAM` / `PLTT_BASE` and fetch a texel from
//! the assembled texture-image and texture-palette VRAM.
//!
//! All seven texture formats fetch a single texel and return it as 6-bit RGB plus a
//! 5-bit alpha (`0` = transparent, `31` = solid). The two "translucent" formats
//! (A3I5, A5I3) and Direct-color carry per-texel alpha; the palette formats are
//! opaque except for the optional transparent color 0. The 4×4 compressed format (5)
//! pairs 2-bit block indices with slot-1 palette-info halfwords (see `compressed`).
//!
//! Coordinates arrive already reduced to integer texels (the 1.11.4 texcoord shifted
//! right by 4); wrapping (clamp / repeat / flip) is applied here against the texture
//! size. Everything is integer — no `f32`.

/// Borrowed texture VRAM for one render: the assembled texture image (up to 512 KB,
/// banks A–D) and the texture palette (up to `0x18000`, banks E/F/G).
#[derive(Clone, Copy, Default)]
pub struct TextureSet<'a> {
    pub image: &'a [u8],
    pub palette: &'a [u8],
}

/// One sampled texel: 6-bit-per-channel color and 5-bit alpha (`0` = transparent).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Texel {
    pub color: [u8; 3],
    pub alpha: u8,
}

/// The decoded `TEXIMAGE_PARAM` (+ `PLTT_BASE`) fields the sampler needs.
#[derive(Clone, Copy, Debug, Default)]
pub struct TexParams {
    /// Texture format 0–7 (0 = none).
    pub format: u8,
    /// Byte offset of the texel data into the assembled image (VRAM offset × 8).
    pub offset: u32,
    pub size_s: i32,
    pub size_t: i32,
    pub repeat_s: bool,
    pub repeat_t: bool,
    pub flip_s: bool,
    pub flip_t: bool,
    /// Palette color 0 is transparent (palette formats 2/3/4 only).
    pub color0_transparent: bool,
    /// Raw `PLTT_BASE` parameter (its low 13 bits are the palette base, in 8- or
    /// 16-byte steps depending on format).
    pub pltt_base: u32,
}

impl TexParams {
    /// Decode a polygon's `TEXIMAGE_PARAM` and `PLTT_BASE` register values.
    pub fn decode(tex_param: u32, pltt_base: u32) -> Self {
        TexParams {
            format: ((tex_param >> 26) & 7) as u8,
            offset: (tex_param & 0xFFFF) << 3,
            size_s: 8 << ((tex_param >> 20) & 7),
            size_t: 8 << ((tex_param >> 23) & 7),
            repeat_s: tex_param & (1 << 16) != 0,
            repeat_t: tex_param & (1 << 17) != 0,
            flip_s: tex_param & (1 << 18) != 0,
            flip_t: tex_param & (1 << 19) != 0,
            color0_transparent: tex_param & (1 << 29) != 0,
            pltt_base,
        }
    }

    /// Whether this polygon carries a (sampleable) texture.
    pub fn textured(&self) -> bool {
        self.format != 0
    }

    /// Sample the texel at integer texture coordinates `(s, t)` (the 1.11.4 texcoord
    /// already shifted right by 4). Returns 6-bit color + 5-bit alpha; out-of-range
    /// fetches and the unsupported 4×4 format read as transparent.
    pub fn sample(&self, tex: &TextureSet, s: i32, t: i32) -> Texel {
        let s = wrap(s, self.size_s, self.repeat_s, self.flip_s);
        let t = wrap(t, self.size_t, self.repeat_t, self.flip_t);
        let lin = (t * self.size_s + s) as usize;
        let base = self.offset as usize;
        match self.format {
            1 => self.a3i5(tex, base + lin),
            2 => self.palette_2bpp(tex, base, lin, s),
            3 => self.palette_4bpp(tex, base, lin, s),
            4 => self.palette_8bpp(tex, base + lin),
            5 => self.compressed(tex, s, t),
            6 => self.a5i3(tex, base + lin),
            7 => direct(tex, base + lin * 2),
            _ => Texel::default(), // 0 = none → transparent
        }
    }

    /// Format 5 — 4×4-texel compressed. The texture is a grid of 4×4-texel blocks;
    /// each block is 4 bytes of 2-bit indices in the texel data, paired with a 16-bit
    /// palette-info halfword in the parallel "slot 1" region (`0x20000` above the texel
    /// data, stepping `0x10000` for texel data in the upper texture half). The info's
    /// mode (bits 14-15) selects how the 2-bit index maps to the block's palette:
    /// mode 0/2 use up to 3/4 stored colors, modes 1/3 interpolate two — modes 0/1
    /// make index 3 transparent. The palette base is `PLTT_BASE` (16-byte units) plus
    /// the info's 14-bit offset (4-byte units), addressing `RGB555` entries.
    fn compressed(&self, tex: &TextureSet, s: i32, t: i32) -> Texel {
        let blocks_per_row = (self.size_s / 4).max(1) as usize;
        let block = (t / 4) as usize * blocks_per_row + (s / 4) as usize;
        let texel_addr = self.offset as usize + block * 4;
        let Some(&row) = tex.image.get(texel_addr + (t & 3) as usize) else {
            return Texel::default();
        };
        let value = (row >> ((s & 3) * 2)) & 3;
        // The block's palette-info halfword lives in the parallel slot-1 region.
        let mut info_addr = 0x2_0000 + ((texel_addr & 0x1_FFFF) >> 1);
        if texel_addr >= 0x4_0000 {
            info_addr += 0x1_0000;
        }
        let info = read16(tex.image, info_addr);
        let mode = (info >> 14) & 3;
        let pal = ((self.pltt_base & 0x1FFF) * 16 + (info & 0x3FFF) * 4) as usize;
        let raw = |i: usize| read16(tex.palette, pal + i * 2);
        let (word, opaque) = match (mode, value) {
            (_, 0) => (raw(0), true),
            (_, 1) => (raw(1), true),
            (0, 2) | (2, 2) => (raw(2), true),
            (2, 3) => (raw(3), true),
            (1, 2) => (blend555(raw(0), raw(1), 1, 1), true), // (c0+c1)/2
            (3, 2) => (blend555(raw(0), raw(1), 5, 3), true), // (5·c0+3·c1)/8
            (3, 3) => (blend555(raw(0), raw(1), 3, 5), true), // (3·c0+5·c1)/8
            _ => (0, false), // modes 0/1, index 3: transparent
        };
        if !opaque {
            return Texel::default();
        }
        Texel {
            color: [expand6(word) as u8, expand6(word >> 5) as u8, expand6(word >> 10) as u8],
            alpha: 31,
        }
    }

    /// Format 1 — A3I5: 3-bit alpha, 5-bit index into a 32-color palette (16-byte base).
    fn a3i5(&self, tex: &TextureSet, addr: usize) -> Texel {
        let Some(&byte) = tex.image.get(addr) else { return Texel::default() };
        let a3 = byte >> 5;
        Texel {
            color: self.palette_color(tex, 16, (byte & 0x1F) as u32),
            alpha: a3 * 4 + a3 / 2, // 3-bit → 5-bit
        }
    }

    /// Format 6 — A5I3: 5-bit alpha, 3-bit index into an 8-color palette (16-byte base).
    fn a5i3(&self, tex: &TextureSet, addr: usize) -> Texel {
        let Some(&byte) = tex.image.get(addr) else { return Texel::default() };
        Texel {
            color: self.palette_color(tex, 16, (byte & 0x7) as u32),
            alpha: byte >> 3,
        }
    }

    /// Format 2 — 4-color palette (2 bpp, 8-byte palette base).
    fn palette_2bpp(&self, tex: &TextureSet, base: usize, lin: usize, s: i32) -> Texel {
        let Some(&byte) = tex.image.get(base + lin / 4) else { return Texel::default() };
        let index = ((byte >> ((s & 3) * 2)) & 3) as u32;
        self.indexed(tex, 8, index)
    }

    /// Format 3 — 16-color palette (4 bpp, 16-byte palette base).
    fn palette_4bpp(&self, tex: &TextureSet, base: usize, lin: usize, s: i32) -> Texel {
        let Some(&byte) = tex.image.get(base + lin / 2) else { return Texel::default() };
        let index = ((byte >> ((s & 1) * 4)) & 0xF) as u32;
        self.indexed(tex, 16, index)
    }

    /// Format 4 — 256-color palette (8 bpp, 16-byte palette base).
    fn palette_8bpp(&self, tex: &TextureSet, addr: usize) -> Texel {
        let Some(&index) = tex.image.get(addr) else { return Texel::default() };
        self.indexed(tex, 16, index as u32)
    }

    /// A palette-indexed texel: index 0 is transparent when `color0_transparent`,
    /// otherwise the entry is opaque.
    fn indexed(&self, tex: &TextureSet, step: u32, index: u32) -> Texel {
        if index == 0 && self.color0_transparent {
            return Texel::default();
        }
        Texel {
            color: self.palette_color(tex, step, index),
            alpha: 31,
        }
    }

    /// Read palette entry `index` (an `RGB555` halfword) and expand it to 6-bit RGB.
    /// `step` is the palette-base multiplier (8 or 16 bytes per the format).
    fn palette_color(&self, tex: &TextureSet, step: u32, index: u32) -> [u8; 3] {
        let addr = ((self.pltt_base & 0x1FFF) * step + index * 2) as usize;
        let word = read16(tex.palette, addr);
        [
            expand6(word) as u8,
            expand6(word >> 5) as u8,
            expand6(word >> 10) as u8,
        ]
    }
}

/// Format 7 — Direct color: a 16-bit `RGB555` texel with bit 15 as the alpha flag.
fn direct(tex: &TextureSet, addr: usize) -> Texel {
    let word = read16(tex.image, addr);
    Texel {
        color: [
            expand6(word) as u8,
            expand6(word >> 5) as u8,
            expand6(word >> 10) as u8,
        ],
        alpha: if word & 0x8000 != 0 { 31 } else { 0 },
    }
}

/// Blend two `RGB555` colors channel-wise with integer weights `wa:wb` — the 4×4
/// compressed modes' `(c0+c1)/2`, `(5·c0+3·c1)/8`, and `(3·c0+5·c1)/8` — returning a
/// packed `RGB555` word.
fn blend555(a: u32, b: u32, wa: u32, wb: u32) -> u32 {
    let total = wa + wb;
    let ch = |sh: u32| (((a >> sh) & 0x1F) * wa + ((b >> sh) & 0x1F) * wb) / total;
    ch(0) | (ch(5) << 5) | (ch(10) << 10)
}

/// Wrap a texel coordinate to `[0, size)` per the clamp / repeat / flip mode. Sizes
/// are powers of two (`8 << n`); flip mirrors every second repetition.
fn wrap(coord: i32, size: i32, repeat: bool, flip: bool) -> i32 {
    if size <= 0 {
        return 0;
    }
    if !repeat {
        return coord.clamp(0, size - 1);
    }
    let period = coord.rem_euclid(2 * size);
    let (m, second) = if period >= size { (period - size, true) } else { (period, false) };
    if flip && second {
        size - 1 - m
    } else {
        m
    }
}

/// Expand a 5-bit color channel to 6 bits per GBATEK (`0 → 0`, else `c·2+1`).
fn expand6(word: u32) -> i32 {
    let c = (word & 0x1F) as i32;
    if c == 0 {
        0
    } else {
        c * 2 + 1
    }
}

/// Read a little-endian halfword from `mem` at `addr`, or `0` if out of range.
fn read16(mem: &[u8], addr: usize) -> u32 {
    match (mem.get(addr), mem.get(addr + 1)) {
        (Some(&lo), Some(&hi)) => u16::from_le_bytes([lo, hi]) as u32,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_extracts_format_size_and_offset() {
        // offset field 0x100 (× 8 = 0x800), size S = 8<<2 = 32, T = 8<<1 = 16,
        // format 4, repeat S, color-0 transparent.
        let param = 0x100 | (2 << 20) | (1 << 23) | (4 << 26) | (1 << 16) | (1 << 29);
        let p = TexParams::decode(param, 0);
        assert_eq!(p.offset, 0x800);
        assert_eq!(p.size_s, 32);
        assert_eq!(p.size_t, 16);
        assert_eq!(p.format, 4);
        assert!(p.repeat_s && !p.repeat_t);
        assert!(p.color0_transparent);
    }

    #[test]
    fn direct_color_alpha_flag_controls_transparency() {
        let mut image = vec![0u8; 8];
        image[0..2].copy_from_slice(&0xFC1Fu16.to_le_bytes()); // bit15 set, magenta
        let tex = TextureSet { image: &image, palette: &[] };
        let p = TexParams::decode(7 << 26, 0); // format 7, offset 0
        let t = p.sample(&tex, 0, 0);
        assert_eq!(t.alpha, 31);
        assert_eq!(t.color, [63, 0, 63]); // r=31→63, b=31→63

        image[1] &= 0x7F; // clear bit 15
        let tex = TextureSet { image: &image, palette: &[] };
        assert_eq!(p.sample(&tex, 0, 0).alpha, 0);
    }

    #[test]
    fn palette_256_looks_up_and_respects_transparent_color0() {
        let image = vec![0u8, 1u8, 0, 0]; // texel(0,0)=0, texel(1,0)=1
        let mut palette = vec![0u8; 8];
        palette[2..4].copy_from_slice(&0x001Fu16.to_le_bytes()); // entry 1 = red
        let tex = TextureSet { image: &image, palette: &palette };
        // format 4, size 8×8, color-0 transparent, palette base 0.
        let p = TexParams::decode((4 << 26) | (1 << 29), 0);
        assert_eq!(p.sample(&tex, 0, 0).alpha, 0); // index 0 transparent
        let one = p.sample(&tex, 1, 0);
        assert_eq!(one.alpha, 31);
        assert_eq!(one.color, [63, 0, 0]);
    }

    #[test]
    fn compressed_4x4_decodes_block_modes_and_transparency() {
        // An 8×8 texture = a 2×2 grid of 4×4 blocks. Texel data sits at offset 0; each
        // block's 16-bit palette-info is in slot 1 at 0x20000 + texel_addr/2.
        let mut image = vec![0u8; 0x2_0010];
        // Block 0 (texel_addr 0), row 0: texel(0,0)=1, texel(1,0)=2 → 0b0000_1001.
        image[0] = 0b0000_1001;
        // Block 0 info at 0x20000: mode 2 (four opaque colors), palette offset 0.
        image[0x2_0000..0x2_0002].copy_from_slice(&(2u16 << 14).to_le_bytes());
        // Block 1 (texel_addr 4), row 0: texel(4,0)=3; info at 0x20002: mode 0 → index
        // 3 is transparent.
        image[4] = 0b0000_0011;
        image[0x2_0002..0x2_0004].copy_from_slice(&0u16.to_le_bytes());
        // Palette: block 0's c1 = red, c2 = green (bytes 2 and 4).
        let mut palette = vec![0u8; 64];
        palette[2..4].copy_from_slice(&0x001Fu16.to_le_bytes());
        palette[4..6].copy_from_slice(&0x03E0u16.to_le_bytes());
        let tex = TextureSet { image: &image, palette: &palette };
        let p = TexParams::decode(5 << 26, 0); // format 5, offset 0, base 0
        let red = p.sample(&tex, 0, 0); // block 0, texel value 1 → c1
        assert_eq!((red.color, red.alpha), ([63, 0, 0], 31));
        assert_eq!(p.sample(&tex, 1, 0).color, [0, 63, 0]); // texel value 2 → c2
        assert_eq!(p.sample(&tex, 4, 0).alpha, 0); // block 1, value 3, mode 0 → clear
    }

    #[test]
    fn a3i5_splits_alpha_and_index() {
        // byte = alpha 7 (solid) in bits 5-7, index 1 in bits 0-4.
        let image = vec![(7 << 5) | 1u8];
        let mut palette = vec![0u8; 4];
        palette[2..4].copy_from_slice(&0x7C00u16.to_le_bytes()); // entry 1 = blue
        let tex = TextureSet { image: &image, palette: &palette };
        let p = TexParams::decode(1 << 26, 0); // format 1
        let t = p.sample(&tex, 0, 0);
        assert_eq!(t.alpha, 31); // 7*4 + 7/2 = 31
        assert_eq!(t.color, [0, 0, 63]);
    }

    #[test]
    fn clamp_holds_the_edge_texel_outside_the_texture() {
        let image = vec![0u8, 5u8]; // 8×8 palette-8bpp; texel(1,0)=5
        let mut palette = vec![0u8; 12];
        palette[10..12].copy_from_slice(&0x03E0u16.to_le_bytes()); // entry 5 = green
        let tex = TextureSet { image: &image, palette: &palette };
        let p = TexParams::decode(4 << 26, 0); // format 4, clamp (no repeat)
        // s = 1, and s = 99 both clamp within [0,7]; here read s=1 → entry 5.
        assert_eq!(p.sample(&tex, 1, 0).color, [0, 63, 0]);
    }

    #[test]
    fn repeat_wraps_and_flip_mirrors_every_second_tile() {
        let size = 8;
        assert_eq!(wrap(9, size, true, false), 1); // 9 mod 8
        assert_eq!(wrap(9, size, true, true), 6); // second tile flipped: 8-1-1
        assert_eq!(wrap(-1, size, true, false), 7); // rem_euclid wraps negatives
        assert_eq!(wrap(99, size, false, false), 7); // clamp
    }
}
