//! The geometry engine: current vertex state, the vertex-format decoders, primitive
//! assembly (triangles / quads / strips), frustum clipping, back/front-face culling,
//! and the vertex + polygon build buffers.
//!
//! Each submitted vertex is transformed by the clip matrix into clip space and held in
//! a transient primitive buffer. When a primitive completes, it is **clipped against
//! the six frustum planes** (Sutherland–Hodgman, in clip space before the perspective
//! divide) and **culled** by winding against its `POLYGON_ATTR` render bits; the
//! surviving vertices are what land in vertex RAM (so vertex RAM is post-clip, as on
//! hardware) and form a polygon in polygon RAM.
//!
//! Everything is fixed-point: positions/clip are 4.12 (`1.0 == 0x1000`), texcoords
//! 1.11.4, normals/deltas signed 10-bit. Interpolation at clip boundaries uses `i64`.

use crate::command::op;
use crate::debug::{Polygon3dProvenance, Vertex3dProvenance};
use crate::matrix::{MatrixEngine, ONE};

/// Vertex RAM depth (GBATEK: 6144 vertices per frame).
const VERTEX_RAM: usize = 6144;
/// Polygon RAM depth (GBATEK: 2048 polygons per frame).
const POLYGON_RAM: usize = 2048;
/// Maximum vertices in a polygon: a triangle or quad plus one new vertex per frustum
/// plane after clipping.
pub const MAX_POLY_VERTS: usize = 10;
/// Sentinel `source_op` for a vertex created by clipping (not a real `VTX_*` opcode).
pub const CLIP_GENERATED: u8 = 0xFF;

/// A geometry vertex after transform and clipping: clip-space position plus the
/// attributes to interpolate. Colors are 5-bit RGB (expanded later); texcoords 1.11.4.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vertex {
    /// Clip-space `(x, y, z, w)` in 4.12 (before the perspective divide).
    pub clip: [i32; 4],
    /// Vertex color, 5-bit per channel (`COLOR`, or lighting output in Phase 6).
    pub color: [u8; 3],
    /// Texture coordinate `(s, t)` in 1.11.4.
    pub texcoord: [i32; 2],
}

/// An assembled polygon: indices into vertex RAM plus the latched render attributes.
#[derive(Clone, Copy, Debug)]
pub struct Polygon {
    /// Vertex-RAM indices, `count` of them used.
    pub verts: [u16; MAX_POLY_VERTS],
    pub count: u8,
    /// `POLYGON_ATTR` latched at the `BEGIN_VTXS` that opened this polygon's list.
    pub attr: u32,
    pub tex_param: u32,
    pub pltt_base: u32,
    /// Provenance: the `BEGIN_VTXS` primitive type and command sequence number.
    pub primitive: u8,
    pub begin_seq: u32,
}

/// Durable per-vertex provenance kept parallel to vertex RAM (the hot [`Vertex`] stays
/// lean). The full [`Vertex3dProvenance`] is built from this plus the vertex on query.
#[derive(Clone, Copy, Default)]
struct VertexOrigin {
    source_op: u8,
    command_seq: u32,
    object_position: [i32; 3],
}

/// A vertex flowing through primitive assembly and clipping — full attributes carried
/// at interpolation precision (color as `i32`), plus its provenance origin.
#[derive(Clone, Copy, Default)]
struct ClipVertex {
    clip: [i32; 4],
    color: [i32; 3],
    texcoord: [i32; 2],
    origin: VertexOrigin,
}

impl ClipVertex {
    fn to_vertex(self) -> Vertex {
        Vertex {
            clip: self.clip,
            color: [
                self.color[0].clamp(0, 31) as u8,
                self.color[1].clamp(0, 31) as u8,
                self.color[2].clamp(0, 31) as u8,
            ],
            texcoord: self.texcoord,
        }
    }
}

/// Sign-extend the low 10 bits of `v` (used by `VTX_10`, `VTX_DIFF`, `NORMAL`).
fn se10(v: u32) -> i32 {
    ((v as i32 & 0x3FF) << 22) >> 22
}

