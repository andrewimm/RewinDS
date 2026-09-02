//! The software **scanline** rasterizer: it turns a sealed [`RenderList`] into a
//! 256×192 pixel buffer. Faithful to hardware, it fills a polygon scanline by
//! scanline — for each visible row it finds the polygon's span (left/right edge x)
//! and fills across it — rather than one host triangle at a time.
//!
//! This pass does: **Gouraud shading** (per-pixel color interpolated from the
//! vertices), a **depth test** (Z- or W-buffer per `SWAP_BUFFERS`), **texture
//! sampling** (all palette / translucent / direct formats via [`crate::texture`],
//! blended with the vertex color), and **alpha/translucency** — opaque pixels write
//! first, then translucent pixels blend over them in a second pass, and the surviving
//! per-pixel alpha is exported so the 2D compositor can blend the 3D layer over the
//! backgrounds behind it.
//!
//! Interpolation of color, depth and texcoords is linear in screen space (not yet
//! perspective-correct); fog, edge marking, toon/highlight shading, wireframe and the
//! 4×4 compressed texture format are still to come.

use crate::geometry::{Polygon, RenderList, Vertex};
use crate::texture::{TexParams, TextureSet};

/// The 3D screen dimensions.
pub const WIDTH: usize = 256;
pub const HEIGHT: usize = 192;

/// One rendered 3D pixel: a shaded color, its coverage alpha (5-bit; `31` = solid,
/// `0` = the 3D layer is transparent here), and whether any polygon covered it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pixel3d {
    /// 6-bit-per-channel RGB.
    pub color: [u8; 3],
    /// Coverage/opacity for the composite over 2D (0–31; `31` = fully opaque).
    pub alpha: u8,
    /// Whether a polygon was drawn here (else the 3D layer is transparent).
    pub covered: bool,
}

/// The 3D engine's 256×192 output buffer plus its depth, polygon-ID, and stencil buffers.
pub struct Framebuffer3d {
    pub pixels: Vec<Pixel3d>,
    /// Per-pixel depth; smaller is nearer. Cleared to the far value each frame.
    depth: Vec<i32>,
    /// Per-pixel polygon ID of the winning opaque pixel (`POLYGON_ATTR` bits 24-29), so
    /// shadow polygons can require a *different* ID before darkening a pixel.
    poly_id: Vec<u8>,
    /// Per-pixel stencil bit for the two-pass shadow-volume algorithm (set by the shadow
    /// mask, tested and cleared by the shadow render).
    stencil: Vec<bool>,
}

impl Default for Framebuffer3d {
    fn default() -> Self {
        Framebuffer3d::new()
    }
}

impl Framebuffer3d {
    pub fn new() -> Self {
        Framebuffer3d {
            pixels: vec![Pixel3d::default(); WIDTH * HEIGHT],
            depth: vec![i32::MAX; WIDTH * HEIGHT],
            poly_id: vec![0xFF; WIDTH * HEIGHT],
            stencil: vec![false; WIDTH * HEIGHT],
        }
    }
    pub fn clear(&mut self) {
        self.clear_rear([0, 0, 0], 0, i32::MAX);
    }
    /// Initialise every pixel to the rear-plane fill (the 3D "clear color") and the
    /// depth buffer to the clear depth. A rear-plane alpha of 0 is transparent (the
    /// pixel is uncovered, so the 2D layers show through).
    pub fn clear_rear(&mut self, color: [u8; 3], alpha: u8, depth: i32) {
        let fill = Pixel3d { color, alpha, covered: alpha > 0 };
        self.pixels.iter_mut().for_each(|p| *p = fill);
        self.depth.iter_mut().for_each(|d| *d = depth);
        self.poly_id.iter_mut().for_each(|p| *p = 0xFF);
        self.stencil.iter_mut().for_each(|s| *s = false);
    }
    /// The pixel at `(x, y)` (both in range).
    pub fn at(&self, x: usize, y: usize) -> Pixel3d {
        self.pixels[y * WIDTH + x]
    }
}

/// Per-frame rendering controls from `DISP3DCNT`, `ALPHA_TEST_REF`, and the rear-plane
/// clear registers.
#[derive(Clone, Copy)]
pub struct RenderConfig {
    /// `DISP3DCNT` bit 0: global texture-mapping enable. When clear, polygons render
    /// with their vertex/lighting color only (no texture sampling).
    pub texture_enable: bool,
    /// `DISP3DCNT` bit 3: when clear, translucent polygons are drawn opaque (their
    /// pixels overwrite the framebuffer rather than blending).
    pub alpha_blend: bool,
    /// `DISP3DCNT` bit 2: gate pixels on `alpha > alpha_ref` instead of `alpha > 0`.
    pub alpha_test: bool,
    /// `ALPHA_TEST_REF` (0-31): the alpha-test comparison value.
    pub alpha_ref: u8,
    /// The rear-plane fill (`CLEAR_COLOR`): 6-bit RGB and 5-bit alpha (`0` = the 3D
    /// layer is transparent there, so the 2D layers behind show through).
    pub clear_color: [u8; 3],
    pub clear_alpha: u8,
    /// The rear-plane depth (`CLEAR_DEPTH` expanded to 24 bits); the depth buffer is
    /// initialised to this, so polygons must be nearer to draw.
    pub clear_depth: i32,
    /// Debug: skip the depth test entirely (paint in submission order). Distinguishes
    /// "black because the pixel is depth-rejected" from "black because no polygon covers it".
    pub ignore_depth: bool,
}

impl Default for RenderConfig {
    /// The common case for unit tests: texture + alpha-blending on, no alpha test, and
    /// a transparent far rear-plane. Real frames build this from the registers.
    fn default() -> Self {
        RenderConfig {
            texture_enable: true,
            alpha_blend: true,
            alpha_test: false,
            alpha_ref: 0,
            clear_color: [0, 0, 0],
            clear_alpha: 0,
            clear_depth: i32::MAX,
            ignore_depth: false,
        }
    }
}

impl RenderConfig {
    /// Whether a final pixel alpha passes the alpha test (GBATEK: drawn when `alpha >
    /// ALPHA_TEST_REF` if enabled, else when `alpha > 0`).
    fn alpha_passes(&self, alpha: u8) -> bool {
        alpha > if self.alpha_test { self.alpha_ref } else { 0 }
    }
}

