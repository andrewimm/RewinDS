//! Color effects — alpha blend, brighten, darken — applied after priority
//! resolution.
//!
//! Effects run once per pixel on the resolved top (and, for alpha, second)
//! candidate. The `BLDCNT`-driven effects are gated by `BLDCNT` target selection and
//! by the per-pixel window color-special-effect enable. A semi-transparent OBJ (like
//! the DS 3D and bitmap-OBJ layers) instead forces alpha blending regardless of the
//! `BLDCNT` mode *and* regardless of the window effect-enable, as long as the pixel
//! behind it is a valid second target.

use super::debug::explain::{AppliedEffect, EffectExplanation, EffectMode};
use super::registers::Registers;
use super::state::{CandidatePixel, Color15, LayerId};

/// The outcome of the color-effects stage.
#[derive(Clone, Copy, Debug)]
pub struct EffectResult {
    pub color: Color15,
    pub mode: EffectMode,
    pub applied: AppliedEffect,
}

impl EffectResult {
    fn none(color: Color15) -> Self {
        EffectResult {
            color,
            mode: EffectMode::None,
            applied: AppliedEffect::None,
        }
    }
}

/// The `BLDCNT` target-select bit for a layer (bit 0-5).
fn target_bit(layer: LayerId) -> u16 {
    match layer {
        LayerId::Bg0 => 0,
        LayerId::Bg1 => 1,
        LayerId::Bg2 => 2,
        LayerId::Bg3 => 3,
        LayerId::Obj => 4,
        LayerId::Backdrop => 5,
    }
}

fn is_first_target(regs: &Registers, layer: LayerId) -> bool {
    regs.bldcnt & (1 << target_bit(layer)) != 0
}

fn is_second_target(regs: &Registers, layer: LayerId) -> bool {
    regs.bldcnt & (1 << (8 + target_bit(layer))) != 0
}

/// Alpha-blend two colors: `min(31, first*eva/16 + second*evb/16)` per channel.
fn alpha_blend(first: Color15, second: Color15, eva: u16, evb: u16) -> Color15 {
    let (fr, fg, fb) = first.channels();
    let (sr, sg, sb) = second.channels();
    let mix = |a: u8, b: u8| -> u16 {
        ((a as u16 * eva + b as u16 * evb) / 16).min(31)
    };
    pack(mix(fr, sr), mix(fg, sg), mix(fb, sb))
}

/// Brighten toward white: `c + (31-c)*evy/16` per channel.
fn brighten(color: Color15, evy: u16) -> Color15 {
    let (r, g, b) = color.channels();
    let up = |c: u8| -> u16 { c as u16 + (31 - c as u16) * evy / 16 };
    pack(up(r), up(g), up(b))
}

/// Darken toward black: `c - c*evy/16` per channel.
fn darken(color: Color15, evy: u16) -> Color15 {
    let (r, g, b) = color.channels();
    let down = |c: u8| -> u16 { c as u16 - c as u16 * evy / 16 };
    pack(down(r), down(g), down(b))
}

fn pack(r: u16, g: u16, b: u16) -> Color15 {
    Color15((r & 0x1F) | ((g & 0x1F) << 5) | ((b & 0x1F) << 10))
}