/// Sign-extend the low 16 bits of `v` to `i32` (used by the 16-bit vertex formats).
fn se16(v: u32) -> i32 {
    v as i16 as i32
}

/// The clip-space plane function for a vertex: `>= 0` means inside that plane. The six
/// planes are `w±x`, `w±y`, `w±z` — the frustum `-w ≤ x,y,z ≤ w` before the divide.
fn plane_value(clip: &[i32; 4], plane: usize) -> i64 {
    let (x, y, z, w) = (clip[0] as i64, clip[1] as i64, clip[2] as i64, clip[3] as i64);
    match plane {
        0 => w + x, // left
        1 => w - x, // right
        2 => w + y, // bottom
        3 => w - y, // top
        4 => w + z, // near
        _ => w - z, // far
    }
}

/// Interpolate a new vertex where edge `a→b` crosses a plane, `fa`/`fb` being the two
/// endpoints' plane values (opposite signs). `t = fa / (fa - fb)`, applied to every
/// attribute in `i64`.
fn intersect(a: &ClipVertex, b: &ClipVertex, fa: i64, fb: i64) -> ClipVertex {
    let denom = fa - fb;
    let lerp = |va: i32, vb: i32| -> i32 {
        (va as i64 + (vb as i64 - va as i64) * fa / denom) as i32
    };
    ClipVertex {
        clip: [
            lerp(a.clip[0], b.clip[0]),
            lerp(a.clip[1], b.clip[1]),
            lerp(a.clip[2], b.clip[2]),
            lerp(a.clip[3], b.clip[3]),
        ],
        color: [
            lerp(a.color[0], b.color[0]),
            lerp(a.color[1], b.color[1]),
            lerp(a.color[2], b.color[2]),
        ],
        texcoord: [lerp(a.texcoord[0], b.texcoord[0]), lerp(a.texcoord[1], b.texcoord[1])],
        origin: VertexOrigin {
            source_op: CLIP_GENERATED,
            command_seq: a.origin.command_seq,
            object_position: [
                lerp(a.origin.object_position[0], b.origin.object_position[0]),
                lerp(a.origin.object_position[1], b.origin.object_position[1]),
                lerp(a.origin.object_position[2], b.origin.object_position[2]),
            ],
        },
    }
}

/// Clip a convex polygon against one frustum plane (Sutherland–Hodgman): keep inside
/// vertices, and emit an interpolated vertex wherever an edge crosses the plane.
fn clip_plane(poly: &[ClipVertex], plane: usize, out: &mut Vec<ClipVertex>) {
    out.clear();
    let n = poly.len();
    for i in 0..n {
        let cur = &poly[i];
        let prev = &poly[(i + n - 1) % n];
        let fcur = plane_value(&cur.clip, plane);
        let fprev = plane_value(&prev.clip, plane);
        let cur_in = fcur >= 0;
        let prev_in = fprev >= 0;
        if cur_in {
            if !prev_in {
                out.push(intersect(prev, cur, fprev, fcur)); // entering
            }
            out.push(*cur);
        } else if prev_in {
            out.push(intersect(prev, cur, fprev, fcur)); // leaving
        }
    }
}

/// The homogeneous signed area of the first three clip-space vertices — its sign is the
/// winding, used for back/front-face culling.
fn signed_area(a: &[i32; 4], b: &[i32; 4], c: &[i32; 4]) -> i64 {
    let (ax, ay, aw) = (a[0] as i64, a[1] as i64, a[3] as i64);
    let (bx, by, bw) = (b[0] as i64, b[1] as i64, b[3] as i64);
    let (cx, cy, cw) = (c[0] as i64, c[1] as i64, c[3] as i64);
    ax * (by * cw - cy * bw) - ay * (bx * cw - cx * bw) + aw * (bx * cy - cx * by)
}

/// Whether a polygon is culled given its `POLYGON_ATTR` render bits (6 = back, 7 =
/// front) and its winding. Both bits set never culls; neither set always culls. The
/// front-face convention (positive homogeneous signed area) was verified against NSMB:
/// its front-only title-logo polygons survive with this sign and vanish with the other.
fn culled(attr: u32, poly: &[ClipVertex]) -> bool {
    let render_back = attr & (1 << 6) != 0;
    let render_front = attr & (1 << 7) != 0;
    if render_front && render_back {
        return false;
    }
    if !render_front && !render_back {
        return true;
    }
    let front = signed_area(&poly[0].clip, &poly[1].clip, &poly[2].clip) > 0;
    (front && !render_front) || (!front && !render_back)
}