/// The screen rectangle geometry projects into, decoded from the `VIEWPORT` command.
#[derive(Clone, Copy)]
pub struct Viewport {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

impl Viewport {
    pub fn full() -> Self {
        Viewport { x1: 0, y1: 0, x2: 255, y2: 191 }
    }
    /// Decode the `VIEWPORT` parameter (0 means the game never set it → full screen).
    pub fn decode(param: u32) -> Self {
        if param == 0 {
            return Viewport::full();
        }
        Viewport {
            x1: (param & 0xFF) as i32,
            y1: ((param >> 8) & 0xFF) as i32,
            x2: ((param >> 16) & 0xFF) as i32,
            y2: ((param >> 24) & 0xFF) as i32,
        }
    }
}

/// The reciprocal-`w` fixed-point scale for perspective-correct interpolation: each
/// vertex carries `iw = IW_ONE / w`, and its color/texcoord premultiplied by `iw`.
/// Interpolating those linearly in screen space and dividing by the interpolated `iw`
/// per pixel recovers the perspective-correct attribute (textures no longer swim on
/// polygons seen at an angle).
///
/// The scale must be large enough that `iw` keeps precision even when `w` is huge —
/// far geometry like a skybox reaches `w ≈ 2²³`, where `1<<28` left `iw` only ~5 bits
/// (≈8 texels of error, collapsing far textures into flat/repeating garbage). `1<<38`
/// gives ~15 bits there, while staying within `i64` once premultiplied by a 16-bit
/// texcoord (≤2¹⁵) and accumulated across a span (`2¹⁵·2³⁸·2⁸ = 2⁶¹`, comfortably < 2⁶³).
const IW_ONE: i64 = 1 << 38;

/// A vertex projected to integer screen coordinates, carrying the perspective-correct
/// interpolants: `depth` (smaller = nearer, interpolated linearly per the DS depth
/// buffer), the reciprocal `iw`, and 6-bit RGB `color` / 1.11.4 texcoord `st` each
/// premultiplied by `iw`.
#[derive(Clone, Copy)]
struct RVertex {
    x: i32,
    y: i32,
    depth: i32,
    iw: i64,
    cw: [i64; 3],
    stw: [i64; 2],
}

/// Perspective divide + viewport transform: clip `(x, y, w)` → screen pixel. Per GBATEK
/// the DS 3D coordinate origin is the **lower-left** (viewport Y1 = bottom-most, Y2 =
/// top-most), so NDC y runs upward and is flipped into the top-origin framebuffer row
/// (`row = 191 - y_bottom`). `w` is positive by near-plane clipping. Depth is W
/// (W-buffer) or `z/w` mapped to the 24-bit range.
fn project(v: &Vertex, vp: &Viewport, w_buffer: bool) -> RVertex {
    let w = (v.clip[3] as i64).max(1);
    let (x, y, z) = (v.clip[0] as i64, v.clip[1] as i64, v.clip[2] as i64);
    let vp_w = (vp.x2 - vp.x1 + 1) as i64;
    let vp_h = (vp.y2 - vp.y1 + 1) as i64;
    // Depth maps to [0, 0xFFFFFE], one below CLEAR_DEPTH's 0xFFFFFF — so the rear plane is
    // strictly the backmost and far geometry (the sky) always draws over it instead of
    // tying the depth test (which rejected it as flickering black holes). Full precision:
    // an earlier form zeroed the low 9 bits, coarse enough to z-fight co-planar surfaces
    // (the ground popping through a pipe).
    let depth = if w_buffer {
        (w as i32).clamp(0, 0x00FF_FFFE)
    } else {
        // Z-buffer: (z/w + 1)·0x7FFFFF, multiplying before dividing to keep the low bits.
        ((z + w) * 0x7F_FFFF / w).clamp(0, 0x00FF_FFFE) as i32
    };
    // Bottom-origin viewport y (Y1 at the screen bottom), then flip to the top-origin
    // framebuffer row shared with the 2D layers.
    let y_bottom = vp.y1 as i64 + (y + w) * vp_h / (2 * w);
    // Premultiply color/texcoord by 1/w for perspective-correct interpolation.
    let iw = IW_ONE / w;
    RVertex {
        x: (vp.x1 as i64 + (x + w) * vp_w / (2 * w)) as i32,
        y: (HEIGHT as i64 - 1 - y_bottom) as i32,
        depth,
        iw,
        cw: [v.color[0] as i64 * iw, v.color[1] as i64 * iw, v.color[2] as i64 * iw],
        stw: [v.texcoord[0] as i64 * iw, v.texcoord[1] as i64 * iw],
    }
}

/// A projected polygon ready to scan-fill: Gouraud vertices, its y-extent, and the
/// texture/alpha state shared by every pixel of the polygon.
struct RPoly {
    pts: Vec<RVertex>,
    ymin: i32,
    ymax: i32,
    tex: TexParams,
    /// Polygon alpha 1–31 (wireframe `0` is treated as solid for now).
    poly_alpha: u8,
    /// Texture-blend mode (`POLYGON_ATTR` bits 4-5): 0 = modulation, 1 = decal,
    /// 2 = toon/highlight, 3 = shadow.
    blend_mode: u8,
    /// Polygon ID (`POLYGON_ATTR` bits 24-29) — the shadow algorithm's identity check.
    poly_id: u8,
    /// Whether translucent pixels of this polygon update the depth buffer (bit 11).
    trans_depth_write: bool,
    /// Whether any pixel of this polygon can be translucent (skips it in the opaque
    /// pass early when it cannot).
    has_translucency: bool,
}

impl RPoly {
    fn build(p: &Polygon, verts: &[Vertex], vp: &Viewport, w_buffer: bool) -> RPoly {
        let pts: Vec<RVertex> = p.verts[..p.count as usize]
            .iter()
            .map(|&i| project(&verts[i as usize], vp, w_buffer))
            .collect();
        let ymin = pts.iter().map(|v| v.y).min().unwrap();
        let ymax = pts.iter().map(|v| v.y).max().unwrap();
        let tex = TexParams::decode(p.tex_param, p.pltt_base);
        let raw_alpha = ((p.attr >> 16) & 0x1F) as u8;
        let poly_alpha = if raw_alpha == 0 { 31 } else { raw_alpha };
        // A polygon can produce translucent pixels if its own alpha is partial or its
        // texture format carries per-texel alpha (A3I5, A5I3, direct).
        let has_translucency =
            poly_alpha < 31 || matches!(tex.format, 1 | 6 | 7);
        RPoly {
            pts,
            ymin,
            ymax,
            tex,
            poly_alpha,
            blend_mode: ((p.attr >> 4) & 3) as u8,
            poly_id: ((p.attr >> 24) & 0x3F) as u8,
            trans_depth_write: p.attr & (1 << 11) != 0,
            has_translucency,
        }
    }

