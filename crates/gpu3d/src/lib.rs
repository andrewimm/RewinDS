//! The DS 3D graphics engine: a fixed-point geometry pipeline and a software
//! scanline rasterizer, built as the correctness reference for a later compute
//! backend. All geometry and lighting math is fixed-point integer — there is no
//! `f32` anywhere in this crate.
//!
//! This crate owns the console's GX register block (the ARM9 side of `0x4000400`+,
//! plus `DISP3DCNT` and the clear/fog/toon control registers) behind [`Gpu3d`]. The
//! `nds` crate routes those registers here, drains the command FIFO into the
//! geometry engine, and composites the rasterized output as BG0 of 2D Engine A.
//!
//! Current status: the command processor and FIFO (this module, [`command`],
//! [`fifo`]) are in place; the matrix engine, geometry, clipping, lighting,
//! rasterizer, and provenance follow in later phases.

pub mod command;
pub mod debug;
pub mod fifo;
pub mod geometry;
pub mod matrix;
pub mod raster;
pub mod texture;

use command::Decoder;
use debug::{Polygon3dProvenance, Vertex3dProvenance};
use fifo::{Entry, Fifo};
use geometry::GeometryEngine;

/// IO register offsets relative to `0x0400_0000` (the ARM9 GX block).
mod reg {
    pub const DISP3DCNT: u32 = 0x060;
    pub const GXFIFO_LO: u32 = 0x400;
    pub const GXFIFO_HI: u32 = 0x440; // exclusive end of the GXFIFO region
    pub const PORT_HI: u32 = 0x600; // exclusive end of the command-port region
    pub const GXSTAT: u32 = 0x600;
    pub const RAM_COUNT: u32 = 0x604;
    pub const CLIPMTX_LO: u32 = 0x640;
    pub const CLIPMTX_HI: u32 = 0x680; // exclusive; 16 words
    pub const VECMTX_LO: u32 = 0x680;
    pub const VECMTX_HI: u32 = 0x6A4; // exclusive; 9 words
}

/// The 3D engine device: the command FIFO and its decoder, the geometry engine, the
/// rasterizer's output buffer, texture VRAM, and the GX control registers.
pub struct Gpu3d {
    fifo: Fifo,
    decoder: Decoder,
    geometry: GeometryEngine,
    /// `DISP3DCNT` (`0x4000060`): 3D display/blend/fog/edge enables.
    disp3dcnt: u16,
    /// The rasterized 256×192 output of the sealed render list, and the frame index it
    /// was rendered from (so it is rasterized at most once per swap).
    framebuffer: raster::Framebuffer3d,
    rendered_frame: u64,
    /// The assembled texture-image VRAM (512 KB) and texture-palette VRAM (`0x18000`),
    /// refreshed by the `nds` glue from banked VRAM before each render. The engine
    /// owns the storage but stays ignorant of the VRAM banking that fills it.
    tex_image: Box<[u8]>,
    tex_palette: Box<[u8]>,
}

impl Default for Gpu3d {
    fn default() -> Self {
        Gpu3d::new()
    }
}

impl Gpu3d {
    pub fn new() -> Self {
        Gpu3d {
            fifo: Fifo::default(),
            decoder: Decoder::default(),
            geometry: GeometryEngine::default(),
            disp3dcnt: 0,
            framebuffer: raster::Framebuffer3d::new(),
            rendered_frame: 0,
            tex_image: vec![0u8; 0x8_0000].into_boxed_slice(),
            tex_palette: vec![0u8; 0x1_8000].into_boxed_slice(),
        }
    }

    /// The texture-image VRAM buffer, for the `nds` glue to refresh from banked VRAM
    /// before a render (512 KB, indexed by the `TEXIMAGE_PARAM` offset).
    pub fn texture_image_mut(&mut self) -> &mut [u8] {
        &mut self.tex_image
    }

    /// The texture-palette VRAM buffer, likewise refreshed before a render (`0x18000`).
    pub fn texture_palette_mut(&mut self) -> &mut [u8] {
        &mut self.tex_palette
    }

    // --- command submission -------------------------------------------------

    /// Submit one packed GXFIFO word (the bulk/DMA path). Decodes into per-parameter
    /// [`Entry`]s and buffers them. Returns `false` if the FIFO filled mid-word — the
    /// caller should stall the CPU rather than lose the write.
    pub fn write_gxfifo(&mut self, word: u32) -> bool {
        fifo::push_decoded(&mut self.fifo, &mut self.decoder, word) == 0
    }

    /// Submit one word to a direct command port (`command` implied by the address).
    /// Each write is one parameter (a dummy `0` for zero-parameter commands).
    pub fn write_port(&mut self, command: u8, word: u32) -> bool {
        let param = if command::param_count(command) == 0 { 0 } else { word };
        self.fifo.push(Entry { command, param })
    }