/// Apply color effects to a resolved pixel. `effects_enabled` is the window's
/// per-pixel color-special-effect enable; when clear it suppresses the BLDCNT-driven
/// effects only — the forced per-pixel blends (3D layer, semi-transparent OBJ, DS
/// bitmap OBJ) still apply, matching hardware.
pub fn apply(
    top: CandidatePixel,
    second: CandidatePixel,
    regs: &Registers,
    effects_enabled: bool,
) -> EffectResult {
    // The DS 3D layer (Engine A BG0) blends with the 2D layer behind it using its OWN
    // per-pixel alpha as the coefficient — `EVA = A/2`, `EVB = 16 − A/2` (GBATEK) — not
    // BLDALPHA. It is intrinsic to the 3D layer and not gated by the window's effect
    // enable. Opaque 3D pixels (`A = 31`) are NOT alpha-blended here (they would wash out
    // against an additive BLDALPHA); they fall through and may still be brightened/
    // darkened by EVY below like an ordinary BG0.
    let eva = (regs.bldalpha & 0x1F).min(16);
    let evb = ((regs.bldalpha >> 8) & 0x1F).min(16);
    let evy = (regs.bldy & 0x1F).min(16);

    // The forced, per-pixel blends below (3D layer, semi-transparent OBJ, DS bitmap OBJ)
    // are intrinsic to the source pixel rather than selected by BLDCNT, and so run BEFORE
    // the window's color-effect gate: on hardware they blend even where a window clears the
    // color-special-effect enable bit (that bit gates only the BLDCNT-driven effects at the
    // end). Missing this made FireRed's Pokémon Tower fog — semi-transparent OBJs under a
    // full-screen WIN0 with effects disabled — render opaque and conceal the map.

    // The DS 3D layer (Engine A BG0) blends with the 2D layer behind it using its OWN
    // per-pixel alpha as the coefficient — `EVA = A/2`, `EVB = 16 − A/2` (GBATEK) — not
    // BLDALPHA. Opaque 3D pixels (`A = 31`) are NOT alpha-blended here (they would wash out
    // against an additive BLDALPHA); they fall through and may still be brightened/darkened
    // by EVY below like an ordinary BG0.
    let top_is_3d = top.flags.three_d_alpha.is_some();
    if let Some(a) = top.flags.three_d_alpha {
        if a < 31 && is_second_target(regs, second.layer) {
            let eva3 = (a / 2) as u16;
            return EffectResult {
                color: alpha_blend(top.color, second.color, eva3, 16 - eva3),
                mode: EffectMode::ThreeDBlend,
                applied: AppliedEffect::ThreeDBlend { second: second.layer, alpha: a },
            };
        }
    }

    // A semi-transparent OBJ is always the alpha-blend 1st target (regardless of BLDCNT),
    // blending with the layer behind it provided that layer is a valid second target.
    if top.flags.semi_transparent_obj && is_second_target(regs, second.layer) {
        return EffectResult {
            color: alpha_blend(top.color, second.color, eva, evb),
            mode: EffectMode::Alpha,
            applied: AppliedEffect::Alpha {
                first: top.layer,
                second: second.layer,
                eva: eva as u8,
                evb: evb as u8,
            },
        };
    }

    // A DS direct-color bitmap OBJ blends with the layer behind it using its OWN
    // per-object alpha (OAM attr2 bits 12-15) as the coefficient — EVA = alpha+1,
    // EVB = 15−alpha (the DS's blend formula) — not BLDALPHA. alpha 15 is opaque
    // (EVA=16/EVB=0) and falls through to the plain top color below.
    if let Some(a) = top.flags.obj_alpha {
        if a < 15 && is_second_target(regs, second.layer) {
            let oeva = a as u16 + 1;
            let oevb = 15 - a as u16;
            return EffectResult {
                color: alpha_blend(top.color, second.color, oeva, oevb),
                mode: EffectMode::Alpha,
                applied: AppliedEffect::Alpha {
                    first: top.layer,
                    second: second.layer,
                    eva: oeva as u8,
                    evb: oevb as u8,
                },
            };
        }
    }

    // The window's per-pixel color-special-effect enable gates the BLDCNT-driven effects
    // only (the forced per-pixel blends above already returned). When clear, the plain top
    // color passes through.
    if !effects_enabled {
        return EffectResult::none(top.color);
    }

    match (regs.bldcnt >> 6) & 0x3 {
        // Alpha blend via BLDALPHA: top a first target, second a second target. The 3D
        // layer is excluded — its blend already ran above with its own coefficients (and
        // an opaque 3D pixel must not be additively blended here).
        1 if !top_is_3d
            && is_first_target(regs, top.layer)
            && is_second_target(regs, second.layer) =>
        {
            EffectResult {
                color: alpha_blend(top.color, second.color, eva, evb),
                mode: EffectMode::Alpha,
                applied: AppliedEffect::Alpha {
                    first: top.layer,
                    second: second.layer,
                    eva: eva as u8,
                    evb: evb as u8,
                },
            }
        }
        // Brighten the top, if it is a first target.
        2 if is_first_target(regs, top.layer) => EffectResult {
            color: brighten(top.color, evy),
            mode: EffectMode::Brighten,
            applied: AppliedEffect::Brighten {
                source: top.layer,
                evy: evy as u8,
            },
        },
        // Darken the top, if it is a first target.
        3 if is_first_target(regs, top.layer) => EffectResult {
            color: darken(top.color, evy),
            mode: EffectMode::Darken,
            applied: AppliedEffect::Darken {
                source: top.layer,
                evy: evy as u8,
            },
        },
        _ => EffectResult::none(top.color),
    }
}