    /// Representative depth for back-to-front translucent ordering: the centroid depth
    /// (mean of the projected vertices, `larger = farther`). Averaging normalizes across
    /// the 3- and 4-vertex polygons so a nearer polygon always sorts after a farther one.
    fn sort_depth(&self) -> i64 {
        let sum: i64 = self.pts.iter().map(|v| v.depth as i64).sum();
        sum / self.pts.len() as i64
    }
}

/// Where one edge crosses a scanline, with the interpolated attributes there. Color and
/// texcoord are carried in their `·iw` (perspective) form; `iw` recovers them per pixel.
#[derive(Clone, Copy)]
struct Crossing {
    x: i32,
    depth: i32,
    iw: i64,
    cw: [i64; 3],
    stw: [i64; 2],
}

/// Interpolate the crossing of edge `a→b` at scanline `y` (caller guarantees `a.y != b.y`).
fn edge_crossing(a: &RVertex, b: &RVertex, y: i32) -> Crossing {
    // Evaluate every edge from its upper (smaller-y) vertex, so an edge shared by two
    // adjacent polygons yields the *identical* crossing for both. Interpolating in the
    // winding's direction instead lets integer truncation disagree by a pixel between the
    // two tris — leaving a 1-pixel seam where the rear plane shows through.
    let (a, b) = if a.y <= b.y { (a, b) } else { (b, a) };
    let tn = (y - a.y) as i64;
    let td = (b.y - a.y) as i64;
    let lerp = |va: i64, vb: i64| va + (vb - va) * tn / td;
    Crossing {
        x: lerp(a.x as i64, b.x as i64) as i32,
        depth: lerp(a.depth as i64, b.depth as i64) as i32,
        iw: lerp(a.iw, b.iw),
        cw: [lerp(a.cw[0], b.cw[0]), lerp(a.cw[1], b.cw[1]), lerp(a.cw[2], b.cw[2])],
        stw: [lerp(a.stw[0], b.stw[0]), lerp(a.stw[1], b.stw[1])],
    }
}

/// Which pixels a fill pass writes: opaque first, then translucent blended over, then the
/// two shadow-volume passes (mask sets the stencil, render darkens where it is clear).
#[derive(Clone, Copy, PartialEq)]
enum Pass {
    Opaque,
    Translucent,
    ShadowMask,
    ShadowRender,
}

/// Blend a translucent source (color + 5-bit alpha) over the resolved destination pixel,
/// per GBATEK: `out = (src·(A+1) + dst·(31−A)) / 32`, keeping the larger alpha as the
/// composite coverage. When the destination is transparent the source is written as-is,
/// so its alpha carries the coverage into the 2D composite instead of blending to black.
fn blend_over(dst: Pixel3d, rgb: [u8; 3], alpha: u8) -> Pixel3d {
    if !dst.covered {
        return Pixel3d { color: rgb, alpha, covered: true };
    }
    let a = alpha as i32;
    let b = |s: i32, d: i32| ((s * (a + 1) + d * (31 - a)) / 32) as u8;
    Pixel3d {
        color: [
            b(rgb[0] as i32, dst.color[0] as i32),
            b(rgb[1] as i32, dst.color[1] as i32),
            b(rgb[2] as i32, dst.color[2] as i32),
        ],
        alpha: a.max(dst.alpha as i32) as u8,
        covered: true,
    }
}

/// Combine a (possibly textured) polygon's interpolated vertex color with a sampled
/// texel per the blend mode, yielding the final 6-bit color and 5-bit alpha.
fn shade(rp: &RPoly, tex: &TextureSet, cfg: &RenderConfig, vtx: [i32; 3], st: [i32; 2]) -> ([u8; 3], u8) {
    let vtx = [vtx[0].clamp(0, 63), vtx[1].clamp(0, 63), vtx[2].clamp(0, 63)];
    if !cfg.texture_enable || !rp.tex.textured() {
        return ([vtx[0] as u8, vtx[1] as u8, vtx[2] as u8], rp.poly_alpha);
    }
    let texel = rp.tex.sample(tex, st[0] >> 4, st[1] >> 4);
    let at = texel.alpha as i32;
    let color = match rp.blend_mode {
        // Decal: the texel replaces the vertex color weighted by texel alpha.
        1 => {
            let mix = |t: i32, v: i32| -> u8 {
                (if at == 0 {
                    v
                } else if at >= 31 {
                    t
                } else {
                    (t * at + v * (31 - at)) / 31
                }) as u8
            };
            [
                mix(texel.color[0] as i32, vtx[0]),
                mix(texel.color[1] as i32, vtx[1]),
                mix(texel.color[2] as i32, vtx[2]),
            ]
        }
        // Modulation (and, for now, toon/highlight): per-channel `((t+1)*(v+1)-1)/64`.
        _ => {
            let mul = |t: i32, v: i32| (((t + 1) * (v + 1) - 1) / 64) as u8;
            [
                mul(texel.color[0] as i32, vtx[0]),
                mul(texel.color[1] as i32, vtx[1]),
                mul(texel.color[2] as i32, vtx[2]),
            ]
        }
    };
    // Final alpha modulates the texel alpha by the polygon alpha (decal keeps the
    // polygon alpha, since its color already folded the texel alpha in).
    let alpha = if rp.blend_mode == 1 {
        rp.poly_alpha
    } else {
        (((at + 1) * (rp.poly_alpha as i32 + 1) - 1) / 32) as u8
    };
    (color, alpha)
}

/// Fill scanline `y` between the two crossings, shading and depth-testing each pixel.
/// `pass` selects opaque pixels (write + depth) or translucent pixels (blend over the
/// resolved opaque layer). A pixel is treated as opaque when it is fully opaque OR when
/// alpha-blending is globally disabled (`DISP3DCNT` bit 3 = 0).
#[allow(clippy::too_many_arguments)]
fn fill_span(
    fb: &mut Framebuffer3d,
    rp: &RPoly,
    tex: &TextureSet,
    cfg: &RenderConfig,
    y: i32,
    l: &Crossing,
    r: &Crossing,
    pass: Pass,
) {
    // Half-open span [l.x, r.x): the right edge belongs to the polygon on its other
    // side, so a shared edge is covered by exactly one polygon — no gaps (seams) and no
    // overlap (which double-blends a translucent shadow's face edges into dark lines).
    let xl = l.x.max(0);
    let xr = r.x.min(WIDTH as i32);
    if xl >= xr {
        return;
    }
    let span = (r.x - l.x).max(1) as i64;
    let row = y as usize * WIDTH;
    for x in xl..xr {
        let sn = (x - l.x) as i64;
        let lerp = |va: i64, vb: i64| va + (vb - va) * sn / span;
        let idx = row + x as usize;
        let depth = lerp(l.depth as i64, r.depth as i64) as i32;
        if !cfg.ignore_depth && depth >= fb.depth[idx] {
            continue; // occluded by a nearer (or equal) pixel already drawn
        }
        // Shadow mask (step 1): the back-face volume is nearer than the scene here, so
        // flag the stencil. Writes no color and no depth — needs no shading.
        if pass == Pass::ShadowMask {
            fb.stencil[idx] = true;
            continue;
        }
        // Recover perspective-correct attributes: divide each ·iw interpolant by the
        // interpolated iw at this pixel.
        let iw = lerp(l.iw, r.iw).max(1);
        let recover = |a: i64, b: i64| (lerp(a, b) / iw) as i32;
        let color = [recover(l.cw[0], r.cw[0]), recover(l.cw[1], r.cw[1]), recover(l.cw[2], r.cw[2])];
        let st = [recover(l.stw[0], r.stw[0]), recover(l.stw[1], r.stw[1])];
        let (rgb, alpha) = shade(rp, tex, cfg, color, st);
        if !cfg.alpha_passes(alpha) {
            continue; // failed the alpha test (or fully transparent)
        }
        let opaque_pixel = alpha == 31 || !cfg.alpha_blend;
        match pass {
            Pass::Opaque if opaque_pixel => {
                // Overwrite with the polygon's color and alpha (per GBATEK, a
                // blend-disabled translucent pixel keeps its own alpha in the buffer).
                fb.pixels[idx] = Pixel3d { color: rgb, alpha, covered: true };
                fb.depth[idx] = depth;
                fb.poly_id[idx] = rp.poly_id;
            }
            Pass::Translucent if !opaque_pixel => {
                fb.pixels[idx] = blend_over(fb.pixels[idx], rgb, alpha);
                if rp.trans_depth_write {
                    fb.depth[idx] = depth;
                }
            }
            // Shadow render (step 2): the front-face volume is nearer than the scene here;
            // darken the pixel only where the mask left the stencil clear and the covered
            // polygon has a different ID (never shadow a surface onto itself). The stencil
            // is reset after this test, per GBATEK.
            Pass::ShadowRender => {
                if !fb.stencil[idx] && fb.poly_id[idx] != rp.poly_id {
                    fb.pixels[idx] = blend_over(fb.pixels[idx], rgb, alpha);
                }
                fb.stencil[idx] = false;
            }
            _ => {}
        }
    }
}

/// Scan-fill one projected polygon's spans on scanline `y` for the given pass.
fn scan_poly(fb: &mut Framebuffer3d, rp: &RPoly, tex: &TextureSet, cfg: &RenderConfig, y: i32, pass: Pass) {
    if y < rp.ymin || y > rp.ymax {
        return;
    }
    // A convex polygon crosses the scanline at exactly two edges (each vertex counted
    // once via the half-open y test); the span runs between them.
    let mut crossings = [Crossing { x: 0, depth: 0, iw: 1, cw: [0; 3], stw: [0; 2] }; 2];
    let mut count = 0;
    let n = rp.pts.len();
    for i in 0..n {
        let a = &rp.pts[i];
        let b = &rp.pts[(i + 1) % n];
        if ((a.y <= y && y < b.y) || (b.y <= y && y < a.y)) && count < 2 {
            crossings[count] = edge_crossing(a, b, y);
            count += 1;
        }
    }
    if count < 2 {
        return;
    }
    let (l, r) = if crossings[0].x <= crossings[1].x {
        (crossings[0], crossings[1])
    } else {
        (crossings[1], crossings[0])
    };
    fill_span(fb, rp, tex, cfg, y, &l, &r, pass);
}

/// Rasterize the sealed render list into `fb`, sampling from `tex` under `cfg`. Opaque
/// pixels are laid down first (with depth), then — if alpha-blending is enabled —
/// translucent pixels blend over them.
pub fn render(list: &RenderList, tex: &TextureSet, cfg: &RenderConfig, fb: &mut Framebuffer3d) {
    fb.clear_rear(cfg.clear_color, cfg.clear_alpha, cfg.clear_depth);
    let vp = Viewport::decode(list.viewport);
    let w_buffer = list.w_buffer();
    let verts = list.vertices();

    let polys: Vec<RPoly> = list
        .polygons()
        .iter()
        .filter(|p| p.count >= 3)
        .map(|p| RPoly::build(p, verts, &vp, w_buffer))
        .collect();

    // Pass 1: opaque pixels of every non-shadow polygon (depth-tested, write depth and
    // polygon ID). When alpha-blending is disabled, every passing pixel is drawn opaque.
    for y in 0..HEIGHT as i32 {
        for rp in &polys {
            if rp.blend_mode != 3 {
                scan_poly(fb, rp, tex, cfg, y, Pass::Opaque);
            }
        }
    }
    // Pass 2: translucent pixels blend over the resolved opaque layer. Per GBATEK the
    // opaque polygons are always drawn first (Pass 1) and the translucent ones last;
    // SWAP_BUFFERS bit 0 then selects the order *among* the translucent polygons.
    //
    // Manual-sort (bit 0 = 1) keeps submission order — which shadow volumes require, since
    // their ID-0 mask must be drawn before the ID≠0 render volume. Auto-sort (bit 0 = 0)
    // draws them back-to-front by depth so a nearer translucent surface blends over a
    // farther one instead of leaving whichever was submitted last on top. GBATEK documents
    // that a sort happens but not its key; back-to-front is the order that composites
    // correctly. Shadow polygons (blend mode 3) run their two-pass stencil algorithm in
    // this order. All of Pass 2 needs alpha-blending; it is skipped when disabled.
    if cfg.alpha_blend {
        let mut order: Vec<usize> = (0..polys.len())
            .filter(|&i| polys[i].blend_mode == 3 || polys[i].has_translucency)
            .collect();
        if !list.manual_sort() {
            // Stable so equal-depth polygons keep submission order; Reverse → farthest first.
            order.sort_by_key(|&i| std::cmp::Reverse(polys[i].sort_depth()));
        }
        for y in 0..HEIGHT as i32 {
            for &i in &order {
                let rp = &polys[i];
                match (rp.blend_mode == 3, rp.poly_id) {
                    (true, 0) => scan_poly(fb, rp, tex, cfg, y, Pass::ShadowMask),
                    (true, _) => scan_poly(fb, rp, tex, cfg, y, Pass::ShadowRender),
                    (false, _) => scan_poly(fb, rp, tex, cfg, y, Pass::Translucent),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::op;
    use crate::geometry::GeometryEngine;
    use crate::matrix::ONE;

    fn e8(n: i32) -> i32 {
        n * ONE / 8
    }

    /// Rasterize a render list with no textures bound (default config: blend on).
    fn rasterize(e: &GeometryEngine) -> Framebuffer3d {
        rasterize_cfg(e, &RenderConfig::default())
    }

    /// Rasterize with an explicit render config.
    fn rasterize_cfg(e: &GeometryEngine, cfg: &RenderConfig) -> Framebuffer3d {
        let mut fb = Framebuffer3d::new();
        render(e.render_list(), &TextureSet::default(), cfg, &mut fb);
        fb
    }

    /// Submit one flat-colored triangle rendering both faces, at the given eighths
    /// coords, with `attr` OR-ed into POLYGON_ATTR (for alpha bits).
    fn one_triangle(color: u32, attr: u32) -> GeometryEngine {
        let mut e = GeometryEngine::new();
        e.execute(op::MTX_MODE, &[1]);
        e.execute(op::COLOR, &[color]);
        e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7) | attr]);
        e.execute(op::BEGIN_VTXS, &[0]);
        for (x, y) in [(-4, 4), (4, 4), (0, -4)] {
            let lo = (e8(x) as u32 & 0xFFFF) | ((e8(y) as u32 & 0xFFFF) << 16);
            e.execute(op::VTX_16, &[lo, 0]);
        }
        e.execute(op::SWAP_BUFFERS, &[0]);
        e
    }

    /// Build a render list from a single flat-colored triangle at the given eighths
    /// coordinates (identity clip → NDC = object coords), then rasterize it.
    fn render_triangle(color: u32, tri: [(i32, i32); 3]) -> Framebuffer3d {
        let mut e = GeometryEngine::new();
        e.execute(op::MTX_MODE, &[1]); // position mode, identity clip
        e.execute(op::COLOR, &[color]);
        e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7)]); // render both faces
        e.execute(op::BEGIN_VTXS, &[0]);
        for (x, y) in tri {
            let lo = (e8(x) as u32 & 0xFFFF) | ((e8(y) as u32 & 0xFFFF) << 16);
            e.execute(op::VTX_16, &[lo, 0]);
        }
        e.execute(op::SWAP_BUFFERS, &[0]);
        rasterize(&e)
    }

    #[test]
    fn single_triangle_fills_its_interior_and_nothing_else() {
        // NDC (-0.5, 0.5), (0.5, 0.5), (0, -0.5) → screen (64,48),(192,48),(128,144).
        let fb = render_triangle(0x1F, [(-4, 4), (4, 4), (0, -4)]);
        assert!(fb.at(128, 96).covered, "the centroid is inside");
        assert_eq!(fb.at(128, 96).color, [63, 0, 0]); // flat red (6-bit)
        assert_eq!(fb.at(128, 96).alpha, 31, "opaque");
        assert!(!fb.at(0, 0).covered, "top-left corner is outside");
        assert!(!fb.at(0, 96).covered, "left edge is outside the span");
        assert!(!fb.at(128, 10).covered, "above the triangle's top edge");
    }

    #[test]
    fn far_plane_polygon_draws_over_the_rear_plane() {
        // A polygon at the far plane (z/w = 1, like a skybox) must draw over the rear
        // plane. The old Z formula mapped it to 0x1000000, past CLEAR_DEPTH's 0xFFFFFF,
        // so the depth test rejected it — far polygons flickered as black holes.
        let mut e = GeometryEngine::new();
        e.execute(op::MTX_MODE, &[1]); // position mode, identity clip
        e.execute(op::COLOR, &[0x7FFF]); // white
        e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7)]);
        e.execute(op::BEGIN_VTXS, &[0]);
        for (x, y) in [(-4, 4), (4, 4), (0, -4)] {
            let lo = (e8(x) as u32 & 0xFFFF) | ((e8(y) as u32 & 0xFFFF) << 16);
            e.execute(op::VTX_16, &[lo, ONE as u32 & 0xFFFF]); // z = 1.0 → the far plane
        }
        e.execute(op::SWAP_BUFFERS, &[0]);
        let cfg = RenderConfig { clear_depth: 0x00FF_FFFF, ..RenderConfig::default() };
        let fb = rasterize_cfg(&e, &cfg);
        assert!(fb.at(128, 96).covered, "far-plane polygon rejected by the depth test");
        assert_eq!(fb.at(128, 96).color, [63, 63, 63]);
    }

    #[test]
    fn perspective_correct_interpolation_biases_toward_the_near_vertex() {
        // A quad receding in depth: its left edge is near (w = 2, bright red) and its
        // right edge far (w = 6, black). A projection with clip.w = z makes the divisor
        // vary across the span, so perspective-correct interpolation keeps the near-bright
        // side dominant well past the geometric midpoint — where a linear (w-agnostic)
        // average would read ~half red. This test fails under the old linear interpolation.
        let mut e = GeometryEngine::new();
        e.execute(op::MTX_MODE, &[0]); // projection
        let o = ONE as u32;
        // Column-major map (x, y, z, 1) -> (x, y, z, z): w = z.
        e.execute(op::MTX_LOAD_4X4, &[o, 0, 0, 0, 0, o, 0, 0, 0, 0, o, o, 0, 0, 0, 0]);
        e.execute(op::MTX_MODE, &[1]); // position (identity)
        e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7)]); // both faces
        e.execute(op::BEGIN_VTXS, &[1]); // quad
        let mut vtx = |x: i32, y: i32, z: i32, r: u32| {
            e.execute(op::COLOR, &[r]);
            let lo = (x as u32 & 0xFFFF) | ((y as u32 & 0xFFFF) << 16);
            e.execute(op::VTX_16, &[lo, z as u32 & 0xFFFF]);
        };
        vtx(-ONE, ONE, 2 * ONE, 0x1F); // left edge: near (w=2), red
        vtx(-ONE, -ONE, 2 * ONE, 0x1F);
        vtx(ONE, -ONE, 6 * ONE, 0x00); // right edge: far (w=6), black
        vtx(ONE, ONE, 6 * ONE, 0x00);
        e.execute(op::SWAP_BUFFERS, &[0]);
        let fb = rasterize(&e);
        // Widest covered row, then its middle covered pixel.
        let mut best: Option<(usize, usize, usize)> = None;
        for y in 0..HEIGHT {
            let xs: Vec<usize> = (0..WIDTH).filter(|&x| fb.at(x, y).covered).collect();
            if let (Some(&xl), Some(&xr)) = (xs.first(), xs.last()) {
                if best.is_none_or(|(_, bl, br)| xr - xl > br - bl) {
                    best = Some((y, xl, xr));
                }
            }
        }
        let (y, xl, xr) = best.expect("the quad covered some pixels");
        let mid = fb.at((xl + xr) / 2, y);
        assert!(mid.color[0] > 40, "screen-midpoint red {} should be near-biased (linear≈31)", mid.color[0]);
    }

    #[test]
    fn shadow_volume_darkens_only_where_the_mask_left_the_stencil_clear() {
        // Opaque white ground (ID 1) fills the view. A shadow MASK (mode 3, ID 0) covers
        // the left half and, being nearer, flags the stencil there. A shadow RENDER
        // (mode 3, ID 2, translucent black) covers the whole view but darkens only where
        // the stencil stayed clear (the right half) and the ID differs. So the left half
        // stays white and the right half is darkened — not a solid black volume.
        let mut e = GeometryEngine::new();
        e.execute(op::MTX_MODE, &[1]); // position mode, identity clip
        let mut quad = |corners: [(i32, i32); 4], z: i32, color: u32, attr: u32| {
            e.execute(op::COLOR, &[color]);
            e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7) | attr]); // both faces
            e.execute(op::BEGIN_VTXS, &[1]); // quad
            for (x, y) in corners {
                let lo = (e8(x) as u32 & 0xFFFF) | ((e8(y) as u32 & 0xFFFF) << 16);
                e.execute(op::VTX_16, &[lo, e8(z) as u32 & 0xFFFF]);
            }
        };
        let full = [(-7, 7), (-7, -7), (7, -7), (7, 7)];
        let left = [(-7, 7), (-7, -7), (0, -7), (0, 7)];
        quad(full, 4, 0x7FFF, 1 << 24); // ground: white, far, poly ID 1
        quad(left, -4, 0x0000, (3 << 4) | (10 << 16)); // mask: mode 3, ID 0, near
        quad(full, -4, 0x0000, (3 << 4) | (10 << 16) | (2 << 24)); // render: mode 3, ID 2
        e.execute(op::SWAP_BUFFERS, &[0]);
        let fb = rasterize(&e);
        assert_eq!(fb.at(64, 96).color[0], 63, "left half masked → stays white");
        let right = fb.at(192, 96);
        assert!(right.covered && right.color[0] < 55, "right half shadowed → darkened ({})", right.color[0]);
    }

    #[test]
    fn adjacent_translucent_polys_do_not_double_blend_at_the_shared_edge() {
        // Two translucent quads meeting at an edge must blend the seam once, not twice —
        // otherwise the shared edge darkens into a visible line (the shadow-face-edge
        // artifact). The half-open fill rule covers the shared column with one polygon.
        let mut e = GeometryEngine::new();
        e.execute(op::MTX_MODE, &[1]); // position mode, identity clip
        let mut quad = |corners: [(i32, i32); 4], z: i32, color: u32, attr: u32| {
            e.execute(op::COLOR, &[color]);
            e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7) | attr]);
            e.execute(op::BEGIN_VTXS, &[1]);
            for (x, y) in corners {
                let lo = (e8(x) as u32 & 0xFFFF) | ((e8(y) as u32 & 0xFFFF) << 16);
                e.execute(op::VTX_16, &[lo, e8(z) as u32 & 0xFFFF]);
            }
        };
        quad([(-7, 7), (-7, -7), (7, -7), (7, 7)], 4, 0x7FFF, 0); // opaque white, far
        let alpha = 16 << 16; // translucent
        quad([(-7, 7), (-7, -7), (0, -7), (0, 7)], -4, 0x0000, alpha); // left, near
        quad([(0, 7), (0, -7), (7, -7), (7, 7)], -4, 0x0000, alpha); // right, near
        e.execute(op::SWAP_BUFFERS, &[0]);
        let fb = rasterize(&e);
        let interior = fb.at(64, 96).color[0]; // one black-over-white blend
        let seam = fb.at(128, 96).color[0]; // the shared edge, world x = 0
        assert_eq!(seam, interior, "seam double-blended ({seam}) vs interior ({interior})");
    }

    #[test]
    fn translucent_polys_blend_back_to_front_under_auto_sort() {
        // Two translucent quads fully overlap over an opaque white background, submitted
        // near-first. Submission-order blending leaves the FAR quad on top; auto-sort
        // (SWAP_BUFFERS bit 0 = 0) draws them back-to-front so the NEAR quad blends last
        // and dominates. Manual-sort (bit 0 = 1) keeps submission order — the opposite.
        let scene = |swap: u32| -> Framebuffer3d {
            let mut e = GeometryEngine::new();
            e.execute(op::MTX_MODE, &[1]); // position mode, identity clip
            let mut quad = |z: i32, color: u32, attr: u32| {
                e.execute(op::COLOR, &[color]);
                e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7) | attr]);
                e.execute(op::BEGIN_VTXS, &[1]);
                for (x, y) in [(-7, 7), (-7, -7), (7, -7), (7, 7)] {
                    let lo = (e8(x) as u32 & 0xFFFF) | ((e8(y) as u32 & 0xFFFF) << 16);
                    e.execute(op::VTX_16, &[lo, e8(z) as u32 & 0xFFFF]);
                }
            };
            let alpha = 16 << 16; // translucent
            quad(7, 0x7FFF, 0); // opaque white background, far
            quad(-4, 0x001F, alpha); // near, red, submitted first
            quad(2, 0x7C00, alpha); // farther translucent, blue, submitted second
            e.execute(op::SWAP_BUFFERS, &[swap]);
            rasterize(&e)
        };
        let auto = scene(0).at(128, 96).color;
        let manual = scene(1).at(128, 96).color;
        assert!(auto[0] > auto[2], "auto-sort: near red on top (R {} > B {})", auto[0], auto[2]);
        assert!(manual[2] > manual[0], "manual-sort: far blue on top (B {} > R {})", manual[2], manual[0]);
    }

    #[test]
    fn shared_edges_rasterize_without_seams() {
        // A shared edge must yield the identical crossing regardless of winding direction,
        // or adjacent triangles leave a 1-pixel seam. (0,0)→(10,3) is a slope where naive
        // truncation disagrees by a pixel depending on which end you interpolate from.
        let mk = |x: i32, y: i32| RVertex { x, y, depth: 0, iw: 1 << 20, cw: [0; 3], stw: [0; 2] };
        let (a, b) = (mk(0, 0), mk(10, 3));
        for y in 1..=2 {
            assert_eq!(edge_crossing(&a, &b, y).x, edge_crossing(&b, &a, y).x, "seam at y={y}");
        }
    }

    #[test]
    fn uniform_color_triangle_renders_that_color() {
        let fb = render_triangle(0x03E0, [(-4, 4), (4, 4), (0, -4)]); // green
        assert_eq!(fb.at(128, 96).color, [0, 63, 0]);
    }

    #[test]
    fn gouraud_interpolates_the_vertex_colors() {
        // Red / green / blue vertices → the interior blends all three channels.
        let mut e = GeometryEngine::new();
        e.execute(op::MTX_MODE, &[1]);
        e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7)]);
        e.execute(op::BEGIN_VTXS, &[0]);
        let tri = [(0x1Fu32, (-4, 4)), (0x03E0, (4, 4)), (0x7C00, (0, -4))];
        for (color, (x, y)) in tri {
            e.execute(op::COLOR, &[color]);
            let lo = (e8(x) as u32 & 0xFFFF) | ((e8(y) as u32 & 0xFFFF) << 16);
            e.execute(op::VTX_16, &[lo, 0]);
        }
        e.execute(op::SWAP_BUFFERS, &[0]);
        let c = rasterize(&e).at(128, 80).color; // near the centroid
        assert!(
            c[0] > 3 && c[1] > 3 && c[2] > 3 && c.iter().all(|&v| v < 63),
            "all channels blended, none saturated: {c:?}"
        );
    }

    #[test]
    fn depth_test_keeps_the_nearer_polygon_regardless_of_order() {
        // Two overlapping triangles at the same screen position: red is near (small z),
        // blue is far (large z) and is submitted LAST. The near red must still win.
        let tri = [(-4, 4), (4, 4), (0, -4)];
        let mut e = GeometryEngine::new();
        e.execute(op::MTX_MODE, &[1]);
        e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7)]);
        for &(color, z) in &[(0x1Fu32, -4i32), (0x7C00, 4)] {
            // near red (z=-0.5), then far blue (z=+0.5)
            e.execute(op::COLOR, &[color]);
            e.execute(op::BEGIN_VTXS, &[0]);
            for (x, y) in tri {
                let lo = (e8(x) as u32 & 0xFFFF) | ((e8(y) as u32 & 0xFFFF) << 16);
                e.execute(op::VTX_16, &[lo, e8(z) as u32 & 0xFFFF]);
            }
        }
        e.execute(op::SWAP_BUFFERS, &[0]);
        assert_eq!(rasterize(&e).at(128, 96).color, [63, 0, 0], "the nearer (red) triangle wins");
    }

    #[test]
    fn empty_render_list_produces_a_transparent_frame() {
        let fb = Framebuffer3d::new();
        assert!(fb.pixels.iter().all(|p| !p.covered));
    }

    /// Open a two-triangle scene: a near translucent quad over a far opaque one, both
    /// covering the centroid — asserting the blend and the exported coverage alpha.
    #[test]
    fn translucent_polygon_blends_over_the_opaque_layer() {
        let tri = [(-4, 4), (4, 4), (0, -4)];
        let mut e = GeometryEngine::new();
        e.execute(op::MTX_MODE, &[1]);
        // Far opaque blue (alpha 31), submitted first.
        e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7) | (31 << 16)]);
        e.execute(op::COLOR, &[0x7C00]); // blue
        e.execute(op::BEGIN_VTXS, &[0]);
        for (x, y) in tri {
            let lo = (e8(x) as u32 & 0xFFFF) | ((e8(y) as u32 & 0xFFFF) << 16);
            e.execute(op::VTX_16, &[lo, e8(4) as u32 & 0xFFFF]); // z = +0.5 (far)
        }
        // Near translucent red (alpha 15), submitted second.
        e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7) | (15 << 16)]);
        e.execute(op::COLOR, &[0x1F]); // red
        e.execute(op::BEGIN_VTXS, &[0]);
        for (x, y) in tri {
            let lo = (e8(x) as u32 & 0xFFFF) | ((e8(y) as u32 & 0xFFFF) << 16);
            e.execute(op::VTX_16, &[lo, e8(-4) as u32 & 0xFFFF]); // z = -0.5 (near)
        }
        e.execute(op::SWAP_BUFFERS, &[0]);
        let p = rasterize(&e).at(128, 96);
        // Blend of red over blue: both red and blue channels present, neither zero.
        assert!(p.color[0] > 0 && p.color[2] > 0, "red blended over blue: {:?}", p.color);
        assert_eq!(p.alpha, 31, "opaque behind → the composite is opaque over 2D");
    }

    #[test]
    fn translucent_over_transparent_backdrop_exports_its_alpha() {
        let mut e = GeometryEngine::new();
        e.execute(op::MTX_MODE, &[1]);
        e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7) | (16 << 16)]); // alpha 16
        e.execute(op::COLOR, &[0x1F]);
        e.execute(op::BEGIN_VTXS, &[0]);
        for (x, y) in [(-4, 4), (4, 4), (0, -4)] {
            let lo = (e8(x) as u32 & 0xFFFF) | ((e8(y) as u32 & 0xFFFF) << 16);
            e.execute(op::VTX_16, &[lo, 0]);
        }
        e.execute(op::SWAP_BUFFERS, &[0]);
        let p = rasterize(&e).at(128, 96);
        assert!(p.covered);
        assert_eq!(p.alpha, 16, "the polygon alpha carries into the 2D composite");
        assert_eq!(p.color, [63, 0, 0], "source color, not pre-blended with black");
    }

    #[test]
    fn alpha_test_hides_pixels_at_or_below_the_reference() {
        // A polygon with alpha 16. With ref 16, `alpha > ref` is false → hidden;
        // with ref 15 it draws; ref 31 hides everything.
        let e = one_triangle(0x1F, 16 << 16);
        let ref16 = RenderConfig { alpha_test: true, alpha_ref: 16, ..Default::default() };
        assert!(!rasterize_cfg(&e, &ref16).at(128, 96).covered, "alpha 16 not > ref 16");
        let ref15 = RenderConfig { alpha_test: true, alpha_ref: 15, ..Default::default() };
        assert!(rasterize_cfg(&e, &ref15).at(128, 96).covered, "alpha 16 > ref 15");
        // Opaque polygon, ref 31 → hidden (GBATEK: 1Fh hides all polygons).
        let opaque = one_triangle(0x1F, 31 << 16);
        let ref31 = RenderConfig { alpha_test: true, alpha_ref: 31, ..Default::default() };
        assert!(!rasterize_cfg(&opaque, &ref31).at(128, 96).covered, "even opaque hidden at ref 31");
    }

    #[test]
    fn alpha_blend_disabled_draws_translucent_polygons_opaque() {
        // Near translucent red (alpha 8) over far opaque blue. With blending disabled,
        // the near polygon overwrites (no blend) — the centroid is pure red.
        let tri_near = [(-4, 4), (4, 4), (0, -4)];
        let mut e = GeometryEngine::new();
        e.execute(op::MTX_MODE, &[1]);
        e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7) | (31 << 16)]);
        e.execute(op::COLOR, &[0x7C00]); // far blue, opaque
        e.execute(op::BEGIN_VTXS, &[0]);
        for (x, y) in tri_near {
            let lo = (e8(x) as u32 & 0xFFFF) | ((e8(y) as u32 & 0xFFFF) << 16);
            e.execute(op::VTX_16, &[lo, e8(4) as u32 & 0xFFFF]);
        }
        e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7) | (8 << 16)]);
        e.execute(op::COLOR, &[0x1F]); // near red, alpha 8
        e.execute(op::BEGIN_VTXS, &[0]);
        for (x, y) in tri_near {
            let lo = (e8(x) as u32 & 0xFFFF) | ((e8(y) as u32 & 0xFFFF) << 16);
            e.execute(op::VTX_16, &[lo, e8(-4) as u32 & 0xFFFF]);
        }
        e.execute(op::SWAP_BUFFERS, &[0]);
        let no_blend = RenderConfig { alpha_blend: false, ..Default::default() };
        let p = rasterize_cfg(&e, &no_blend).at(128, 96);
        assert_eq!(p.color, [63, 0, 0], "translucent poly drawn opaque, overwriting blue");
        assert_eq!(p.alpha, 8, "its own alpha is still written to the buffer");
    }

    #[test]
    fn global_texture_disable_uses_the_vertex_color() {
        // A textured (format 4) polygon over an empty texture VRAM. With texturing on it
        // samples transparent → nothing drawn; with texturing off it shows vertex color.
        let mut e = GeometryEngine::new();
        e.execute(op::MTX_MODE, &[1]);
        e.execute(op::COLOR, &[0x1F]); // red
        e.execute(op::TEXIMAGE_PARAM, &[4 << 26]); // format 4
        e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7) | (31 << 16)]);
        e.execute(op::BEGIN_VTXS, &[0]);
        for (x, y) in [(-4, 4), (4, 4), (0, -4)] {
            let lo = (e8(x) as u32 & 0xFFFF) | ((e8(y) as u32 & 0xFFFF) << 16);
            e.execute(op::VTX_16, &[lo, 0]);
        }
        e.execute(op::SWAP_BUFFERS, &[0]);
        assert!(!rasterize(&e).at(128, 96).covered, "textured over empty VRAM → transparent");
        let no_tex = RenderConfig { texture_enable: false, ..Default::default() };
        assert_eq!(rasterize_cfg(&e, &no_tex).at(128, 96).color, [63, 0, 0], "vertex color");
    }

    #[test]
    fn rear_plane_fills_the_uncovered_background() {
        // An opaque red triangle over an opaque blue rear-plane: inside is red, and the
        // corners (outside the triangle) are the blue rear-plane, covered and opaque.
        let e = one_triangle(0x1F, 31 << 16);
        let cfg = RenderConfig { clear_color: [0, 0, 63], clear_alpha: 31, ..Default::default() };
        let fb = rasterize_cfg(&e, &cfg);
        assert_eq!(fb.at(128, 96).color, [63, 0, 0], "the triangle draws over the rear-plane");
        let corner = fb.at(0, 0);
        assert!(corner.covered && corner.alpha == 31, "rear-plane covers the background");
        assert_eq!(corner.color, [0, 0, 63], "the rear-plane clear color");
    }

    #[test]
    fn transparent_rear_plane_leaves_the_background_uncovered() {
        // Clear alpha 0 (the default): pixels the polygons miss stay transparent so the
        // 2D layers show through — the behavior NSMB relies on.
        let fb = render_triangle(0x1F, [(-4, 4), (4, 4), (0, -4)]);
        assert!(!fb.at(0, 0).covered, "transparent rear-plane → uncovered background");
    }
}