/// Current geometry state set by the state commands and sampled at each vertex.
#[derive(Clone, Copy, Default)]
struct State {
    /// Current vertex position (4.12), updated in full or per-component by the VTX_*
    /// commands.
    position: [i32; 3],
    color: [u8; 3],
    texcoord: [i32; 2],
    /// `POLYGON_ATTR` staged by the command, latched into `cur_attr` at `BEGIN_VTXS`.
    pending_attr: u32,
    cur_attr: u32,
    tex_param: u32,
    pltt_base: u32,
    /// Command sequence number of the current list's `BEGIN_VTXS` (polygon provenance).
    begin_seq: u32,
}

/// Groups the stream of submitted vertices into primitives per the `BEGIN_VTXS` type,
/// returning a primitive's (pre-clip) vertices as soon as one completes.
#[derive(Default)]
struct Assembler {
    prim: u8,
    /// Vertices submitted since `begin` (kept for strips; cleared after each complete
    /// primitive for lists).
    run: Vec<ClipVertex>,
}

impl Assembler {
    fn begin(&mut self, prim: u8) {
        self.prim = prim & 3;
        self.run.clear();
    }

    /// Record a submitted vertex; return a primitive's `count` vertices when complete.
    fn add(&mut self, cv: ClipVertex) -> Option<([ClipVertex; 4], u8)> {
        self.run.push(cv);
        let n = self.run.len();
        match self.prim {
            0 => (n == 3).then(|| {
                let p = [self.run[0], self.run[1], self.run[2], ClipVertex::default()];
                self.run.clear();
                (p, 3)
            }),
            1 => (n == 4).then(|| {
                let p = [self.run[0], self.run[1], self.run[2], self.run[3]];
                self.run.clear();
                (p, 4)
            }),
            2 => (n >= 3).then(|| {
                // Triangle strip: odd triangles swap the first two for consistent winding.
                let (a, b, c) = (self.run[n - 3], self.run[n - 2], self.run[n - 1]);
                let tri = n - 3;
                (
                    if tri.is_multiple_of(2) {
                        [a, b, c, ClipVertex::default()]
                    } else {
                        [b, a, c, ClipVertex::default()]
                    },
                    3,
                )
            }),
            _ => (n >= 4 && n.is_multiple_of(2)).then(|| {
                ([self.run[n - 4], self.run[n - 3], self.run[n - 1], self.run[n - 2]], 4)
            }),
        }
    }
}

/// The geometry engine: matrix stack, current state, and the post-clip build buffers.
#[derive(Default)]
pub struct GeometryEngine {
    pub matrix: MatrixEngine,
    state: State,
    vertices: Vec<Vertex>,
    polygons: Vec<Polygon>,
    /// Provenance parallel to `vertices` (same index; cleared together on swap).
    vertex_origins: Vec<VertexOrigin>,
    assembler: Assembler,
    /// Reused Sutherland–Hodgman ping-pong buffers (avoid per-polygon allocation).
    clip_a: Vec<ClipVertex>,
    clip_b: Vec<ClipVertex>,
    /// Commands executed since the last `SWAP_BUFFERS` — the per-frame command
    /// sequence number stamped on vertex/polygon provenance.
    command_seq: u32,
    /// High-water marks of `(polygons, vertices)` built in a single frame, captured at
    /// each `SWAP_BUFFERS` — a debug window on how much geometry a game submits.
    peak: (usize, usize),
}

impl GeometryEngine {
    pub fn new() -> Self {
        GeometryEngine::default()
    }

    // --- read-back ----------------------------------------------------------

    pub fn vertices(&self) -> &[Vertex] {
        &self.vertices
    }
    pub fn polygons(&self) -> &[Polygon] {
        &self.polygons
    }

