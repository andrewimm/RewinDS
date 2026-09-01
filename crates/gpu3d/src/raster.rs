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

/// The 3D engine's 256×192 output buffer plus its depth buffer.
pub struct Framebuffer3d {
    pub pixels: Vec<Pixel3d>,
    /// Per-pixel depth; smaller is nearer. Cleared to the far value each frame.
    depth: Vec<i32>,
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
        }
    }
    pub fn clear(&mut self) {
        self.pixels.iter_mut().for_each(|p| *p = Pixel3d::default());
        self.depth.iter_mut().for_each(|d| *d = i32::MAX);
    }
    /// The pixel at `(x, y)` (both in range).
    pub fn at(&self, x: usize, y: usize) -> Pixel3d {
        self.pixels[y * WIDTH + x]
    }
}

/// Per-frame rendering controls from `DISP3DCNT` + `ALPHA_TEST_REF`.
#[derive(Clone, Copy)]
pub struct RenderConfig {
    /// `DISP3DCNT` bit 3: when clear, translucent polygons are drawn opaque (their
    /// pixels overwrite the framebuffer rather than blending).
    pub alpha_blend: bool,
    /// `DISP3DCNT` bit 2: gate pixels on `alpha > alpha_ref` instead of `alpha > 0`.
    pub alpha_test: bool,
    /// `ALPHA_TEST_REF` (0-31): the alpha-test comparison value.
    pub alpha_ref: u8,
}

