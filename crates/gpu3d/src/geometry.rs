//! The geometry engine: current vertex state, the vertex-format decoders, primitive
//! assembly (triangles / quads / strips), and the vertex + polygon build buffers.
//! It owns the [`MatrixEngine`] and executes every geometry command — each submitted
//! vertex is transformed by the clip matrix into clip space and appended to vertex
//! RAM, and the primitive assembler groups vertices into polygons in polygon RAM.
//!
//! Everything is fixed-point: positions are 4.12 (`1.0 == 0x1000`), texcoords 1.11.4,
//! normals/deltas signed 10-bit. Clip coordinates come straight from the 4.12 clip
//! matrix.

use crate::command::op;
use crate::debug::{Polygon3dProvenance, Vertex3dProvenance};
use crate::matrix::{MatrixEngine, ONE};

/// Vertex RAM depth (GBATEK: 6144 vertices per frame).
const VERTEX_RAM: usize = 6144;
/// Polygon RAM depth (GBATEK: 2048 polygons per frame).
const POLYGON_RAM: usize = 2048;
/// Maximum vertices in a polygon: a triangle or quad plus one new vertex per frustum
/// plane after clipping (Phase 5).
pub const MAX_POLY_VERTS: usize = 10;

/// A geometry vertex after transform: clip-space position plus the attributes to
/// interpolate. Colors are 5-bit RGB (expanded later); texcoords are 1.11.4.
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

/// Sign-extend the low 10 bits of `v` (used by `VTX_10`, `VTX_DIFF`, `NORMAL`).
fn se10(v: u32) -> i32 {
    ((v as i32 & 0x3FF) << 22) >> 22
}