    /// `RAM_COUNT` (`0x4000604`): polygon count (bits 0-11) and vertex count (16-28).
    pub fn ram_count(&self) -> u32 {
        (self.polygons.len() as u32 & 0xFFF) | ((self.vertices.len() as u32 & 0x1FFF) << 16)
    }

    /// The `GXSTAT` bits the geometry side owns (currently the matrix-stack bits;
    /// geometry-busy is added with command timing).
    pub fn gxstat_bits(&self) -> u32 {
        self.matrix.gxstat_bits()
    }

    /// The `(polygons, vertices)` high-water mark across all frames so far.
    pub fn peak(&self) -> (usize, usize) {
        self.peak
    }

    // --- command execution --------------------------------------------------

    /// Execute one fully-assembled geometry command.
    pub fn execute(&mut self, cmd: u8, params: &[u32]) {
        use op::*;
        self.command_seq += 1;
        match cmd {
            MTX_MODE => self.matrix.set_mode(params[0] as u8),
            MTX_PUSH => self.matrix.push(),
            MTX_POP => self.matrix.pop(params[0]),
            MTX_STORE => self.matrix.store(params[0]),
            MTX_RESTORE => self.matrix.restore(params[0]),
            MTX_IDENTITY => self.matrix.load_identity(),
            MTX_LOAD_4X4 => self.matrix.load_4x4(params),
            MTX_LOAD_4X3 => self.matrix.load_4x3(params),
            MTX_MULT_4X4 => self.matrix.mult_4x4(params),
            MTX_MULT_4X3 => self.matrix.mult_4x3(params),
            MTX_MULT_3X3 => self.matrix.mult_3x3(params),
            MTX_SCALE => self.matrix.scale(params),
            MTX_TRANS => self.matrix.translate(params),

            COLOR => self.state.color = unpack_rgb5(params[0]),
            TEXCOORD => {
                self.state.texcoord = [se16(params[0]), se16(params[0] >> 16)];
            }

            VTX_16 => {
                self.state.position = [se16(params[0]), se16(params[0] >> 16), se16(params[1])];
                self.submit_vertex(VTX_16);
            }
            VTX_10 => {
                let p = params[0];
                self.state.position = [se10(p) << 6, se10(p >> 10) << 6, se10(p >> 20) << 6];
                self.submit_vertex(VTX_10);
            }
            VTX_XY => {
                self.state.position[0] = se16(params[0]);
                self.state.position[1] = se16(params[0] >> 16);
                self.submit_vertex(VTX_XY);
            }
            VTX_XZ => {
                self.state.position[0] = se16(params[0]);
                self.state.position[2] = se16(params[0] >> 16);
                self.submit_vertex(VTX_XZ);
            }
            VTX_YZ => {
                self.state.position[1] = se16(params[0]);
                self.state.position[2] = se16(params[0] >> 16);
                self.submit_vertex(VTX_YZ);
            }
            VTX_DIFF => {
                let p = params[0];
                self.state.position[0] += se10(p);
                self.state.position[1] += se10(p >> 10);
                self.state.position[2] += se10(p >> 20);
                self.submit_vertex(VTX_DIFF);
            }

            POLYGON_ATTR => self.state.pending_attr = params[0],
            TEXIMAGE_PARAM => self.state.tex_param = params[0],
            PLTT_BASE => self.state.pltt_base = params[0],
            BEGIN_VTXS => {
                // POLYGON_ATTR takes effect here, at the start of the vertex list.
                self.state.cur_attr = self.state.pending_attr;
                self.state.begin_seq = self.command_seq;
                self.assembler.begin(params[0] as u8);
            }
            END_VTXS => {}
            SWAP_BUFFERS => self.swap_buffers(),

            // NORMAL / lighting / materials — Phase 6; other commands — later phases.
            _ => {}
        }
    }

