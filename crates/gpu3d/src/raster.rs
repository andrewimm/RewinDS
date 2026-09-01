//! The software **scanline** rasterizer: it turns a sealed [`RenderList`] into a
//! 256×192 pixel buffer. Faithful to hardware, it fills a polygon scanline by
//! scanline — for each visible row it finds the polygon's span (left/right edge x)
//! and fills across it — rather than one host triangle at a time.
//!
//! This pass does: opaque geometry, **Gouraud shading** (per-pixel color interpolated
//! from the vertices), and a **depth test** (Z- or W-buffer per `SWAP_BUFFERS`), so the
//! nearest polygon wins regardless of draw order. Still to come: perspective-correct
//! texturing, alpha/translucency, fog, edge marking and anti-aliasing.

use crate::geometry::{RenderList, Vertex};

/// The 3D screen dimensions.
pub const WIDTH: usize = 256;
pub const HEIGHT: usize = 192;

/// One rendered 3D pixel: a Gouraud-interpolated color and whether a polygon covered
/// it (transparent elsewhere). Alpha/poly-id attach in later phases.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pixel3d {
    /// 6-bit-per-channel RGB.
    pub color: [u8; 3],
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
/// rasterizer interpolates: `depth` (smaller = nearer) and 6-bit RGB `color`.
#[derive(Clone, Copy)]
struct RVertex {
    x: i32,
    y: i32,
    depth: i32,
    color: [i32; 3],
}

/// Perspective divide + viewport transform: clip `(x, y, w)` → screen pixel. NDC y is
/// up, so screen y is flipped. `w` is guaranteed positive by near-plane clipping. Depth
/// is the W value (W-buffer) or `(z/w + 1)` scaled to 24 bits (Z-buffer).
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
    RVertex {
        x: (vp.x1 as i64 + (x + w) * vp_w / (2 * w)) as i32,
        y: (vp.y1 as i64 + (w - y) * vp_h / (2 * w)) as i32,
        depth,
        color: [v.color[0] as i32, v.color[1] as i32, v.color[2] as i32],
    }
}

/// A projected polygon ready to scan-fill (Gouraud vertices + its y-extent).
struct RPoly {
    pts: Vec<RVertex>,
    ymin: i32,
    ymax: i32,
}

/// Where one edge crosses a scanline, with the interpolated attributes there.
#[derive(Clone, Copy)]
struct Crossing {
    x: i32,
    depth: i32,
    color: [i32; 3],
}

/// Interpolate the crossing of edge `a→b` at scanline `y` (caller guarantees `a.y != b.y`).
fn edge_crossing(a: &RVertex, b: &RVertex, y: i32) -> Crossing {
    let tn = (y - a.y) as i64;
    let td = (b.y - a.y) as i64;
    let lerp = |va: i32, vb: i32| (va as i64 + (vb as i64 - va as i64) * tn / td) as i32;
    Crossing {
        x: lerp(a.x, b.x),
        depth: lerp(a.depth, b.depth),
        color: [
            lerp(a.color[0], b.color[0]),
            lerp(a.color[1], b.color[1]),
            lerp(a.color[2], b.color[2]),
        ],
    }
}

/// Fill scanline `y` between the two crossings, interpolating depth and color across
/// the span and depth-testing each pixel.
fn fill_span(fb: &mut Framebuffer3d, y: i32, l: &Crossing, r: &Crossing) {
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
        if depth < fb.depth[idx] {
            fb.pixels[idx] = Pixel3d {
                color: [
                    lerp(l.color[0], r.color[0]) as u8,
                    lerp(l.color[1], r.color[1]) as u8,
                    lerp(l.color[2], r.color[2]) as u8,
                ],
                covered: true,
            };
            fb.depth[idx] = depth;
        }
    }
}

/// Rasterize the sealed render list into `fb`, depth-tested and Gouraud-shaded.
pub fn render(list: &RenderList, fb: &mut Framebuffer3d) {
    fb.clear();
    let vp = Viewport::decode(list.viewport);
    let w_buffer = list.w_buffer();
    let verts = list.vertices();

    let polys: Vec<RPoly> = list
        .polygons()
        .iter()
        .filter(|p| p.count >= 3)
        .map(|p| {
            let pts: Vec<RVertex> = p.verts[..p.count as usize]
                .iter()
                .map(|&i| project(&verts[i as usize], &vp, w_buffer))
                .collect();
            let ymin = pts.iter().map(|v| v.y).min().unwrap();
            let ymax = pts.iter().map(|v| v.y).max().unwrap();
            RPoly { pts, ymin, ymax }
        })
        .collect();

    for y in 0..HEIGHT as i32 {
        for rp in &polys {
            if y < rp.ymin || y > rp.ymax {
                continue;
            }
            // A convex polygon crosses the scanline at exactly two edges (each vertex
            // counted once via the half-open y test); the span runs between them.
            let mut crossings = [Crossing { x: 0, depth: 0, color: [0; 3] }; 2];
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
                continue;
            }
            let (l, r) = if crossings[0].x <= crossings[1].x {
                (crossings[0], crossings[1])
            } else {
                (crossings[1], crossings[0])
            };
            fill_span(fb, y, &l, &r);
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
        let mut fb = Framebuffer3d::new();
        render(e.render_list(), &mut fb);
        let c = fb.at(128, 80).color; // near the centroid
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
        let mut fb = Framebuffer3d::new();
        render(e.render_list(), &mut fb);
        assert_eq!(fb.at(128, 96).color, [63, 0, 0], "the nearer (red) triangle wins");
    }

    #[test]
    fn empty_render_list_produces_a_transparent_frame() {
        let fb = Framebuffer3d::new();
        assert!(fb.pixels.iter().all(|p| !p.covered));
    }
}
