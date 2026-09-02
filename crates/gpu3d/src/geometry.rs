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
/// attributes to interpolate. Colors are 6-bit RGB; texcoords 1.11.4.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vertex {
    /// Clip-space `(x, y, z, w)` in 4.12 (before the perspective divide).
    pub clip: [i32; 4],
    /// Vertex color, 6-bit per channel — `COLOR` (5-bit expanded) or lighting output.
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

/// Build the full provenance of vertex `i` from a vertex slice and its parallel origins.
fn explain_vertex_from(
    vertices: &[Vertex],
    origins: &[VertexOrigin],
    i: usize,
) -> Option<Vertex3dProvenance> {
    let v = vertices.get(i)?;
    let o = origins.get(i)?;
    Some(Vertex3dProvenance {
        index: i as u16,
        source_op: o.source_op,
        command_seq: o.command_seq,
        object_position: o.object_position,
        clip: v.clip,
    })
}

/// Build the full provenance of polygon `i` from a polygon slice.
fn explain_polygon_from(polygons: &[Polygon], i: usize) -> Option<Polygon3dProvenance> {
    let p = polygons.get(i)?;
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

/// A sealed, immutable snapshot of one frame's geometry — the seam between the
/// geometry engine and the rasterizer. `SWAP_BUFFERS` moves the build buffer here; the
/// rasterizer (Phase 8) consumes it. Carries the swap parameter (depth mode and
/// translucent sort) and its own provenance so a rendered pixel is explainable.
#[derive(Default)]
pub struct RenderList {
    vertices: Vec<Vertex>,
    polygons: Vec<Polygon>,
    vertex_origins: Vec<VertexOrigin>,
    /// The `SWAP_BUFFERS` parameter (bit 0 = manual translucent sort, bit 1 = W-buffer).
    pub swap_flags: u32,
    /// The `VIEWPORT` parameter (x1|y1<<8|x2<<16|y2<<24), or 0 for full-screen.
    pub viewport: u32,
    /// Increments each swap — the sealed frame's index.
    pub frame: u64,
}

impl RenderList {
    pub fn vertices(&self) -> &[Vertex] {
        &self.vertices
    }
    pub fn polygons(&self) -> &[Polygon] {
        &self.polygons
    }
    /// Depth buffering uses the W value (vs Z) — `SWAP_BUFFERS` bit 1.
    pub fn w_buffer(&self) -> bool {
        self.swap_flags & (1 << 1) != 0
    }
    /// Translucent polygons are manually sorted (vs auto by depth) — bit 0.
    pub fn manual_sort(&self) -> bool {
        self.swap_flags & 1 != 0
    }
    pub fn explain_vertex(&self, i: usize) -> Option<Vertex3dProvenance> {
        explain_vertex_from(&self.vertices, &self.vertex_origins, i)
    }
    pub fn explain_polygon(&self, i: usize) -> Option<Polygon3dProvenance> {
        explain_polygon_from(&self.polygons, i)
    }
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
                self.color[0].clamp(0, 63) as u8,
                self.color[1].clamp(0, 63) as u8,
                self.color[2].clamp(0, 63) as u8,
            ],
            texcoord: self.texcoord,
        }
    }
}

/// One of the four directional lights: its transformed direction and half vector
/// (1.9 fixed, `1.0 == 512`) and its color (6-bit per channel).
#[derive(Clone, Copy, Default)]
struct Light {
    dir: [i32; 3],
    half: [i32; 3],
    color: [i32; 3],
}

/// Material colors, the shininess table, and the four lights — the lighting state
/// sampled by `NORMAL`. Colors are 6-bit (0-63); levels use a 1/512 fixed scale.
struct Lighting {
    emission: [i32; 3],
    diffuse: [i32; 3],
    ambient: [i32; 3],
    specular: [i32; 3],
    shininess: [u8; 128],
    use_table: bool,
    lights: [Light; 4],
}