    /// Transform the current position by the clip matrix into a clip-space vertex and
    /// feed the primitive assembler; a completed primitive is finalized (clipped,
    /// culled, emitted). `source_op` is the vertex command that produced it.
    fn submit_vertex(&mut self, source_op: u8) {
        let [x, y, z] = self.state.position;
        let clip = self.matrix.clip().transform([x, y, z, ONE]);
        let cv = ClipVertex {
            clip,
            color: [
                self.state.color[0] as i32,
                self.state.color[1] as i32,
                self.state.color[2] as i32,
            ],
            texcoord: self.state.texcoord,
            origin: VertexOrigin {
                source_op,
                command_seq: self.command_seq,
                object_position: self.state.position,
            },
        };
        if let Some((prim, count)) = self.assembler.add(cv) {
            self.finalize_polygon(&prim[..count as usize]);
        }
    }

    /// Clip a completed primitive against the six frustum planes, cull it by winding,
    /// and emit the survivors to vertex RAM as a polygon.
    fn finalize_polygon(&mut self, prim: &[ClipVertex]) {
        // Sutherland–Hodgman across all six planes, ping-ponging the two buffers.
        let mut src = std::mem::take(&mut self.clip_a);
        let mut dst = std::mem::take(&mut self.clip_b);
        src.clear();
        src.extend_from_slice(prim);
        for plane in 0..6 {
            clip_plane(&src, plane, &mut dst);
            std::mem::swap(&mut src, &mut dst);
            if src.len() < 3 {
                break;
            }
        }
        let survived = src.len() >= 3 && !culled(self.state.cur_attr, &src);
        if survived {
            self.emit_polygon(&src);
        }
        // Return the buffers for reuse.
        self.clip_a = src;
        self.clip_b = dst;
    }

    /// Append a clipped polygon's vertices to vertex RAM and record the polygon.
    fn emit_polygon(&mut self, poly: &[ClipVertex]) {
        let n = poly.len().min(MAX_POLY_VERTS);
        if self.polygons.len() >= POLYGON_RAM || self.vertices.len() + n > VERTEX_RAM {
            return;
        }
        let mut verts = [0u16; MAX_POLY_VERTS];
        for (j, cv) in poly.iter().take(n).enumerate() {
            verts[j] = self.vertices.len() as u16;
            self.vertices.push(cv.to_vertex());
            self.vertex_origins.push(cv.origin);
        }
        self.polygons.push(Polygon {
            verts,
            count: n as u8,
            attr: self.state.cur_attr,
            tex_param: self.state.tex_param,
            pltt_base: self.state.pltt_base,
            primitive: self.assembler.prim,
            begin_seq: self.state.begin_seq,
        });
    }

    /// Build the full provenance of vertex-RAM entry `i` from its durable origin.
    pub fn explain_vertex(&self, i: usize) -> Option<Vertex3dProvenance> {
        let v = self.vertices.get(i)?;
        let o = &self.vertex_origins[i];
        Some(Vertex3dProvenance {
            index: i as u16,
            source_op: o.source_op,
            command_seq: o.command_seq,
            object_position: o.object_position,
            clip: v.clip,
        })
    }

    /// Build the full provenance of polygon-RAM entry `i`.
    pub fn explain_polygon(&self, i: usize) -> Option<Polygon3dProvenance> {
        let p = self.polygons.get(i)?;
        Some(Polygon3dProvenance {
            index: i as u16,
            primitive: p.primitive,
            command_seq: p.begin_seq,
            attr: p.attr,
            poly_id: ((p.attr >> 24) & 0x3F) as u8,
            tex_param: p.tex_param,
            pltt_base: p.pltt_base,
            vertices: p.verts[..p.count as usize].to_vec(),
        })
    }

    /// `SWAP_BUFFERS`: reset the build buffers. Phase 7 first seals them into an
    /// immutable render list for the rasterizer; for now it only clears them so the
    /// next frame starts fresh (and RAM does not overflow across frames).
    fn swap_buffers(&mut self) {
        self.peak.0 = self.peak.0.max(self.polygons.len());
        self.peak.1 = self.peak.1.max(self.vertices.len());
        self.vertices.clear();
        self.vertex_origins.clear();
        self.polygons.clear();
        self.assembler.run.clear();
        self.command_seq = 0;
    }
}

