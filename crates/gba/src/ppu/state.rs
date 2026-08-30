//! Shared rendering data types: the pixel currency, per-scanline scratch, the
//! framebuffer, and the register-latch snapshot.
//!
//! These are the lightweight types carried on the fast rendering path. They hold
//! no provenance — a candidate pixel is just a color, a source layer, and a
//! priority. The heavyweight explanation types live in [`crate::ppu::debug`] and
//! are only built when instrumentation is on.

use super::debug::provenance::WindowRegion;
use super::obj::evaluate::SpriteInstance;
use super::registers::Registers;

/// Visible framebuffer width in pixels.
pub const WIDTH: usize = 240;
/// Visible framebuffer height in scanlines.
pub const HEIGHT: usize = 160;

/// A native GBA color: 15-bit BGR555 packed into a `u16` (bit 15 unused). This is
/// the emulator's canonical color; conversion to a host format happens only at the
/// presentation boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Color15(pub u16);

impl Color15 {
    /// The 5-bit red/green/blue channels.
    pub fn channels(self) -> (u8, u8, u8) {
        let r = (self.0 & 0x1F) as u8;
        let g = ((self.0 >> 5) & 0x1F) as u8;
        let b = ((self.0 >> 10) & 0x1F) as u8;
        (r, g, b)
    }

    /// Expand to 8-bit-per-channel opaque RGBA for host presentation. This is the
    /// only place native color leaves the canonical BGR555 representation.
    pub fn to_rgba8(self) -> [u8; 4] {
        let (r, g, b) = self.channels();
        let expand = |c: u8| (c << 3) | (c >> 2);
        [expand(r), expand(g), expand(b), 0xFF]
    }
}

/// A visible source layer, in the order the compositor considers them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerId {
    Bg0,
    Bg1,
    Bg2,
    Bg3,
    Obj,
    Backdrop,
}

impl LayerId {
    /// The background layer for index `0..=3`.
    pub fn bg(index: usize) -> LayerId {
        match index {
            0 => LayerId::Bg0,
            1 => LayerId::Bg1,
            2 => LayerId::Bg2,
            _ => LayerId::Bg3,
        }
    }

    /// Tie-break rank when two candidates share a priority: lower wins (is drawn
    /// on top). Sprites beat backgrounds; lower-numbered backgrounds beat higher;
    /// the backdrop is always last.
    pub fn tiebreak_rank(self) -> u8 {
        match self {
            LayerId::Obj => 0,
            LayerId::Bg0 => 1,
            LayerId::Bg1 => 2,
            LayerId::Bg2 => 3,
            LayerId::Bg3 => 4,
            LayerId::Backdrop => 5,
        }
    }
}

/// Per-pixel source flags that survive into the compositor (color effects and OBJ
/// window depend on them).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PixelFlags {
    /// An OBJ in the semi-transparent (alpha) mode — forces alpha blending as the
    /// first target regardless of `BLDCNT`. Consumed by OBJ rendering and effects.
    pub semi_transparent_obj: bool,
    /// An OBJ in the object-window mode — contributes to the window mask, not to
    /// visible color. Consumed by OBJ rendering and windowing.
    pub obj_window: bool,
    /// The pixel was produced through the mosaic sampler.
    pub mosaic: bool,
}

/// One resolved source pixel competing for a screen position.
#[derive(Clone, Copy, Debug)]
pub struct CandidatePixel {
    pub color: Color15,
    pub layer: LayerId,
    /// 0..=3 for backgrounds and sprites (from the source's control register); the
    /// backdrop uses 4 so it always loses.
    pub priority: u8,
    pub flags: PixelFlags,
}

impl CandidatePixel {
    /// The always-present fallback candidate: palette entry 0, lowest priority.
    pub fn backdrop(color: Color15) -> CandidatePixel {
        CandidatePixel {
            color,
            layer: LayerId::Backdrop,
            priority: 4,
            flags: PixelFlags::default(),
        }
    }
}

/// One background layer's candidate pixels for the scanline being drawn. `None`
/// marks a transparent pixel, which never becomes a candidate.
#[derive(Clone, Debug)]
pub struct LayerLine {
    pub pixels: [Option<CandidatePixel>; WIDTH],
}

impl Default for LayerLine {
    fn default() -> Self {
        LayerLine {
            pixels: std::array::from_fn(|_| None),
        }
    }
}

impl LayerLine {
    fn clear(&mut self) {
        self.pixels.fill(None);
    }
}

/// The OBJ (sprite) layer's candidate pixels for a scanline, plus the OBJ-window
/// coverage mask that object-window sprites contribute (consumed by windowing).
#[derive(Clone, Debug)]
pub struct ObjLine {
    pub pixels: [Option<CandidatePixel>; WIDTH],
    pub window: [bool; WIDTH],
}

impl Default for ObjLine {
    fn default() -> Self {
        ObjLine {
            pixels: std::array::from_fn(|_| None),
            window: [false; WIDTH],
        }
    }
}

impl ObjLine {
    fn clear(&mut self) {
        self.pixels.fill(None);
        self.window.fill(false);
    }
}

/// Per-pixel visibility of each source layer and of color effects, after window
/// resolution.
#[derive(Clone, Copy, Debug)]
pub struct WindowMask {
    /// Visibility of BG0..BG3.
    pub bg: [bool; 4],
    pub obj: bool,
    pub effects: bool,
}

