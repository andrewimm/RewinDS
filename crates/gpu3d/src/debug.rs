//! Provenance for the 3D pipeline — the data that answers "why is this pixel /
//! polygon / vertex what it is", and the sink that funnels it.
//!
//! Mirrors `video2d`'s `ProvenanceSink`: heavy payloads pass as `FnOnce` closures so
//! the fast path ([`NullSink`]) drops them unbuilt. The two stages differ in cost, so
//! they are handled differently:
//!
//!   - **Geometry provenance** (vertex/polygon origins) is small and is stored
//!     durably by the geometry engine as it builds the frame — query it with
//!     `explain_vertex` / `explain_polygon`. It links each vertex/polygon back to the
//!     GX command (a per-frame command sequence number) and object-space position.
//!   - **Pixel provenance** is expensive (one entry per rendered pixel), so the
//!     rasterizer builds it lazily through a [`Poly3dSink`] (Phase 8), re-running the
//!     render for the target pixel so an explained pixel can never diverge from the
//!     drawn one.

/// Where a vertex came from: the vertex command that produced it, when in the frame it
/// ran, its object-space position, and its resulting clip coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vertex3dProvenance {
    /// Index into vertex RAM.
    pub index: u16,
    /// The `VTX_*` opcode that created it.
    pub source_op: u8,
    /// The command sequence number (commands executed since the last `SWAP_BUFFERS`).
    pub command_seq: u32,
    /// Position before the clip-matrix transform (4.12).
    pub object_position: [i32; 3],
    /// Clip-space `(x, y, z, w)` after the transform (4.12).
    pub clip: [i32; 4],
}

/// Where a polygon came from: its primitive type, the `BEGIN_VTXS` that opened its
/// list, its attributes/texture parameters, and the vertices it references.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Polygon3dProvenance {
    /// Index into polygon RAM.
    pub index: u16,
    /// `BEGIN_VTXS` primitive type: 0 triangles, 1 quads, 2 tri-strip, 3 quad-strip.
    pub primitive: u8,
    /// The command sequence number of the `BEGIN_VTXS` that opened this list.
    pub command_seq: u32,
    /// `POLYGON_ATTR`, and the polygon id decoded from its bits 24-29.
    pub attr: u32,
    pub poly_id: u8,
    pub tex_param: u32,
    pub pltt_base: u32,
    /// Vertex-RAM indices this polygon references.
    pub vertices: Vec<u16>,
}

/// Where a rendered 3D pixel came from (Phase 8, filled by the rasterizer): the winning
/// polygon and the screen position. Interpolants, texel address, and depth attach as
/// the rasterizer gains them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pixel3dProvenance {
    pub screen: (u16, u16),
    pub polygon_index: u16,
    pub poly_id: u8,
}

/// A sink the pipeline funnels provenance through. All methods default to no-ops and
/// the closures are dropped unbuilt, so [`NullSink`] is genuinely zero-cost.
pub trait Poly3dSink {
    /// Cheap gate: the rasterizer skips building a pixel's provenance closure unless
    /// the sink wants that pixel.
    fn wants_pixel(&self, _x: u16, _y: u16) -> bool {
        false
    }
    fn record_vertex(&mut self, _f: impl FnOnce() -> Vertex3dProvenance) {}
    fn record_polygon(&mut self, _f: impl FnOnce() -> Polygon3dProvenance) {}
    fn record_pixel(&mut self, _f: impl FnOnce() -> Pixel3dProvenance) {}
}

/// The fast path: records nothing.
pub struct NullSink;
impl Poly3dSink for NullSink {}

/// Captures the provenance of a single target pixel (the explain path). The rasterizer
/// records the winning polygon and pixel here when it reaches the target column/row.
#[derive(Default)]
pub struct PixelRecorder {
    target: (u16, u16),
    pub pixel: Option<Pixel3dProvenance>,
    pub polygon: Option<Polygon3dProvenance>,
}

impl PixelRecorder {
    pub fn new(x: u16, y: u16) -> Self {
        PixelRecorder {
            target: (x, y),
            pixel: None,
            polygon: None,
        }
    }
}

impl Poly3dSink for PixelRecorder {
    fn wants_pixel(&self, x: u16, y: u16) -> bool {
        (x, y) == self.target
    }
    fn record_pixel(&mut self, f: impl FnOnce() -> Pixel3dProvenance) {
        self.pixel = Some(f());
    }
    fn record_polygon(&mut self, f: impl FnOnce() -> Polygon3dProvenance) {
        self.polygon = Some(f());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The null sink builds none of the closures it is handed.
    #[test]
    fn null_sink_drops_closures() {
        let mut sink = NullSink;
        // If the closure ran it would panic; NullSink must not call it.
        sink.record_pixel(|| panic!("closure must not be built on the fast path"));
    }

    /// A pixel recorder gated on its target builds the closure only for that pixel.
    #[test]
    fn pixel_recorder_captures_only_its_target() {
        let mut sink = PixelRecorder::new(10, 20);
        assert!(!sink.wants_pixel(0, 0));
        assert!(sink.wants_pixel(10, 20));
        if sink.wants_pixel(10, 20) {
            sink.record_pixel(|| Pixel3dProvenance {
                screen: (10, 20),
                polygon_index: 3,
                poly_id: 7,
            });
        }
        assert_eq!(sink.pixel.as_ref().unwrap().polygon_index, 3);
    }
}