impl Default for RenderConfig {
    /// The common case: alpha-blending on, no alpha test — the configuration the
    /// rasterizer unit tests assume. Real frames build this from the registers.
    fn default() -> Self {
        RenderConfig { alpha_blend: true, alpha_test: false, alpha_ref: 0 }
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

/// A vertex projected to integer screen coordinates, carrying the attributes the
/// rasterizer interpolates: `depth` (smaller = nearer), 6-bit RGB `color`, and the
/// 1.11.4 texture coordinate `st`.
#[derive(Clone, Copy)]
struct RVertex {
    x: i32,
    y: i32,
    depth: i32,
    color: [i32; 3],
    st: [i32; 2],
}

/// Perspective divide + viewport transform: clip `(x, y, w)` → screen pixel. Per GBATEK
/// the DS 3D coordinate origin is the **lower-left** (viewport Y1 = bottom-most, Y2 =
/// top-most), so NDC y runs upward and is flipped into the top-origin framebuffer row
/// (`row = 191 - y_bottom`). `w` is positive by near-plane clipping. Depth is W
/// (W-buffer) or `(z/w+1)` scaled to 24 bits.
fn project(v: &Vertex, vp: &Viewport, w_buffer: bool) -> RVertex {
    let w = (v.clip[3] as i64).max(1);
    let (x, y, z) = (v.clip[0] as i64, v.clip[1] as i64, v.clip[2] as i64);
    let vp_w = (vp.x2 - vp.x1 + 1) as i64;
    let vp_h = (vp.y2 - vp.y1 + 1) as i64;
    let depth = if w_buffer {
        w as i32
    } else {
        (z * 0x0080_0000 / w + 0x0080_0000) as i32
    };
    // Bottom-origin viewport y (Y1 at the screen bottom), then flip to the top-origin
    // framebuffer row shared with the 2D layers.
    let y_bottom = vp.y1 as i64 + (y + w) * vp_h / (2 * w);
    RVertex {
        x: (vp.x1 as i64 + (x + w) * vp_w / (2 * w)) as i32,
        y: (HEIGHT as i64 - 1 - y_bottom) as i32,
        depth,
        color: [v.color[0] as i32, v.color[1] as i32, v.color[2] as i32],
        st: v.texcoord,
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
    /// Texture-blend mode (`POLYGON_ATTR` bits 4-5): 0 = modulation, 1 = decal.
    blend_mode: u8,
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
            trans_depth_write: p.attr & (1 << 11) != 0,
            has_translucency,
        }
    }
}

/// Where one edge crosses a scanline, with the interpolated attributes there.
#[derive(Clone, Copy)]
struct Crossing {
    x: i32,
    depth: i32,
    color: [i32; 3],
    st: [i32; 2],
}

/// Interpolate the crossing of edge `a→b` at scanline `y` (caller guarantees `a.y != b.y`).
fn edge_crossing(a: &RVertex, b: &RVertex, y: i32) -> Crossing {
    let tn = (y - a.y) as i64;
    let td = (b.y - a.y) as i64;
    let lerp = |va: i32, vb: i32| (va as i64 + (vb as i64 - va as i64) * tn / td) as i32;
    Crossing {
        x: lerp(a.x, b.x),
        depth: lerp(a.depth, b.depth),
        color: [lerp(a.color[0], b.color[0]), lerp(a.color[1], b.color[1]), lerp(a.color[2], b.color[2])],
        st: [lerp(a.st[0], b.st[0]), lerp(a.st[1], b.st[1])],
    }
}

/// Which pixels a fill pass writes: opaque pixels first, then translucent blended over.
#[derive(Clone, Copy, PartialEq)]
enum Pass {
    Opaque,
    Translucent,
}

/// Combine a (possibly textured) polygon's interpolated vertex color with a sampled
/// texel per the blend mode, yielding the final 6-bit color and 5-bit alpha.
fn shade(rp: &RPoly, tex: &TextureSet, vtx: [i32; 3], st: [i32; 2]) -> ([u8; 3], u8) {
    let vtx = [vtx[0].clamp(0, 63), vtx[1].clamp(0, 63), vtx[2].clamp(0, 63)];
    if !rp.tex.textured() {
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
    let xl = l.x.max(0);
    let xr = r.x.min(WIDTH as i32 - 1);
    if xl > xr {
        return;
    }
    let span = (r.x - l.x).max(1) as i64;
    let row = y as usize * WIDTH;
    for x in xl..=xr {
        let sn = (x - l.x) as i64;
        let lerp = |va: i32, vb: i32| (va as i64 + (vb as i64 - va as i64) * sn / span) as i32;
        let idx = row + x as usize;
        let depth = lerp(l.depth, r.depth);
        if depth >= fb.depth[idx] {
            continue; // occluded by a nearer (or equal) pixel already drawn
        }
        let color = [lerp(l.color[0], r.color[0]), lerp(l.color[1], r.color[1]), lerp(l.color[2], r.color[2])];
        let st = [lerp(l.st[0], r.st[0]), lerp(l.st[1], r.st[1])];
        let (rgb, alpha) = shade(rp, tex, color, st);
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
            }
            Pass::Translucent if !opaque_pixel => {
                let dst = fb.pixels[idx];
                let a = alpha as i32;
                // GBATEK: FrameBuf = (Poly·(A+1) + FrameBuf·(31−A)) / 32, FrameBuf[A] =
                // max(A, FrameBuf[A]). Bypassed when the destination is transparent
                // (FrameBuf[A] = 0) — then the source is written as-is, so its alpha
                // carries the coverage into the 2D composite instead of blending to black.
                let (out_color, out_alpha) = if dst.covered {
                    let blend = |s: i32, d: i32| ((s * (a + 1) + d * (31 - a)) / 32) as u8;
                    (
                        [
                            blend(rgb[0] as i32, dst.color[0] as i32),
                            blend(rgb[1] as i32, dst.color[1] as i32),
                            blend(rgb[2] as i32, dst.color[2] as i32),
                        ],
                        a.max(dst.alpha as i32) as u8,
                    )
                } else {
                    (rgb, alpha)
                };
                fb.pixels[idx] = Pixel3d { color: out_color, alpha: out_alpha, covered: true };
                if rp.trans_depth_write {
                    fb.depth[idx] = depth;
                }
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
    let mut crossings = [Crossing { x: 0, depth: 0, color: [0; 3], st: [0; 2] }; 2];
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
    fb.clear();
    let vp = Viewport::decode(list.viewport);
    let w_buffer = list.w_buffer();
    let verts = list.vertices();

    let polys: Vec<RPoly> = list
        .polygons()
        .iter()
        .filter(|p| p.count >= 3)
        .map(|p| RPoly::build(p, verts, &vp, w_buffer))
        .collect();

    // Pass 1: opaque pixels of every polygon (depth-tested, write depth). When
    // alpha-blending is disabled, every passing pixel is drawn here as opaque.
    for y in 0..HEIGHT as i32 {
        for rp in &polys {
            scan_poly(fb, rp, tex, cfg, y, Pass::Opaque);
        }
    }
    // Pass 2: translucent pixels blend over the resolved opaque layer, in submission
    // order (a first cut ahead of true translucent depth sorting). Skipped entirely
    // when alpha-blending is disabled.
    if cfg.alpha_blend {
        for y in 0..HEIGHT as i32 {
            for rp in &polys {
                if rp.has_translucency {
                    scan_poly(fb, rp, tex, cfg, y, Pass::Translucent);
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
}