/// Unpack an `RGB555` color word into 5-bit `[r, g, b]`.
fn unpack_rgb5(word: u32) -> [u8; 3] {
    [
        (word & 0x1F) as u8,
        ((word >> 5) & 0x1F) as u8,
        ((word >> 10) & 0x1F) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::ONE;

    /// A coordinate `n` eighths of a unit (in-frustum for `|n| <= 8`, since w = 1.0).
    fn e8(n: i32) -> i32 {
        n * ONE / 8
    }

    /// Engine in position mode (identity clip → clip coords equal object coords).
    fn engine() -> GeometryEngine {
        let mut e = GeometryEngine::new();
        e.execute(op::MTX_MODE, &[1]);
        e
    }

    /// Open a vertex list rendering both faces (so winding culling never fires).
    fn begin(e: &mut GeometryEngine, prim: u8) {
        e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7)]);
        e.execute(op::BEGIN_VTXS, &[prim as u32]);
    }

    /// Submit a `VTX_16` at coordinates given in eighths of a unit.
    fn vtx(e: &mut GeometryEngine, x: i32, y: i32, z: i32) {
        let lo = (e8(x) as u32 & 0xFFFF) | ((e8(y) as u32 & 0xFFFF) << 16);
        e.execute(op::VTX_16, &[lo, e8(z) as u32 & 0xFFFF]);
    }

    /// The clip-space x of each vertex of polygon `i` (encodes the assembly winding).
    fn poly_xs(e: &GeometryEngine, i: usize) -> Vec<i32> {
        let p = &e.polygons()[i];
        (0..p.count as usize).map(|j| e.vertices()[p.verts[j] as usize].clip[0]).collect()
    }

    #[test]
    fn vtx16_decodes_and_transforms_to_clip_space() {
        let mut e = engine();
        begin(&mut e, 0);
        vtx(&mut e, 2, 3, 4); // vertex 0, in-frustum
        vtx(&mut e, -2, 3, 0); // complete the triangle so it emits
        vtx(&mut e, 0, -3, 0);
        assert_eq!(e.vertices()[0].clip, [e8(2), e8(3), e8(4), ONE]);
    }

    #[test]
    fn vtx10_expands_4p6_to_4p12() {
        let mut e = engine();
        begin(&mut e, 0);
        // 4.6 → 4.12 is <<6: x = 16 → 16<<6 = e8(2); y = -16 → -16<<6 = e8(-2).
        let x = 16u32 & 0x3FF;
        let y = ((-16i32) as u32) & 0x3FF;
        e.execute(op::VTX_10, &[x | (y << 10)]); // vertex 0
        vtx(&mut e, -2, 4, 0); // complete the triangle
        vtx(&mut e, 2, 4, 0);
        assert_eq!(e.vertices()[0].clip[0], e8(2));
        assert_eq!(e.vertices()[0].clip[1], e8(-2));
    }

    #[test]
    fn vtx_diff_adds_a_small_delta_to_the_previous_position() {
        let mut e = engine();
        begin(&mut e, 0);
        vtx(&mut e, 1, 1, 1);
        e.execute(op::VTX_DIFF, &[5]); // +5 raw (4.12 units) on x
        vtx(&mut e, 2, 2, 2); // complete the triangle so it emits
        // Vertex 1 (the VTX_DIFF) is the second emitted vertex.
        assert_eq!(e.vertices()[1].clip[0], e8(1) + 5);
        assert_eq!(e.vertices()[1].clip[1], e8(1));
    }

    #[test]
    fn triangle_list_groups_every_three_vertices() {
        let mut e = engine();
        begin(&mut e, 0);
        for i in 0..6 {
            vtx(&mut e, i, i, 0); // non-degenerate, in-frustum
        }
        assert_eq!(e.polygons().len(), 2);
        assert_eq!(poly_xs(&e, 0), vec![e8(0), e8(1), e8(2)]);
        assert_eq!(poly_xs(&e, 1), vec![e8(3), e8(4), e8(5)]);
    }

    #[test]
    fn quad_list_groups_every_four_vertices() {
        let mut e = engine();
        begin(&mut e, 1);
        for i in 0..4 {
            vtx(&mut e, i, i * i % 5, 0);
        }
        assert_eq!(e.polygons().len(), 1);
        assert_eq!(e.polygons()[0].count, 4);
        assert_eq!(poly_xs(&e, 0), vec![e8(0), e8(1), e8(2), e8(3)]);
    }

    #[test]
    fn triangle_strip_shares_vertices_with_alternating_winding() {
        let mut e = engine();
        begin(&mut e, 2);
        for i in 0..5 {
            vtx(&mut e, i, i, 0);
        }
        // 3 triangles: (v0,v1,v2), (v2,v1,v3) [odd, swapped], (v2,v3,v4) — asserted by x.
        assert_eq!(e.polygons().len(), 3);
        assert_eq!(poly_xs(&e, 0), vec![e8(0), e8(1), e8(2)]);
        assert_eq!(poly_xs(&e, 1), vec![e8(2), e8(1), e8(3)]);
        assert_eq!(poly_xs(&e, 2), vec![e8(2), e8(3), e8(4)]);
    }

    #[test]
    fn quad_strip_pairs_new_vertices_with_the_trailing_edge() {
        let mut e = engine();
        begin(&mut e, 3);
        for i in 0..6 {
            vtx(&mut e, i, i, 0);
        }
        assert_eq!(e.polygons().len(), 2);
        assert_eq!(poly_xs(&e, 0), vec![e8(0), e8(1), e8(3), e8(2)]);
        assert_eq!(poly_xs(&e, 1), vec![e8(2), e8(3), e8(5), e8(4)]);
    }

    #[test]
    fn polygon_attr_latches_at_begin_and_color_rides_the_vertex() {
        let mut e = engine();
        e.execute(op::COLOR, &[0x1F | (0x1F << 10)]); // r=31, b=31
        e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7) | (0xBEEF << 8)]);
        e.execute(op::BEGIN_VTXS, &[0]);
        for i in 0..3 {
            vtx(&mut e, i, i, 0);
        }
        assert_eq!(e.polygons()[0].attr, (1 << 6) | (1 << 7) | (0xBEEF << 8));
        assert_eq!(e.vertices()[0].color, [31, 0, 31]);
    }

    #[test]
    fn fully_inside_triangle_passes_through_clipping_unchanged() {
        let mut e = engine();
        begin(&mut e, 0);
        vtx(&mut e, 1, 1, 0);
        vtx(&mut e, -1, 1, 0);
        vtx(&mut e, 0, -1, 0);
        assert_eq!(e.polygons().len(), 1);
        assert_eq!(e.polygons()[0].count, 3); // no new vertices
        // Source ops preserved (not clip-generated).
        assert_eq!(e.explain_vertex(0).unwrap().source_op, op::VTX_16);
    }

    #[test]
    fn triangle_crossing_the_right_plane_is_clipped_to_within_it() {
        let mut e = engine();
        begin(&mut e, 0);
        vtx(&mut e, 0, -4, 0);
        vtx(&mut e, 0, 4, 0);
        vtx(&mut e, 12, 0, 0); // x = 1.5 > w = 1.0 → outside the right plane
        let p = &e.polygons()[0];
        assert_eq!(p.count, 4); // one outside vertex → two boundary vertices
        // Every surviving vertex satisfies the right plane (w - x >= 0).
        for j in 0..p.count as usize {
            let c = e.vertices()[p.verts[j] as usize].clip;
            assert!(plane_value(&c, 1) >= 0);
        }
        // A boundary vertex was clip-generated.
        assert!((0..p.count as usize)
            .any(|j| e.explain_vertex(p.verts[j] as usize).unwrap().source_op == CLIP_GENERATED));
    }

    #[test]
    fn triangle_fully_outside_is_dropped() {
        let mut e = engine();
        begin(&mut e, 0);
        vtx(&mut e, 12, 0, 0);
        vtx(&mut e, 14, 4, 0);
        vtx(&mut e, 13, -4, 0); // all x > w → beyond the right plane
        assert_eq!(e.polygons().len(), 0);
    }

    #[test]
    fn clipping_interpolates_vertex_color_at_the_boundary() {
        let mut e = engine();
        e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7)]);
        e.execute(op::BEGIN_VTXS, &[0]);
        // Inside vertex is red (r=31); outside vertices black; the clipped edge vertex's
        // color must lie strictly between.
        e.execute(op::COLOR, &[0x1F]);
        vtx(&mut e, 0, 0, 0);
        e.execute(op::COLOR, &[0]);
        vtx(&mut e, 12, 4, 0);
        vtx(&mut e, 12, -4, 0);
        let p = &e.polygons()[0];
        let reds: Vec<u8> = (0..p.count as usize).map(|j| e.vertices()[p.verts[j] as usize].color[0]).collect();
        assert!(reds.iter().any(|&r| r > 0 && r < 31), "an interpolated red between 0 and 31: {reds:?}");
    }

    #[test]
    fn back_facing_triangle_is_culled_when_only_front_renders() {
        // Front-only render bit; a triangle of each winding — exactly one survives.
        let ccw = |e: &mut GeometryEngine| {
            e.execute(op::POLYGON_ATTR, &[1 << 7]); // front only
            e.execute(op::BEGIN_VTXS, &[0]);
            vtx(e, 0, 0, 0);
            vtx(e, 4, 0, 0);
            vtx(e, 0, 4, 0);
        };
        let cw = |e: &mut GeometryEngine| {
            e.execute(op::POLYGON_ATTR, &[1 << 7]);
            e.execute(op::BEGIN_VTXS, &[0]);
            vtx(e, 0, 0, 0);
            vtx(e, 0, 4, 0);
            vtx(e, 4, 0, 0);
        };
        let mut a = engine();
        ccw(&mut a);
        let mut b = engine();
        cw(&mut b);
        // The two windings differ, so exactly one is culled by a front-only attr.
        assert_ne!(a.polygons().len(), b.polygons().len());
    }

    #[test]
    fn explain_vertex_reports_source_command_and_object_position() {
        let mut e = engine();
        begin(&mut e, 0);
        vtx(&mut e, 1, 2, 3);
        vtx(&mut e, -1, 2, 3);
        vtx(&mut e, 0, -2, 0);
        let p = e.explain_vertex(0).unwrap();
        assert_eq!(p.source_op, op::VTX_16);
        assert_eq!(p.object_position, [e8(1), e8(2), e8(3)]);
        assert_eq!(p.clip, [e8(1), e8(2), e8(3), ONE]);
        assert!(e.explain_vertex(99).is_none());
    }

    #[test]
    fn explain_polygon_reports_primitive_attr_and_vertices() {
        let mut e = engine();
        e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7) | (7 << 24)]); // poly id 7
        e.execute(op::BEGIN_VTXS, &[0]);
        vtx(&mut e, 0, 0, 0);
        vtx(&mut e, 2, 0, 0);
        vtx(&mut e, 0, 2, 0);
        let p = e.explain_polygon(0).unwrap();
        assert_eq!(p.primitive, 0);
        assert_eq!(p.poly_id, 7);
        assert_eq!(p.vertices.len(), 3);
    }

    #[test]
    fn command_sequence_stamps_provenance_and_resets_on_swap() {
        let mut e = engine(); // MTX_MODE ran → seq 1
        begin(&mut e, 0); // POLYGON_ATTR seq 2, BEGIN_VTXS seq 3
        vtx(&mut e, 0, 0, 0); // seq 4
        vtx(&mut e, 2, 0, 0); // seq 5
        vtx(&mut e, 0, 2, 0); // seq 6 → triangle complete
        assert_eq!(e.explain_polygon(0).unwrap().command_seq, 3); // the BEGIN_VTXS seq
        e.execute(op::SWAP_BUFFERS, &[0]);
        assert_eq!(e.explain_vertex(0), None); // provenance cleared with the buffers
    }

    #[test]
    fn swap_buffers_resets_the_build_buffers() {
        let mut e = engine();
        begin(&mut e, 0);
        for i in 0..3 {
            vtx(&mut e, i, i, 0);
        }
        assert_eq!(e.ram_count() & 0xFFF, 1);
        e.execute(op::SWAP_BUFFERS, &[0]);
        assert_eq!(e.vertices().len(), 0);
        assert_eq!(e.ram_count(), 0);
    }
}