    /// Drain the next buffered command entry (raw, for the future async drain).
    pub fn pop_command(&mut self) -> Option<Entry> {
        self.fifo.pop()
    }

    /// The `(polygons, vertices)` high-water mark the geometry engine has built in a
    /// single frame — confirms a game is submitting 3D geometry through the pipeline.
    pub fn peak_geometry(&self) -> (usize, usize) {
        self.geometry.peak()
    }

    /// Provenance of a built vertex: its source command, object-space position, and
    /// clip coordinates. `None` if the index is out of range.
    pub fn explain_vertex(&self, index: usize) -> Option<Vertex3dProvenance> {
        self.geometry.explain_vertex(index)
    }

    /// Provenance of a built polygon: its primitive type, attributes, and vertices.
    pub fn explain_polygon(&self, index: usize) -> Option<Polygon3dProvenance> {
        self.geometry.explain_polygon(index)
    }

    /// The sealed render list — the previous frame's geometry that the rasterizer
    /// draws (produced by `SWAP_BUFFERS`).
    pub fn render_list(&self) -> &geometry::RenderList {
        self.geometry.render_list()
    }

    /// Rasterize the sealed render list into the internal framebuffer (once per swap;
    /// a no-op if already rendered for the current frame), sampling from the texture
    /// VRAM the glue refreshed via [`Gpu3d::texture_image_mut`]. Called at V-blank.
    pub fn render_frame(&mut self) {
        let frame = self.geometry.render_list().frame;
        if frame != self.rendered_frame {
            let tex = texture::TextureSet { image: &self.tex_image, palette: &self.tex_palette };
            raster::render(self.geometry.render_list(), &tex, &mut self.framebuffer);
            self.rendered_frame = frame;
        }
    }

    /// The rasterized 3D framebuffer (256×192), composited as Engine A's BG0.
    pub fn framebuffer_3d(&self) -> &raster::Framebuffer3d {
        &self.framebuffer
    }

    /// `DISP3DCNT` (`0x4000060`): the 3D display/blend/test/fog enable bits.
    pub fn disp3dcnt(&self) -> u16 {
        self.disp3dcnt
    }

    /// Execute every complete command buffered in the FIFO (a command is complete
    /// once all its parameter entries are present), advancing the geometry engine.
    /// The `nds` glue calls this after each submission. Currently only the matrix
    /// engine is driven; vertex/lighting/swap commands are consumed and ignored until
    /// their phases land. (Until command timing exists this runs synchronously, so
    /// the FIFO never back-pressures the CPU.)
    pub fn run_pending(&mut self) {
        while let Some(front) = self.fifo.front() {
            let cmd = front.command;
            let n = command::param_count(cmd) as usize;
            let needed = n.max(1); // a zero-parameter command still occupies one entry
            if self.fifo.len() < needed {
                break; // wait for the rest of this command's parameters
            }
            let mut params = [0u32; 32];
            for p in params.iter_mut().take(needed) {
                *p = self.fifo.pop().expect("checked len").param;
            }
            self.geometry.execute(cmd, &params[..n]);
        }
    }

    // --- FIFO status for the host bus ---------------------------------------

    /// Whether a GXFIFO DMA should run to refill the FIFO (it is below half full).
    pub fn fifo_wants_dma(&self) -> bool {
        self.fifo.wants_dma()
    }

    /// Whether the FIFO currently asserts its IRQ condition (per its IRQ mode).
    pub fn fifo_irq_asserted(&self) -> bool {
        self.fifo.irq_asserted()
    }

    /// Whether the FIFO is full — a further command write must stall the CPU.
    pub fn fifo_full(&self) -> bool {
        self.fifo.is_full()
    }

    // --- MMIO ---------------------------------------------------------------

    /// Write an ARM9 GX register. `offset` is relative to `0x0400_0000`. Command
    /// submission (GXFIFO and the direct ports) is always a 32-bit word.
    pub fn write_register(&mut self, offset: u32, value: u32, bytes: u32) {
        match offset {
            reg::GXFIFO_LO..reg::GXFIFO_HI => {
                self.write_gxfifo(value);
            }
            reg::GXFIFO_HI..reg::PORT_HI => {
                let command = ((offset - reg::GXFIFO_LO) >> 2) as u8;
                self.write_port(command, value);
            }
            reg::GXSTAT => {
                // The IRQ mode (bits 30-31) is writable, and a write-1 to bit 15
                // clears the matrix-stack error.
                self.fifo.set_irq_mode_bits(value >> 30);
                if value & (1 << 15) != 0 {
                    self.geometry.matrix.clear_error();
                }
            }
            reg::DISP3DCNT => {
                self.disp3dcnt = merge16(self.disp3dcnt, value, bytes);
            }
            // Clear/fog/toon/edge control registers are consumed by the rasterizer
            // (later phases); accept and ignore their writes for now.
            _ => {}
        }
    }

