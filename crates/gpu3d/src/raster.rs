//! The software **scanline** rasterizer: it turns a sealed [`RenderList`] into a
//! 256×192 pixel buffer. Faithful to hardware, it fills a polygon scanline by
//! scanline — for each visible row it finds the polygon's span (left/right edge x)
//! and fills across it — rather than one host triangle at a time.
//!
//! This is the deliberately minimal first pass (per the plan): opaque, flat-shaded
//! (the polygon takes its first vertex's color), painter's order (later polygons in
//! the list overwrite earlier ones), no depth test and no textures. Depth/W-buffering,
//! Gouraud, perspective-correct texturing, alpha, fog and the rest are later phases.

use crate::geometry::{RenderList, Vertex};

/// The 3D screen dimensions.
pub const WIDTH: usize = 256;
pub const HEIGHT: usize = 192;

/// One rendered 3D pixel. For now just a color and whether any polygon covered it
/// (transparent elsewhere); depth/alpha/poly-id attach in later phases.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pixel3d {
    /// 6-bit-per-channel RGB.
    pub color: [u8; 3],
    /// Whether a polygon was drawn here (else the 3D layer is transparent).
    pub covered: bool,
}

/// The 3D engine's 256×192 output buffer.
pub struct Framebuffer3d {
    pub pixels: Vec<Pixel3d>,
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
        }
    }
    pub fn clear(&mut self) {
        self.pixels.iter_mut().for_each(|p| *p = Pixel3d::default());
    }
    /// The pixel at `(x, y)` (both in range).
    pub fn at(&self, x: usize, y: usize) -> Pixel3d {
        self.pixels[y * WIDTH + x]
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

/// A vertex projected to integer screen coordinates.
#[derive(Clone, Copy)]
struct ScreenVertex {
    x: i32,
    y: i32,
}

/// Perspective divide + viewport transform: clip `(x, y, w)` → screen pixel. NDC y is
/// up, so screen y is flipped. `w` is guaranteed positive by near-plane clipping.
fn project(v: &Vertex, vp: &Viewport) -> ScreenVertex {
    let w = (v.clip[3] as i64).max(1);
    let (x, y) = (v.clip[0] as i64, v.clip[1] as i64);
    let vp_w = (vp.x2 - vp.x1 + 1) as i64;
    let vp_h = (vp.y2 - vp.y1 + 1) as i64;
    ScreenVertex {
        x: (vp.x1 as i64 + (x + w) * vp_w / (2 * w)) as i32,
        y: (vp.y1 as i64 + (w - y) * vp_h / (2 * w)) as i32,
    }
}

/// A projected polygon ready to scan-fill.
struct ScreenPoly {
    pts: Vec<ScreenVertex>,
    color: [u8; 3],
    ymin: i32,
    ymax: i32,
}

/// The `[left, right]` span (inclusive) where the convex polygon crosses scanline `y`,
/// or `None` if it does not. Edges are tested half-open in y so each vertex is counted
/// once (a convex polygon then yields exactly two crossings).
fn span_at(pts: &[ScreenVertex], y: i32) -> Option<(i32, i32)> {
    let n = pts.len();
    let (mut lo, mut hi) = (i32::MAX, i32::MIN);
    let mut hit = false;
    for i in 0..n {
        let a = &pts[i];
        let b = &pts[(i + 1) % n];
        let crosses = (a.y <= y && y < b.y) || (b.y <= y && y < a.y);
        if !crosses {
            continue;
        }
        let x = a.x as i64 + (b.x as i64 - a.x as i64) * (y - a.y) as i64 / (b.y - a.y) as i64;
        let x = x as i32;
        lo = lo.min(x);
        hi = hi.max(x);
        hit = true;
    }
    hit.then_some((lo, hi))
}

/// Rasterize the sealed render list into `fb`.
pub fn render(list: &RenderList, fb: &mut Framebuffer3d) {
    fb.clear();
    let vp = Viewport::decode(list.viewport);
    let verts = list.vertices();

    // Project each polygon's vertices once and take its flat color (vertex 0).
    let polys: Vec<ScreenPoly> = list
        .polygons()
        .iter()
        .filter(|p| p.count >= 3)
        .map(|p| {
            let pts: Vec<ScreenVertex> = p.verts[..p.count as usize]
                .iter()
                .map(|&i| project(&verts[i as usize], &vp))
                .collect();
            let ymin = pts.iter().map(|s| s.y).min().unwrap();
            let ymax = pts.iter().map(|s| s.y).max().unwrap();
            ScreenPoly {
                pts,
                color: verts[p.verts[0] as usize].color,
                ymin,
                ymax,
            }
        })
        .collect();

    // Scanline by scanline; later polygons overwrite earlier ones (painter's order).
    for y in 0..HEIGHT as i32 {
        for sp in &polys {
            if y < sp.ymin || y > sp.ymax {
                continue;
            }
            let Some((xl, xr)) = span_at(&sp.pts, y) else {
                continue;
            };
            let xl = xl.max(0);
            let xr = xr.min(WIDTH as i32 - 1);
            let row = y as usize * WIDTH;
            for x in xl..=xr {
                fb.pixels[row + x as usize] = Pixel3d { color: sp.color, covered: true };
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
        let mut fb = Framebuffer3d::new();
        render(e.render_list(), &mut fb);
        fb
    }

    #[test]
    fn single_triangle_fills_its_interior_and_nothing_else() {
        // NDC (-0.5, 0.5), (0.5, 0.5), (0, -0.5) → screen (64,48),(192,48),(128,144).
        let fb = render_triangle(0x1F, [(-4, 4), (4, 4), (0, -4)]);
        assert!(fb.at(128, 96).covered, "the centroid is inside");
        assert_eq!(fb.at(128, 96).color, [63, 0, 0]); // flat red (6-bit)
        assert!(!fb.at(0, 0).covered, "top-left corner is outside");
        assert!(!fb.at(0, 96).covered, "left edge is outside the span");
        assert!(!fb.at(128, 10).covered, "above the triangle's top edge");
    }

    #[test]
    fn flat_color_comes_from_the_first_vertex() {
        let fb = render_triangle(0x03E0, [(-4, 4), (4, 4), (0, -4)]); // green
        assert_eq!(fb.at(128, 96).color, [0, 63, 0]);
    }

    #[test]
    fn painters_order_lets_later_polygons_win() {
        // Two overlapping full-viewport-ish triangles; the second (blue) is submitted
        // last and must cover the shared pixel.
        let mut e = GeometryEngine::new();
        e.execute(op::MTX_MODE, &[1]);
        e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7)]);
        let tri = [(-4, 4), (4, 4), (0, -4)];
        for &color in &[0x1Fu32, 0x7C00] {
            // red, then blue
            e.execute(op::COLOR, &[color]);
            e.execute(op::BEGIN_VTXS, &[0]);
            for (x, y) in tri {
                let lo = (e8(x) as u32 & 0xFFFF) | ((e8(y) as u32 & 0xFFFF) << 16);
                e.execute(op::VTX_16, &[lo, 0]);
            }
        }
        e.execute(op::SWAP_BUFFERS, &[0]);
        let mut fb = Framebuffer3d::new();
        render(e.render_list(), &mut fb);
        assert_eq!(fb.at(128, 96).color, [0, 0, 63], "the later (blue) triangle wins");
    }

    #[test]
    fn empty_render_list_produces_a_transparent_frame() {
        let fb = Framebuffer3d::new();
        assert!(fb.pixels.iter().all(|p| !p.covered));
    }
}