impl WindowMask {
    /// Everything visible — the state when no window is active.
    pub fn all_visible() -> Self {
        WindowMask {
            bg: [true; 4],
            obj: true,
            effects: true,
        }
    }
}

impl Default for WindowMask {
    fn default() -> Self {
        Self::all_visible()
    }
}

/// The resolved window mask and region for every pixel of a scanline.
#[derive(Clone, Debug)]
pub struct WindowLine {
    pub mask: [WindowMask; WIDTH],
    pub region: [WindowRegion; WIDTH],
}

impl Default for WindowLine {
    fn default() -> Self {
        WindowLine {
            mask: [WindowMask::all_visible(); WIDTH],
            region: [WindowRegion::Outside; WIDTH],
        }
    }
}

/// Per-scanline scratch reused across lines so rendering allocates nothing on the
/// hot path.
#[derive(Clone, Debug, Default)]
pub struct Scratch {
    pub bg: [LayerLine; 4],
    pub obj: ObjLine,
    pub window: WindowLine,
    /// Sprites evaluated as visible on the current scanline (reused; the `Vec`
    /// keeps its capacity across lines).
    pub sprites: Vec<SpriteInstance>,
}

impl Scratch {
    /// Reset every layer line, the OBJ line, and the sprite list for a fresh
    /// scanline. The window line is fully overwritten each scanline, so it needs
    /// no clearing.
    pub fn clear(&mut self) {
        for line in &mut self.bg {
            line.clear();
        }
        self.obj.clear();
        self.sprites.clear();
    }
}

/// The 240×160 output image in canonical BGR555.
#[derive(Clone, Debug)]
pub struct Framebuffer {
    pub pixels: Box<[Color15]>,
}

impl Default for Framebuffer {
    fn default() -> Self {
        Framebuffer {
            pixels: vec![Color15::default(); WIDTH * HEIGHT].into_boxed_slice(),
        }
    }
}

/// The internal affine reference point for one background, in 1/256-pixel fixed
/// point. Distinct from the CPU-visible `BGxX`/`BGxY` registers because hardware
/// advances it per scanline while the frame is drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AffineReference {
    pub x: i32,
    pub y: i32,
}

/// The internal affine reference state for the two affine-capable backgrounds.
#[derive(Clone, Copy, Debug, Default)]
pub struct AffineInternalState {
    pub bg2: AffineReference,
    pub bg3: AffineReference,
}

/// The register values that govern the scanline currently being drawn — a snapshot
/// taken at line start. Kept distinct from the live [`Registers`] because a
/// mid-frame CPU write must not retroactively change a line already latched.
#[derive(Clone, Copy, Debug, Default)]
pub struct LatchedState {
    pub regs: Registers,
    /// The video mode (`DISPCNT` bits 0-2), decoded once.
    pub mode: u8,
}

impl LatchedState {
    /// Snapshot the live register block for a scanline.
    pub fn from_registers(regs: &Registers) -> LatchedState {
        LatchedState {
            regs: *regs,
            mode: (regs.dispcnt & 0x7) as u8,
        }
    }

    /// Whether background `index` is enabled in `DISPCNT` (bits 8-11).
    pub fn bg_enabled(&self, index: usize) -> bool {
        self.regs.dispcnt & (1 << (8 + index)) != 0
    }

    /// Whether the OBJ layer is enabled in `DISPCNT` (bit 12).
    pub fn obj_enabled(&self) -> bool {
        self.regs.dispcnt & (1 << 12) != 0
    }

    /// Whether OBJ tile mapping is one-dimensional (`DISPCNT` bit 6). When clear,
    /// sprite tiles are addressed as a 32×32 two-dimensional grid.
    pub fn obj_one_dim_mapping(&self) -> bool {
        self.regs.dispcnt & (1 << 6) != 0
    }

    /// Whether the OBJ per-scanline processing budget is the larger,
    /// H-Blank-interval-free value (`DISPCNT` bit 5).
    pub fn hblank_interval_free(&self) -> bool {
        self.regs.dispcnt & (1 << 5) != 0
    }

    /// Whether background `index` has mosaic enabled (`BGxCNT` bit 6).
    pub fn bg_mosaic_enabled(&self, index: usize) -> bool {
        self.regs.bgcnt[index] & (1 << 6) != 0
    }

    /// The background mosaic `(horizontal, vertical)` block sizes in pixels
    /// (`MOSAIC` bits 0-3 / 4-7, each stored as size-1).
    pub fn bg_mosaic(&self) -> (usize, usize) {
        let m = self.regs.mosaic;
        (((m & 0xF) + 1) as usize, (((m >> 4) & 0xF) + 1) as usize)
    }

    /// The OBJ mosaic `(horizontal, vertical)` block sizes in pixels (`MOSAIC`
    /// bits 8-11 / 12-15).
    pub fn obj_mosaic(&self) -> (usize, usize) {
        let m = self.regs.mosaic;
        ((((m >> 8) & 0xF) + 1) as usize, (((m >> 12) & 0xF) + 1) as usize)
    }

    /// Whether the display is force-blanked (`DISPCNT` bit 7).
    pub fn forced_blank(&self) -> bool {
        self.regs.dispcnt & (1 << 7) != 0
    }
}