    /// Read an ARM9 GX register. `offset` is relative to `0x0400_0000`.
    pub fn read_register(&self, offset: u32, _bytes: u32) -> u32 {
        match offset {
            reg::GXSTAT => self.gxstat(),
            reg::RAM_COUNT => self.geometry.ram_count(),
            reg::DISP3DCNT => self.disp3dcnt as u32,
            reg::CLIPMTX_LO..reg::CLIPMTX_HI => {
                self.geometry.matrix.clip_read(((offset - reg::CLIPMTX_LO) / 4) as usize)
            }
            reg::VECMTX_LO..reg::VECMTX_HI => {
                self.geometry
                    .matrix
                    .vector_read_3x3(((offset - reg::VECMTX_LO) / 4) as usize)
            }
            _ => 0,
        }
    }

    /// `GXSTAT` (`0x4000600`): the FIFO's fill bits merged with the matrix-stack level
    /// and error bits. (Geometry-busy is added when command timing lands.)
    fn gxstat(&self) -> u32 {
        self.fifo.gxstat_bits() | self.geometry.gxstat_bits()
    }
}

/// Merge a partial (byte/halfword) write into a 16-bit register, honouring the write
/// width so a byte write to the high or low half lands correctly.
fn merge16(current: u16, value: u32, bytes: u32) -> u16 {
    match bytes {
        1 => (current & 0xFF00) | (value as u16 & 0x00FF),
        _ => value as u16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gxfifo_write_then_drain_round_trips_a_command() {
        let mut g = Gpu3d::new();
        // MTX_MODE (1 param) via the packed FIFO.
        g.write_register(reg::GXFIFO_LO, command::op::MTX_MODE as u32, 4);
        g.write_register(reg::GXFIFO_LO, 1, 4); // param = position mode
        assert_eq!(g.pop_command(), Some(Entry { command: command::op::MTX_MODE, param: 1 }));
        assert_eq!(g.pop_command(), None);
    }

    #[test]
    fn direct_port_address_implies_the_command() {
        let mut g = Gpu3d::new();
        // Port for VTX_16 (0x23) is 0x400 + 0x23*4 = 0x48C; two parameter words.
        let port = reg::GXFIFO_LO + (command::op::VTX_16 as u32) * 4;
        assert_eq!(port, 0x48C);
        g.write_register(port, 0x0ABC_0DEF, 4);
        g.write_register(port, 0x0000_0123, 4);
        assert_eq!(g.pop_command(), Some(Entry { command: command::op::VTX_16, param: 0x0ABC_0DEF }));
        assert_eq!(g.pop_command(), Some(Entry { command: command::op::VTX_16, param: 0x0000_0123 }));
    }

    #[test]
    fn gxstat_reports_count_and_flags() {
        let mut g = Gpu3d::new();
        assert_eq!(g.read_register(reg::GXSTAT, 4) & (1 << 26), 1 << 26); // empty
        // Push a few entries via a zero-param packing word (4 MTX_PUSH).
        let packing = u32::from_le_bytes([command::op::MTX_PUSH; 4]);
        g.write_register(reg::GXFIFO_LO, packing, 4);
        let stat = g.read_register(reg::GXSTAT, 4);
        assert_eq!((stat >> 16) & 0x1FF, 4); // count == 4
        assert_eq!(stat & (1 << 26), 0); // not empty
        assert_eq!(stat & (1 << 25), 1 << 25); // less than half
    }

    #[test]
    fn fifo_driven_matrix_command_reaches_clip_readback() {
        let mut g = Gpu3d::new();
        // MTX_MODE = position (1), then MTX_TRANS (2,0,0) — all via the packed FIFO.
        g.write_register(reg::GXFIFO_LO, command::op::MTX_MODE as u32, 4);
        g.write_register(reg::GXFIFO_LO, 1, 4);
        g.write_register(reg::GXFIFO_LO, command::op::MTX_TRANS as u32, 4);
        g.write_register(reg::GXFIFO_LO, (2 * matrix::ONE) as u32, 4);
        g.write_register(reg::GXFIFO_LO, 0, 4);
        g.write_register(reg::GXFIFO_LO, 0, 4);
        g.run_pending();
        // Clip matrix element 12 (column 3, row 0) is the translation x = 2.0, and the
        // FIFO drains to empty/idle.
        assert_eq!(g.read_register(reg::CLIPMTX_LO + 12 * 4, 4), (2 * matrix::ONE) as u32);
        assert_eq!(g.read_register(reg::GXSTAT, 4) & (1 << 26), 1 << 26); // empty
    }

    #[test]
    fn gxstat_write_sets_irq_mode() {
        let mut g = Gpu3d::new();
        g.write_register(reg::GXSTAT, 1 << 30, 4); // IRQ mode 1 (less than half)
        assert!(g.fifo_irq_asserted()); // empty is < half
        assert_eq!(g.read_register(reg::GXSTAT, 4) >> 30, 1);
    }
}