/// Sign-extend the low 16 bits of `v` to `i32` (used by the 16-bit vertex formats).
fn se16(v: u32) -> i32 {
    v as i16 as i32
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

/// Groups the stream of submitted vertices into polygons per the `BEGIN_VTXS`
/// primitive type. Returns a polygon's vertex indices as soon as one completes.
#[derive(Default)]
struct Assembler {
    prim: u8,
    /// Vertex-RAM indices submitted since `begin` (kept for strips; cleared after each
    /// complete polygon for lists).
    run: Vec<u16>,
}

impl Assembler {
    fn begin(&mut self, prim: u8) {
        self.prim = prim & 3;
        self.run.clear();
    }

    /// Record a submitted vertex; return the vertex indices of a polygon if one is now
    /// complete (`count` = 3 for triangles, 4 for quads).
    fn add(&mut self, vi: u16) -> Option<([u16; 4], u8)> {
        self.run.push(vi);
        let n = self.run.len();
        match self.prim {
            0 => (n == 3).then(|| {
                let p = [self.run[0], self.run[1], self.run[2], 0];
                self.run.clear();
                (p, 3)
            }),
            1 => (n == 4).then(|| {
                let p = [self.run[0], self.run[1], self.run[2], self.run[3]];
                self.run.clear();
                (p, 4)
            }),
            2 => (n >= 3).then(|| {
                // Triangle strip: triangle `n-3` uses the last three vertices, with
                // odd triangles swapping the first two to keep a consistent winding.
                let (a, b, c) = (self.run[n - 3], self.run[n - 2], self.run[n - 1]);
                let tri = n - 3;
                (if tri.is_multiple_of(2) { [a, b, c, 0] } else { [b, a, c, 0] }, 3)
            }),
            _ => (n >= 4 && n.is_multiple_of(2)).then(|| {
                // Quad strip: each quad reuses the previous quad's trailing edge; the
                // (…, n-4, n-3, n-1, n-2) order keeps the winding.
                ([self.run[n - 4], self.run[n - 3], self.run[n - 1], self.run[n - 2]], 4)
            }),
        }
    }
}

/// The geometry engine: matrix stack, current state, and the build buffers.
#[derive(Default)]
pub struct GeometryEngine {
    pub matrix: MatrixEngine,
    state: State,
    vertices: Vec<Vertex>,
    polygons: Vec<Polygon>,
    /// Provenance parallel to `vertices` (same index; cleared together on swap).
    vertex_origins: Vec<VertexOrigin>,
    assembler: Assembler,
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

    /// Transform the current position by the clip matrix and append it to vertex RAM
    /// (with its provenance), then feed the primitive assembler, emitting a polygon
    /// when one completes. `source_op` is the vertex command that produced it.
    fn submit_vertex(&mut self, source_op: u8) {
        if self.vertices.len() >= VERTEX_RAM {
            return;
        }
        let [x, y, z] = self.state.position;
        let clip = self.matrix.clip().transform([x, y, z, ONE]);
        let vi = self.vertices.len() as u16;
        self.vertices.push(Vertex {
            clip,
            color: self.state.color,
            texcoord: self.state.texcoord,
        });
        self.vertex_origins.push(VertexOrigin {
            source_op,
            command_seq: self.command_seq,
            object_position: self.state.position,
        });
        if let Some((verts, count)) = self.assembler.add(vi) {
            if self.polygons.len() < POLYGON_RAM {
                let mut v = [0u16; MAX_POLY_VERTS];
                v[..4].copy_from_slice(&verts);
                self.polygons.push(Polygon {
                    verts: v,
                    count,
                    attr: self.state.cur_attr,
                    tex_param: self.state.tex_param,
                    pltt_base: self.state.pltt_base,
                    primitive: self.assembler.prim,
                    begin_seq: self.state.begin_seq,
                });
            }
        }
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

    fn f(int: i32) -> i32 {
        int * ONE
    }

    /// Build an engine with an identity clip matrix (position mode).
    fn engine() -> GeometryEngine {
        let mut e = GeometryEngine::new();
        e.execute(op::MTX_MODE, &[1]); // position
        e
    }

    /// Submit a VTX_16 at integer coordinates.
    fn vtx16(e: &mut GeometryEngine, x: i32, y: i32, z: i32) {
        let lo = (f(x) as u32 & 0xFFFF) | ((f(y) as u32 & 0xFFFF) << 16);
        e.execute(op::VTX_16, &[lo, f(z) as u32 & 0xFFFF]);
    }

    #[test]
    fn vtx16_decodes_and_transforms_to_clip_space() {
        let mut e = engine();
        e.execute(op::MTX_TRANS, &[f(1) as u32, f(2) as u32, f(3) as u32]);
        e.execute(op::BEGIN_VTXS, &[0]); // triangles
        vtx16(&mut e, 1, 0, 0);
        // Identity projection * translated position: (1,0,0) + (1,2,3) = (2,2,3,1).
        assert_eq!(e.vertices()[0].clip, [f(2), f(2), f(3), f(1)]);
    }

    #[test]
    fn vtx10_expands_4p6_to_4p12() {
        let mut e = engine();
        e.execute(op::BEGIN_VTXS, &[0]);
        // x = 2.0 in 4.6 is 2<<6 = 128; y = -1.0 is (-1)<<6 = -64 (10-bit two's comp).
        let x = 128u32 & 0x3FF;
        let y = ((-64i32) as u32) & 0x3FF;
        e.execute(op::VTX_10, &[x | (y << 10)]);
        assert_eq!(e.vertices()[0].clip[0], f(2)); // 128<<6 = 0x2000 = 2.0
        assert_eq!(e.vertices()[0].clip[1], f(-1));
    }

    #[test]
    fn vtx_diff_adds_a_small_delta_to_the_previous_position() {
        let mut e = engine();
        e.execute(op::BEGIN_VTXS, &[0]);
        vtx16(&mut e, 1, 1, 1);
        // Delta of +5 (raw 4.12 units) on x only.
        e.execute(op::VTX_DIFF, &[5]);
        assert_eq!(e.vertices()[1].clip[0], f(1) + 5);
        assert_eq!(e.vertices()[1].clip[1], f(1));
    }

    #[test]
    fn partial_vtx_updates_keep_the_other_components() {
        let mut e = engine();
        e.execute(op::BEGIN_VTXS, &[0]);
        vtx16(&mut e, 1, 2, 3);
        // VTX_XY changes x,y; z stays 3.
        let xy = (f(4) as u32 & 0xFFFF) | ((f(5) as u32 & 0xFFFF) << 16);
        e.execute(op::VTX_XY, &[xy]);
        assert_eq!(e.vertices()[1].clip, [f(4), f(5), f(3), f(1)]);
    }

    #[test]
    fn triangle_list_groups_every_three_vertices() {
        let mut e = engine();
        e.execute(op::BEGIN_VTXS, &[0]); // triangles
        for i in 0..6 {
            vtx16(&mut e, i, 0, 0);
        }
        assert_eq!(e.polygons().len(), 2);
        assert_eq!(&e.polygons()[0].verts[..3], &[0, 1, 2]);
        assert_eq!(&e.polygons()[1].verts[..3], &[3, 4, 5]);
        assert!(e.polygons().iter().all(|p| p.count == 3));
    }

    #[test]
    fn quad_list_groups_every_four_vertices() {
        let mut e = engine();
        e.execute(op::BEGIN_VTXS, &[1]); // quads
        for i in 0..4 {
            vtx16(&mut e, i, 0, 0);
        }
        assert_eq!(e.polygons().len(), 1);
        assert_eq!(&e.polygons()[0].verts[..4], &[0, 1, 2, 3]);
        assert_eq!(e.polygons()[0].count, 4);
    }

    #[test]
    fn triangle_strip_shares_vertices_with_alternating_winding() {
        let mut e = engine();
        e.execute(op::BEGIN_VTXS, &[2]); // triangle strip
        for i in 0..5 {
            vtx16(&mut e, i, 0, 0);
        }
        // 5 vertices → 3 triangles: (0,1,2), (2,1,3) [odd, swapped], (2,3,4).
        assert_eq!(e.polygons().len(), 3);
        assert_eq!(&e.polygons()[0].verts[..3], &[0, 1, 2]);
        assert_eq!(&e.polygons()[1].verts[..3], &[2, 1, 3]);
        assert_eq!(&e.polygons()[2].verts[..3], &[2, 3, 4]);
    }

    #[test]
    fn quad_strip_pairs_new_vertices_with_the_trailing_edge() {
        let mut e = engine();
        e.execute(op::BEGIN_VTXS, &[3]); // quad strip
        for i in 0..6 {
            vtx16(&mut e, i, 0, 0);
        }
        // 6 vertices → 2 quads: (0,1,3,2) and (2,3,5,4).
        assert_eq!(e.polygons().len(), 2);
        assert_eq!(&e.polygons()[0].verts[..4], &[0, 1, 3, 2]);
        assert_eq!(&e.polygons()[1].verts[..4], &[2, 3, 5, 4]);
    }

    #[test]
    fn polygon_attr_latches_at_begin_and_color_rides_the_vertex() {
        let mut e = engine();
        e.execute(op::COLOR, &[0x1F | (0x1F << 10)]); // magenta (r=31,b=31)
        e.execute(op::POLYGON_ATTR, &[0xDEAD_BEEF]);
        e.execute(op::BEGIN_VTXS, &[0]);
        for i in 0..3 {
            vtx16(&mut e, i, 0, 0);
        }
        assert_eq!(e.polygons()[0].attr, 0xDEAD_BEEF);
        assert_eq!(e.vertices()[0].color, [31, 0, 31]);
    }

    #[test]
    fn explain_vertex_reports_source_command_and_object_position() {
        let mut e = engine();
        e.execute(op::MTX_TRANS, &[f(10) as u32, 0, 0]); // clip = translate(+10x)
        e.execute(op::BEGIN_VTXS, &[0]);
        vtx16(&mut e, 1, 2, 3);
        let p = e.explain_vertex(0).unwrap();
        assert_eq!(p.index, 0);
        assert_eq!(p.source_op, op::VTX_16);
        assert_eq!(p.object_position, [f(1), f(2), f(3)]); // pre-transform
        assert_eq!(p.clip, [f(11), f(2), f(3), f(1)]); // post-transform (translated x)
        assert!(e.explain_vertex(5).is_none());
    }

    #[test]
    fn explain_polygon_reports_primitive_attr_and_vertices() {
        let mut e = engine();
        e.execute(op::POLYGON_ATTR, &[(7 << 24) | 0x1234]); // poly id 7
        e.execute(op::BEGIN_VTXS, &[2]); // triangle strip
        for i in 0..4 {
            vtx16(&mut e, i, 0, 0);
        }
        let p = e.explain_polygon(1).unwrap();
        assert_eq!(p.primitive, 2);
        assert_eq!(p.poly_id, 7);
        assert_eq!(p.attr, (7 << 24) | 0x1234);
        assert_eq!(p.vertices, vec![2, 1, 3]); // second strip triangle (odd winding)
    }

    #[test]
    fn command_sequence_stamps_provenance_and_resets_on_swap() {
        let mut e = engine(); // engine() already ran one command (MTX_MODE) → seq 1
        e.execute(op::BEGIN_VTXS, &[0]); // seq 2
        vtx16(&mut e, 0, 0, 0); // seq 3
        assert_eq!(e.explain_vertex(0).unwrap().command_seq, 3);
        assert_eq!(e.explain_polygon(0), None); // only one vertex so far
        vtx16(&mut e, 1, 0, 0); // seq 4
        vtx16(&mut e, 2, 0, 0); // seq 5 → triangle complete
        assert_eq!(e.explain_polygon(0).unwrap().command_seq, 2); // the BEGIN_VTXS seq
        e.execute(op::SWAP_BUFFERS, &[0]);
        assert_eq!(e.explain_vertex(0), None); // provenance cleared with the buffers
    }

    #[test]
    fn swap_buffers_resets_the_build_buffers() {
        let mut e = engine();
        e.execute(op::BEGIN_VTXS, &[0]);
        for i in 0..3 {
            vtx16(&mut e, i, 0, 0);
        }
        assert_eq!(e.ram_count() & 0xFFF, 1); // one polygon
        e.execute(op::SWAP_BUFFERS, &[0]);
        assert_eq!(e.vertices().len(), 0);
        assert_eq!(e.polygons().len(), 0);
        assert_eq!(e.ram_count(), 0);
    }
}