impl Default for Lighting {
    fn default() -> Self {
        Lighting {
            emission: [0; 3],
            diffuse: [0; 3],
            ambient: [0; 3],
            specular: [0; 3],
            shininess: [0; 128],
            use_table: false,
            lights: [Light::default(); 4],
        }
    }
}

/// Dot product of two 3-vectors in `i64` (vector components are ~1.9, so the product
/// is 1/512² scale).
fn dot3(a: &[i32; 3], b: &[i32; 3]) -> i64 {
    a[0] as i64 * b[0] as i64 + a[1] as i64 * b[1] as i64 + a[2] as i64 * b[2] as i64
}

/// Expand a 5-bit color channel to 6 bits per GBATEK (`0 -> 0`, else `c·2+1`; so
/// `31 -> 63`, `1 -> 3`).
fn expand6(c5: u32) -> i32 {
    let c = (c5 & 0x1F) as i32;
    if c == 0 {
        0
    } else {
        c * 2 + 1
    }
}

/// Unpack an `RGB555` (bits 0-14 of `word`) to 6-bit `[r, g, b]`.
fn unpack_rgb6(word: u32) -> [i32; 3] {
    [expand6(word), expand6(word >> 5), expand6(word >> 10)]
}

/// Narrow a 6-bit color triple to the stored `[u8; 3]` (already in range).
fn color6(c: [i32; 3]) -> [u8; 3] {
    [c[0] as u8, c[1] as u8, c[2] as u8]
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
    /// The `VIEWPORT` rectangle, sealed into the render list at swap.
    viewport: u32,
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
    lighting: Lighting,
    vertices: Vec<Vertex>,
    polygons: Vec<Polygon>,
    /// Provenance parallel to `vertices` (same index; cleared together on swap).
    vertex_origins: Vec<VertexOrigin>,
    /// The sealed snapshot of the previous frame's geometry (the rasterizer's input).
    render_list: RenderList,
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
    /// Debug lifetime counters over `finalize_polygon`: primitives submitted, dropped by
    /// clipping (< 3 survivors), dropped by winding culling, and emitted.
    dbg_submitted: u64,
    dbg_clipped: u64,
    dbg_culled: u64,
    dbg_emitted: u64,
    /// The last `BOX_TEST` result (GXSTAT bit 1): the tested cuboid is inside the view.
    box_test_result: bool,
    /// Debug lifetime counters: total `BOX_TEST`s run, and how many reported "inside".
    dbg_box_tests: u64,
    dbg_box_pass: u64,
    /// Debug: build-buffer `(polys, verts)` observed at the moment of the last swap,
    /// before it was sealed — distinguishes "swap saw an empty buffer" from a sealing bug.
    dbg_last_swap_build: (usize, usize),
    /// Debug: lifetime count of `emit_polygon` calls dropped by the RAM-full guard.
    dbg_emit_dropped: u64,
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
        // Bit 1 is the BOX_TEST result; bit 0 (test busy) stays 0 — tests run
        // synchronously here, so a game polling for "ready" sees it immediately.
        self.matrix.gxstat_bits() | ((self.box_test_result as u32) << 1)
    }

    /// Debug lifetime `(submitted, clipped_out, culled, emitted)` primitive counts.
    pub fn pipeline_stats(&self) -> (u64, u64, u64, u64) {
        (self.dbg_submitted, self.dbg_clipped, self.dbg_culled, self.dbg_emitted)
    }

    /// Debug lifetime `(box_tests_run, box_tests_passed)` — how many occlusion
    /// queries a game issued, and how many we reported as "inside the view".
    pub fn box_test_stats(&self) -> (u64, u64) {
        (self.dbg_box_tests, self.dbg_box_pass)
    }

    /// Debug: build buffer `(polys, verts)` seen at the last swap, and the lifetime
    /// count of polygons dropped by the vertex/polygon-RAM-full guard.
    pub fn swap_debug(&self) -> ((usize, usize), u64) {
        (self.dbg_last_swap_build, self.dbg_emit_dropped)
    }

    /// Debug: the current (not-yet-sealed) build buffer `(polys, verts)`.
    pub fn build_len(&self) -> (usize, usize) {
        (self.polygons.len(), self.vertices.len())
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

            COLOR => self.state.color = color6(unpack_rgb6(params[0])),
            TEXCOORD => {
                let raw = [se16(params[0]), se16(params[0] >> 16)];
                // Texcoord-transform mode 1 ("TexCoord source", TEXIMAGE_PARAM bits
                // 30-31 = 1) multiplies the coordinate by the texture matrix here, at
                // the TEXCOORD command — this is how games scale/scroll/rotate texture
                // coordinates. Modes 0/2/3 are not transformed here (2/3 = env/vertex
                // mapping, computed at NORMAL/VTX time — not yet modelled).
                self.state.texcoord = if (self.state.tex_param >> 30) & 3 == 1 {
                    self.transform_texcoord(raw[0], raw[1])
                } else {
                    raw
                };
            }

            DIF_AMB => {
                let p = params[0];
                self.lighting.diffuse = unpack_rgb6(p);
                self.lighting.ambient = unpack_rgb6(p >> 16);
                if p & (1 << 15) != 0 {
                    // Bit 15: also set the diffuse color as the current vertex color.
                    self.state.color = color6(self.lighting.diffuse);
                }
            }
            SPE_EMI => {
                let p = params[0];
                self.lighting.specular = unpack_rgb6(p);
                self.lighting.emission = unpack_rgb6(p >> 16);
                self.lighting.use_table = p & (1 << 15) != 0;
            }
            SHININESS => {
                for (i, &w) in params.iter().enumerate().take(32) {
                    for b in 0..4 {
                        self.lighting.shininess[i * 4 + b] = (w >> (b * 8)) as u8;
                    }
                }
            }
            LIGHT_VECTOR => {
                let p = params[0];
                let dir = self.transform_direction([se10(p), se10(p >> 10), se10(p >> 20)]);
                let num = ((p >> 30) & 3) as usize;
                self.lighting.lights[num].dir = dir;
                // Half vector = (direction + line-of-sight (0,0,-1)) / 2, in 1.9.
                self.lighting.lights[num].half = [dir[0] / 2, dir[1] / 2, (dir[2] - 512) / 2];
            }
            LIGHT_COLOR => {
                let p = params[0];
                self.lighting.lights[((p >> 30) & 3) as usize].color = unpack_rgb6(p);
            }
            NORMAL => {
                let p = params[0];
                let raw = [se10(p), se10(p >> 10), se10(p >> 20)];
                // Texcoord-transform mode 2 ("Normal source") derives the texcoord from
                // the normal here, at the NORMAL command.
                if (self.state.tex_param >> 30) & 3 == 2 {
                    self.state.texcoord = self.transform_texcoord_normal(raw);
                }
                self.compute_lighting(raw);
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
            VIEWPORT => self.state.viewport = params[0],
            END_VTXS => {}
            SWAP_BUFFERS => self.swap_buffers(params[0]),

            // BOX_TEST: games skip drawing objects whose bounding box is outside the
            // view (occlusion culling). Without it, GXSTAT's result bit stays 0 and the
            // game skips *everything*. POS/VEC_TEST readbacks are still later work.
            BOX_TEST => {
                self.box_test_result = self.box_test(params);
                self.dbg_box_tests += 1;
                self.dbg_box_pass += self.box_test_result as u64;
            }
            _ => {}
        }
    }

    /// `BOX_TEST`: whether a bounding cuboid is (partly or fully) inside the view
    /// volume. The cuboid corner `(x,y,z)` and size `(w,h,d)` come as three packed
    /// words (1.3.12 fixed, treated as the 4.12 vertex scale). Its 8 corners are pushed
    /// through the clip matrix; it is outside only if all 8 lie beyond one frustum
    /// plane (a conservative AABB test — never reports a visible box as hidden).
    fn box_test(&self, p: &[u32]) -> bool {
        let (x, y) = (se16(p[0]), se16(p[0] >> 16));
        let (z, w) = (se16(p[1]), se16(p[1] >> 16));
        let (h, d) = (se16(p[2]), se16(p[2] >> 16));
        let m = self.matrix.clip();
        let mut corners = [[0i32; 4]; 8];
        let mut i = 0;
        for &dx in &[0, w] {
            for &dy in &[0, h] {
                for &dz in &[0, d] {
                    corners[i] = m.transform([x + dx, y + dy, z + dz, ONE]);
                    i += 1;
                }
            }
        }
        // Fully outside iff every corner is beyond some single frustum plane.
        !(0..6).any(|plane| corners.iter().all(|c| plane_value(c, plane) < 0))
    }

    /// Texcoord-transform mode 1 ("TexCoord source"): the texture matrix scales/rotates/
    /// scrolls the coordinate at the TEXCOORD command. GBATEK:
    /// `(S' T') = (S T 1/16 1/16) · left-two-columns(TexMtx)`. That row-vector×matrix
    /// product reads `S'` from `m[0],m[4],m[8],m[12]` and `T'` from `m[1],m[5],m[9],m[13]`
    /// — the same `M·v` convention as the position transform, NOT the transpose. `1/16`
    /// is `1` in 1.11.4 (a texel is 16); the 12-bit-fraction matrix shifts the result back.
    fn transform_texcoord(&self, s: i32, t: i32) -> [i32; 2] {
        let m = &self.matrix.texture().m;
        let (s, t) = (s as i64, t as i64);
        let sp = (s * m[0] as i64 + t * m[4] as i64 + m[8] as i64 + m[12] as i64) >> 12;
        let tp = (s * m[1] as i64 + t * m[5] as i64 + m[9] as i64 + m[13] as i64) >> 12;
        [sp as i32, tp as i32]
    }

    /// Texcoord-transform mode 3 ("Vertex source"): the coordinate comes from the vertex
    /// position × texture matrix at each VTX. GBATEK replaces the matrix's bottom row with
    /// the current TexCoord: `(S' T') = (Vx Vy Vz 1.0) · [m rows 0-2 ; (S T)]`. The vertex
    /// is 1.3.12 and the matrix 1.19.12, so the product carries 24 fraction bits → shift
    /// to the 4-bit texcoord fraction, then add the current TexCoord.
    fn transform_texcoord_vertex(&self, v: [i32; 3]) -> [i32; 2] {
        let m = &self.matrix.texture().m;
        let (vx, vy, vz) = (v[0] as i64, v[1] as i64, v[2] as i64);
        let sp = ((vx * m[0] as i64 + vy * m[4] as i64 + vz * m[8] as i64) >> 20) + self.state.texcoord[0] as i64;
        let tp = ((vx * m[1] as i64 + vy * m[5] as i64 + vz * m[9] as i64) >> 20) + self.state.texcoord[1] as i64;
        [sp as i32, tp as i32]
    }

    /// Texcoord-transform mode 2 ("Normal source"): spherical reflection mapping (skyboxes,
    /// shiny surfaces). GBATEK, with the bottom row replaced by the current TexCoord:
    /// `(S' T') = (Nx Ny Nz 1.0) · [m rows 0-2 ; (S T)]`. The normal is 1.0.9 and the matrix
    /// 1.19.12, so the product carries 21 fraction bits → shift to the 4-bit texcoord
    /// fraction, then add the current TexCoord. Same column indexing as mode 1/3.
    fn transform_texcoord_normal(&self, n: [i32; 3]) -> [i32; 2] {
        let m = &self.matrix.texture().m;
        let (nx, ny, nz) = (n[0] as i64, n[1] as i64, n[2] as i64);
        let sp = ((nx * m[0] as i64 + ny * m[4] as i64 + nz * m[8] as i64) >> 17) + self.state.texcoord[0] as i64;
        let tp = ((nx * m[1] as i64 + ny * m[5] as i64 + nz * m[9] as i64) >> 17) + self.state.texcoord[1] as i64;
        [sp as i32, tp as i32]
    }

    /// Transform a direction (normal or light vector) by the 3×3 of the directional
    /// (vector) matrix, keeping it in 1.9 fixed-point (`>> 12`).
    fn transform_direction(&self, v: [i32; 3]) -> [i32; 3] {
        let m = &self.matrix.vector().m;
        let mut out = [0i32; 3];
        for (i, o) in out.iter_mut().enumerate() {
            let mut acc = 0i64;
            for (j, &vj) in v.iter().enumerate() {
                acc += m[j * 4 + i] as i64 * vj as i64;
            }
            *o = (acc >> 12) as i32;
        }
        out
    }

    /// `NORMAL`: transform the normal by the directional matrix and compute the vertex
    /// color from emission + each enabled light's ambient/diffuse/specular terms
    /// (GBATEK formula). Colors are 6-bit; levels are 1/512 fixed and combine as
    /// `(material·light >> 6) · level >> 9`.
    fn compute_lighting(&mut self, normal_raw: [i32; 3]) {
        let n = self.transform_direction(normal_raw);
        let attr = self.state.cur_attr;
        let mut color = self.lighting.emission;
        for i in 0..4 {
            if attr & (1 << i) == 0 {
                continue; // light i disabled by POLYGON_ATTR
            }
            let light = &self.lighting.lights[i];
            let diffuse_level = ((-dot3(&light.dir, &n)) >> 9).clamp(0, 512) as i32;
            let shine_dot = ((-dot3(&light.half, &n)) >> 9).clamp(0, 512) as i32;
            let mut shine_level = (shine_dot * shine_dot) >> 9; // (-H·N)^2, in 1/512
            if self.lighting.use_table {
                let idx = ((shine_level >> 2) as usize).min(127);
                shine_level = (self.lighting.shininess[idx] as i32) << 1; // 8-bit → 1/512
            }
            for (c, chan) in color.iter_mut().enumerate() {
                let lc = light.color[c];
                *chan += ((self.lighting.ambient[c] * lc) >> 6)
                    + ((((self.lighting.diffuse[c] * lc) >> 6) * diffuse_level) >> 9)
                    + ((((self.lighting.specular[c] * lc) >> 6) * shine_level) >> 9);
            }
        }
        self.state.color = [
            color[0].clamp(0, 63) as u8,
            color[1].clamp(0, 63) as u8,
            color[2].clamp(0, 63) as u8,
        ];
    }

    /// Transform the current position by the clip matrix into a clip-space vertex and
    /// feed the primitive assembler; a completed primitive is finalized (clipped,
    /// culled, emitted). `source_op` is the vertex command that produced it.
    fn submit_vertex(&mut self, source_op: u8) {
        let [x, y, z] = self.state.position;
        let clip = self.matrix.clip().transform([x, y, z, ONE]);
        // Texcoord-transform mode 3 ("Vertex source") derives the texcoord from this
        // vertex's position; modes 0/1/2 use the current (already-resolved) texcoord.
        let texcoord = if (self.state.tex_param >> 30) & 3 == 3 {
            self.transform_texcoord_vertex([x, y, z])
        } else {
            self.state.texcoord
        };
        let cv = ClipVertex {
            clip,
            color: [
                self.state.color[0] as i32,
                self.state.color[1] as i32,
                self.state.color[2] as i32,
            ],
            texcoord,
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
        self.dbg_submitted += 1;
        if src.len() < 3 {
            self.dbg_clipped += 1;
        } else if culled(self.state.cur_attr, &src) {
            self.dbg_culled += 1;
        } else {
            self.dbg_emitted += 1;
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
            self.dbg_emit_dropped += 1;
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

    /// Provenance of a build-buffer vertex (the frame being assembled).
    pub fn explain_vertex(&self, i: usize) -> Option<Vertex3dProvenance> {
        explain_vertex_from(&self.vertices, &self.vertex_origins, i)
    }

    /// Provenance of a build-buffer polygon (the frame being assembled).
    pub fn explain_polygon(&self, i: usize) -> Option<Polygon3dProvenance> {
        explain_polygon_from(&self.polygons, i)
    }

    /// The sealed render list (the previous frame's geometry) the rasterizer consumes.
    pub fn render_list(&self) -> &RenderList {
        &self.render_list
    }

    /// `SWAP_BUFFERS`: seal the build buffer into the render list (moving the vertices,
    /// polygons, and their provenance) and reset the build buffer for the next frame.
    /// On hardware the swap waits for V-blank; here it takes effect on the command
    /// (still one frame ahead of the rasterizer, which reads the sealed list).
    fn swap_buffers(&mut self, param: u32) {
        self.dbg_last_swap_build = (self.polygons.len(), self.vertices.len());
        self.peak.0 = self.peak.0.max(self.polygons.len());
        self.peak.1 = self.peak.1.max(self.vertices.len());
        self.render_list.vertices = std::mem::take(&mut self.vertices);
        self.render_list.polygons = std::mem::take(&mut self.polygons);
        self.render_list.vertex_origins = std::mem::take(&mut self.vertex_origins);
        self.render_list.swap_flags = param;
        self.render_list.viewport = self.state.viewport;
        self.render_list.frame += 1;
        self.assembler.run.clear();
        self.command_seq = 0;
    }
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
    fn box_test_reports_inside_and_outside_the_frustum() {
        // With the identity clip matrix, a box near the origin is inside the view and a
        // box translated past the right plane is outside. GXSTAT bit 1 carries the result.
        let mut e = GeometryEngine::new();
        let pack = |a: i32, b: i32| (a as u32 & 0xFFFF) | ((b as u32 & 0xFFFF) << 16);
        let half = ONE / 2;
        e.execute(op::BOX_TEST, &[pack(0, 0), pack(0, half), pack(half, half)]);
        assert_ne!(e.gxstat_bits() & (1 << 1), 0, "box at the origin is inside the view");
        // Corner x = 2.0 with size 1.0 → entirely beyond the right plane (w = 1.0).
        e.execute(op::BOX_TEST, &[pack(2 * ONE, 0), pack(0, ONE), pack(ONE, ONE)]);
        assert_eq!(e.gxstat_bits() & (1 << 1), 0, "box past the right plane is outside");
    }

    #[test]
    fn texcoord_transform_mode1_scales_by_the_texture_matrix() {
        // A game scales its texcoords via the texture matrix (mode 1). Without applying
        // it, small texcoords sample the wrong texels (SM64's stars rendered black).
        let mut e = GeometryEngine::new();
        e.execute(op::MTX_MODE, &[3]); // texture matrix
        e.execute(op::MTX_IDENTITY, &[]);
        e.execute(op::MTX_SCALE, &[8 * ONE as u32, 8 * ONE as u32, ONE as u32]); // ×8
        e.execute(op::MTX_MODE, &[1]); // back to position mode for the geometry
        e.execute(op::TEXIMAGE_PARAM, &[1 << 30]); // texcoord-transform mode 1
        e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7)]);
        e.execute(op::BEGIN_VTXS, &[0]);
        e.execute(op::TEXCOORD, &[16 | (16 << 16)]); // s=t=16 (1.11.4 = 1 texel)
        for (x, y) in [(0i32, 0i32), (2, 0), (0, 2)] {
            let lo = (e8(x) as u32 & 0xFFFF) | ((e8(y) as u32 & 0xFFFF) << 16);
            e.execute(op::VTX_16, &[lo, 0]);
        }
        // The texture matrix scaled the coordinate ×8: 16 → 128 (8 texels).
        assert_eq!(e.vertices()[0].texcoord, [128, 128]);

        // Mode 0 (no transform) leaves the raw coordinate.
        e.execute(op::TEXIMAGE_PARAM, &[0]);
        e.execute(op::BEGIN_VTXS, &[0]);
        e.execute(op::TEXCOORD, &[16 | (16 << 16)]);
        for (x, y) in [(0i32, 0i32), (2, 0), (0, 2)] {
            let lo = (e8(x) as u32 & 0xFFFF) | ((e8(y) as u32 & 0xFFFF) << 16);
            e.execute(op::VTX_16, &[lo, 0]);
        }
        assert_eq!(e.vertices()[3].texcoord, [16, 16]);
    }

    #[test]
    fn texcoord_transform_uses_the_correct_matrix_columns() {
        // An off-diagonal texture matrix distinguishes M·v from its transpose. With m[4]
        // (column 1, row 0) set, T must feed into S'; the transposed indexing fed it into
        // T' instead, swapping the axes — which warped env-mapped skies (mode 2).
        let mut e = GeometryEngine::new();
        e.execute(op::MTX_MODE, &[3]); // texture matrix
        let o = ONE as u32;
        e.execute(op::MTX_LOAD_4X4, &[o, 0, 0, 0, 2 * o, o, 0, 0, 0, 0, o, 0, 0, 0, 0, o]);
        e.execute(op::MTX_MODE, &[1]);
        e.execute(op::TEXIMAGE_PARAM, &[1 << 30]); // texcoord-transform mode 1
        e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7)]);
        e.execute(op::BEGIN_VTXS, &[0]);
        e.execute(op::TEXCOORD, &[16 | (16 << 16)]); // S = T = 16
        for (x, y) in [(0i32, 0), (2, 0), (0, 2)] {
            let lo = (e8(x) as u32 & 0xFFFF) | ((e8(y) as u32 & 0xFFFF) << 16);
            e.execute(op::VTX_16, &[lo, 0]);
        }
        // S' = S·m[0] + T·m[4] = 16·1 + 16·2 = 48; T' = S·m[1] + T·m[5] = 0 + 16 = 16.
        assert_eq!(e.vertices()[0].texcoord, [48, 16]);
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
        assert_eq!(e.vertices()[0].color, [63, 0, 63]); // 5-bit 31 → 6-bit 63
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
        assert!(reds.iter().any(|&r| r > 0 && r < 63), "an interpolated red between 0 and 63: {reds:?}");
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

    /// Complete an in-frustum triangle and return its first vertex's (lit) color.
    fn triangle_color(e: &mut GeometryEngine) -> [u8; 3] {
        vtx(e, 0, 0, 0);
        vtx(e, 2, 0, 0);
        vtx(e, 0, 2, 0);
        e.vertices()[0].color
    }

    #[test]
    fn lighting_emission_is_the_baseline_without_lights() {
        let mut e = engine();
        e.execute(op::SPE_EMI, &[0x1F << 16]); // emission red = 31 → 63
        e.execute(op::POLYGON_ATTR, &[(1 << 6) | (1 << 7)]); // render both, no lights
        e.execute(op::BEGIN_VTXS, &[0]);
        e.execute(op::NORMAL, &[0]);
        assert_eq!(triangle_color(&mut e), [63, 0, 0]);
    }

    #[test]
    fn lighting_diffuse_responds_to_normal_orientation() {
        // Light 0 along -Z, red diffuse material and red light.
        let setup = |e: &mut GeometryEngine, normal: u32| {
            let z = ((-256i32) as u32) & 0x3FF;
            e.execute(op::LIGHT_VECTOR, &[z << 20]); // light 0 dir (0,0,-256)
            e.execute(op::LIGHT_COLOR, &[0x1F]); // red
            e.execute(op::DIF_AMB, &[0x1F]); // diffuse red, ambient 0
            e.execute(op::SPE_EMI, &[0]); // no specular/emission
            e.execute(op::POLYGON_ATTR, &[1 | (1 << 6) | (1 << 7)]); // light 0 + render both
            e.execute(op::BEGIN_VTXS, &[0]);
            e.execute(op::NORMAL, &[normal]);
        };
        let mut facing = engine();
        setup(&mut facing, (256u32 & 0x3FF) << 20); // normal +Z, toward the light
        let mut perp = engine();
        setup(&mut perp, 256u32 & 0x3FF); // normal +X, perpendicular
        assert!(triangle_color(&mut facing)[0] > 0, "diffuse when facing the light");
        assert_eq!(triangle_color(&mut perp)[0], 0, "no diffuse when perpendicular");
    }

    #[test]
    fn lighting_ambient_adds_regardless_of_normal() {
        let mut e = engine();
        e.execute(op::LIGHT_COLOR, &[0x1F]); // light 0 red
        e.execute(op::DIF_AMB, &[0x1F << 16]); // ambient red, diffuse 0
        e.execute(op::SPE_EMI, &[0]);
        e.execute(op::POLYGON_ATTR, &[1 | (1 << 6) | (1 << 7)]);
        e.execute(op::BEGIN_VTXS, &[0]);
        e.execute(op::NORMAL, &[0]);
        assert!(triangle_color(&mut e)[0] > 0, "ambient always contributes");
    }

    #[test]
    fn dif_amb_bit15_sets_the_vertex_color() {
        let mut e = engine();
        e.execute(op::DIF_AMB, &[0x1F | (1 << 15)]); // diffuse red + set-vertex-color
        begin(&mut e, 0);
        assert_eq!(triangle_color(&mut e), [63, 0, 0]); // no NORMAL: color from bit 15
    }

    #[test]
    fn lighting_specular_uses_the_shininess_table() {
        let mut e = engine();
        let z = ((-256i32) as u32) & 0x3FF;
        e.execute(op::LIGHT_VECTOR, &[z << 20]); // light 0 along -Z
        e.execute(op::LIGHT_COLOR, &[0x7FFF]); // white
        e.execute(op::SPE_EMI, &[0x7FFF | (1 << 15)]); // specular white + table enable
        e.execute(op::SHININESS, &[0xFFFF_FFFF; 32]); // table saturated
        e.execute(op::POLYGON_ATTR, &[1 | (1 << 6) | (1 << 7)]);
        e.execute(op::BEGIN_VTXS, &[0]);
        e.execute(op::NORMAL, &[(256u32 & 0x3FF) << 20]); // normal +Z
        assert!(triangle_color(&mut e)[0] > 0, "specular via the table contributes");
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

    #[test]
    fn swap_buffers_seals_geometry_into_the_render_list() {
        let mut e = engine();
        begin(&mut e, 0);
        vtx(&mut e, 0, 0, 0);
        vtx(&mut e, 2, 0, 0);
        vtx(&mut e, 0, 2, 0);
        // Before the swap: geometry is in the build buffer, the render list is empty.
        assert_eq!(e.polygons().len(), 1);
        assert_eq!(e.render_list().polygons().len(), 0);

        e.execute(op::SWAP_BUFFERS, &[0b10]); // W-buffer depth mode

        // After: the build buffer reset, the render list holds the sealed frame.
        assert_eq!(e.polygons().len(), 0);
        let rl = e.render_list();
        assert_eq!(rl.polygons().len(), 1);
        assert_eq!(rl.vertices().len(), 3);
        assert!(rl.w_buffer() && !rl.manual_sort());
        assert_eq!(rl.frame, 1);
        // Provenance travels into the sealed list.
        assert_eq!(rl.explain_vertex(0).unwrap().source_op, op::VTX_16);
        assert_eq!(rl.explain_polygon(0).unwrap().primitive, 0);

        // A second swap advances the frame counter.
        e.execute(op::SWAP_BUFFERS, &[0]);
        assert_eq!(e.render_list().frame, 2);
        assert_eq!(e.render_list().polygons().len(), 0); // empty build buffer sealed
    }
}

