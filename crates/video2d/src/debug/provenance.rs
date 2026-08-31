//! Structured per-source provenance: what exact guest state produced a candidate
//! pixel.
//!
//! Provenance is data, never a string. Every address field is an absolute guest
//! address into palette RAM / VRAM / OAM, so a future memory last-writer bridge
//! can answer "who wrote that address, from what PC" with no change here.

use crate::state::{AffineReference, Color15};

/// A background layer identity (backgrounds only; distinct from the compositor's
/// [`LayerId`](crate::state::LayerId) which also names OBJ/backdrop).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundId {
    Bg0,
    Bg1,
    Bg2,
    Bg3,
}

/// A tile's color depth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BgColorMode {
    /// 16-color: a 4-bit index into one of 16 palette banks.
    Bpp4 { palette_bank: u8 },
    /// 256-color: an 8-bit index into the single background palette.
    Bpp8,
}

/// A bitmap-mode sample before palette resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitmapSample {
    /// A direct 15-bit color read straight from VRAM (modes 3, 5).
    Direct(Color15),
    /// A palette index read from VRAM (mode 4).
    Indexed(u8),
}

/// The affine transform coefficients in effect for a scanline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AffineMatrix {
    pub pa: i16,
    pub pb: i16,
    pub pc: i16,
    pub pd: i16,
}

/// How an affine background treats coordinates outside the map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AffineWrap {
    Wrap,
    Transparent,
}

/// An OBJ's compositing mode (`OBJ` attribute 0 bits 10-11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjMode {
    Normal,
    SemiTransparent,
    ObjWindow,
}

/// An OBJ's color depth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjColorMode {
    Bpp4 { palette_bank: u8 },
    Bpp8,
}

/// The backdrop: palette entry 0, shown where nothing else is opaque.
#[derive(Clone, Copy, Debug)]
pub struct BackdropProvenance {
    pub palette_address: u32,
    pub raw_color: Color15,
}

/// A text (tiled) background pixel's full sampling chain (spec §14).
#[derive(Clone, Copy, Debug)]
pub struct TextBgProvenance {
    pub bg: BackgroundId,
    pub screen_x: u16,
    pub screen_y: u16,
    /// Post-scroll, wrapped background coordinate.
    pub source_x: u16,
    pub source_y: u16,
    pub map_address: u32,
    pub map_entry: u16,
    pub tile_number: u16,
    pub tile_address: u32,
    pub tile_x: u8,
    pub tile_y: u8,
    pub tile_byte_address: u32,
    pub palette_index: u8,
    pub palette_address: u32,
    pub horizontal_flip: bool,
    pub vertical_flip: bool,
    pub color_mode: BgColorMode,
}

/// An affine (rotation/scaling) background pixel's chain.
#[derive(Clone, Copy, Debug)]
pub struct AffineBgProvenance {
    pub bg: BackgroundId,
    pub screen_x: u16,
    pub screen_y: u16,
    pub reference: AffineReference,
    pub matrix: AffineMatrix,
    pub transformed_x: i32,
    pub transformed_y: i32,
    pub wrap: AffineWrap,
    /// Whether the transformed coordinate landed inside the map. `false` means the
    /// pixel became transparent.
    pub in_range: bool,
    pub map_address: u32,
    pub map_entry: u8,
    pub tile_number: u16,
    pub tile_address: u32,
    pub tile_byte_address: u32,
    pub palette_index: u8,
    pub palette_address: u32,
}

/// A bitmap-mode (3/4/5) BG2 pixel's chain.
#[derive(Clone, Copy, Debug)]
pub struct BitmapBgProvenance {
    pub video_mode: u8,
    /// Page/frame selected by `DISPCNT` bit 4 (modes 4/5).
    pub frame: u8,
    pub source_x: u16,
    pub source_y: u16,
    pub vram_address: u32,
    pub raw: BitmapSample,
    /// Present when the sample was a palette index (mode 4).
    pub palette_address: Option<u32>,
    pub resolved: Color15,
    pub priority: u8,
}

/// An OBJ (sprite) pixel's chain.
#[derive(Clone, Copy, Debug)]
pub struct ObjProvenance {
    pub oam_index: u8,
    pub oam_address: u32,
    pub attr0: u16,
    pub attr1: u16,
    pub attr2: u16,
    pub obj_mode: ObjMode,
    pub color_mode: ObjColorMode,
    pub local_x: u16,
    pub local_y: u16,
    pub tile_number: u16,
    pub tile_address: u32,
    pub tile_byte_address: u32,
    pub palette_index: u8,
    pub palette_address: u32,
    pub priority: u8,
    pub hflip: bool,
    pub vflip: bool,
}

/// The provenance of one candidate pixel, tagged by its source kind.
#[derive(Clone, Copy, Debug)]
pub enum SourceProvenance {
    Backdrop(BackdropProvenance),
    TextBg(TextBgProvenance),
    AffineBg(AffineBgProvenance),
    BitmapBg(BitmapBgProvenance),
    Obj(ObjProvenance),
}

impl SourceProvenance {
    /// The exact guest memory addresses this pixel was sampled from — the bridge
    /// to `memory.last_writer(addr)` once write tracking exists.
    pub fn source_addresses(&self) -> Vec<u32> {
        match self {
            SourceProvenance::Backdrop(p) => vec![p.palette_address],
            SourceProvenance::TextBg(p) => {
                vec![p.map_address, p.tile_byte_address, p.palette_address]
            }
            SourceProvenance::AffineBg(p) => {
                vec![p.map_address, p.tile_byte_address, p.palette_address]
            }
            SourceProvenance::BitmapBg(p) => {
                let mut v = vec![p.vram_address];
                v.extend(p.palette_address);
                v
            }
            SourceProvenance::Obj(p) => {
                vec![p.oam_address, p.tile_byte_address, p.palette_address]
            }
        }
    }
}

/// Which window region a screen position resolved to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WindowRegion {
    Win0,
    Win1,
    ObjWindow,
    #[default]
    Outside,
}

/// Why a candidate did not contribute the final pixel (spec §37).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectionReason {
    /// The layer is disabled in `DISPCNT`.
    LayerDisabled,
    /// The source texel was palette index 0 (transparent).
    TransparentPixel { palette_index: u8 },
    /// A window hid this layer at this position.
    WindowHidden { region: WindowRegion },
    /// An affine coordinate fell outside a non-wrapping background.
    OutOfBounds,
    /// A higher-priority candidate won.
    LowerPriority { winner_priority: u8 },
    /// An OBJ does not cover this scanline.
    NotOnScanline,
    /// An OBJ was dropped by a per-scanline hardware limit.
    ObjHardwareLimit,
    /// The display is force-blanked.
    ForcedBlank,
}