impl EffectResult {
    /// The structured explanation of the applied effect.
    pub fn explain(&self) -> EffectExplanation {
        EffectExplanation {
            mode: self.mode,
            applied: self.applied,
            result: self.color,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PixelFlags;

    fn pixel(color: u16, layer: LayerId) -> CandidatePixel {
        CandidatePixel {
            color: Color15(color),
            layer,
            priority: 0,
            flags: PixelFlags::default(),
        }
    }

    #[test]
    fn alpha_blend_mixes_by_coefficients() {
        // eva = evb = 8 (half each): average of the two colors.
        let out = alpha_blend(Color15(0x001F), Color15(0x7C00), 8, 8);
        // red 31*8/16 + 0 = 15; blue 0 + 31*8/16 = 15.
        assert_eq!(out, pack(15, 0, 15));
    }

    #[test]
    fn alpha_blend_clamps_channels() {
        let out = alpha_blend(Color15(0x7FFF), Color15(0x7FFF), 16, 16);
        assert_eq!(out, Color15(0x7FFF)); // 31*1 + 31*1 clamps to 31
    }

    #[test]
    fn brighten_and_darken_move_toward_white_and_black() {
        assert_eq!(brighten(Color15(0), 16), Color15(0x7FFF)); // full brighten -> white
        assert_eq!(darken(Color15(0x7FFF), 16), Color15(0)); // full darken -> black
    }

    #[test]
    fn effects_disabled_by_window_pass_through() {
        let regs = Registers {
            bldcnt: (2 << 6) | 0x04, // brighten, BG2 first target
            bldy: 16,
            ..Default::default()
        };
        let top = pixel(0x0000, LayerId::Bg2);
        let out = apply(top, top, &regs, false);
        assert_eq!(out.color, Color15(0x0000));
        assert!(matches!(out.mode, EffectMode::None));
    }

    #[test]
    fn bitmap_obj_alpha_blends_over_second_target() {
        // A DS bitmap OBJ (obj_alpha = 7) blends with its own coefficient EVA=8/EVB=8 —
        // half OBJ, half the layer behind — regardless of BLDALPHA.
        let regs = Registers {
            bldcnt: 1 << 9, // second target = BG1; effect mode 0 (none)
            ..Default::default()
        };
        let mut obj = pixel(0x001F, LayerId::Obj); // red
        obj.flags.obj_alpha = Some(7);
        let below = pixel(0x7C00, LayerId::Bg1); // blue
        let out = apply(obj, below, &regs, true);
        assert!(matches!(out.mode, EffectMode::Alpha));
        assert_eq!(out.color, pack(15, 0, 15)); // 31*8/16 red + 31*8/16 blue
    }

    #[test]
    fn bitmap_obj_alpha_15_is_opaque() {
        // alpha = 15 → EVA=16/EVB=0: the OBJ passes through unblended.
        let regs = Registers {
            bldcnt: 1 << 9,
            ..Default::default()
        };
        let mut obj = pixel(0x001F, LayerId::Obj);
        obj.flags.obj_alpha = Some(15);
        let below = pixel(0x7C00, LayerId::Bg1);
        let out = apply(obj, below, &regs, true);
        assert_eq!(out.color, Color15(0x001F));
    }

    #[test]
    fn semi_transparent_obj_blends_even_when_window_disables_effects() {
        // FireRed's Pokémon Tower fog: a semi-transparent OBJ over BG2 (a valid 2nd
        // target) under a full-screen WIN0 whose color-effect enable bit is clear. The
        // forced OBJ blend is intrinsic to the pixel, so it must still happen — otherwise
        // the fog renders opaque and conceals the map.
        let regs = Registers {
            bldcnt: 1 << 10, // second target = BG2; effect mode 0
            bldalpha: (8 << 8) | 8, // eva = 8, evb = 8 (half each)
            ..Default::default()
        };
        let mut obj = pixel(0x001F, LayerId::Obj); // red fog
        obj.flags.semi_transparent_obj = true;
        let below = pixel(0x7C00, LayerId::Bg2); // blue map
        let out = apply(obj, below, &regs, /* effects_enabled */ false);
        assert!(matches!(out.mode, EffectMode::Alpha));
        assert_eq!(out.color, pack(15, 0, 15)); // blended, not the opaque red OBJ
    }

    #[test]
    fn semi_transparent_obj_forces_alpha() {
        let regs = Registers {
            bldcnt: 1 << 9, // second-target = BG1; effect mode 0 (none)
            bldalpha: 16, // eva = 16, evb = 0 (high byte)
            ..Default::default()
        };
        let mut obj = pixel(0x001F, LayerId::Obj);
        obj.flags.semi_transparent_obj = true;
        let below = pixel(0x7C00, LayerId::Bg1);
        let out = apply(obj, below, &regs, true);
        // eva=16 evb=0 -> just the OBJ color.
        assert_eq!(out.color, Color15(0x001F));
        assert!(matches!(out.mode, EffectMode::Alpha));
    }
}
