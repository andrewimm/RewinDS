//! The geometry matrix engine: four current matrices (projection, position, vector,
//! texture), their push/pop stacks, and the cached clip matrix that transforms
//! vertices. All entries are 4.12 fixed-point (`1.0 == 0x1000`); every multiply
//! accumulates in `i64` and shifts back by 12. There is no floating point.
//!
//! Conventions (GBATEK "DS Geometry Engine"):
//!   - Matrices are **column-major**: element `(row, col)` is `m[col*4 + row]`.
//!   - `MTX_LOAD_4x4` etc. deliver parameters in that column-major order.
//!   - `MTX_MULT` **post-multiplies**: `current = current * param`, so the most
//!     recently applied transform acts on the vertex first (as in OpenGL).
//!   - A vertex `v` (column) is transformed as `clip = ClipMatrix * v`, where
//!     `ClipMatrix = Projection * Position`.
//!   - Mode 2 (position & vector) applies each operation to **both** the position and
//!     the vector (direction) matrices — except `MTX_SCALE`, which affects only the
//!     position matrix, since scaling must not distort normals.

/// 4.12 fixed-point unit (`1.0`).
pub const ONE: i32 = 0x1000;

/// A 4×4 fixed-point matrix in column-major order (`m[col*4 + row]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Matrix {
    pub m: [i32; 16],
}

impl Default for Matrix {
    fn default() -> Self {
        Matrix::identity()
    }
}

impl Matrix {
    pub const fn identity() -> Matrix {
        let mut m = [0i32; 16];
        m[0] = ONE;
        m[5] = ONE;
        m[10] = ONE;
        m[15] = ONE;
        Matrix { m }
    }

    /// `self * rhs` (column-major, 4.12, `i64` accumulation then `>> 12`).
    pub fn mul(&self, rhs: &Matrix) -> Matrix {
        let mut out = [0i32; 16];
        for col in 0..4 {
            for row in 0..4 {
                let mut acc: i64 = 0;
                for k in 0..4 {
                    acc += self.m[k * 4 + row] as i64 * rhs.m[col * 4 + k] as i64;
                }
                out[col * 4 + row] = (acc >> 12) as i32;
            }
        }
        Matrix { m: out }
    }

    /// Transform a 4-component column vector (`out[row] = Σ_col m[col*4+row]*v[col]`).
    pub fn transform(&self, v: [i32; 4]) -> [i32; 4] {
        let mut out = [0i32; 4];
        for (row, o) in out.iter_mut().enumerate() {
            let mut acc: i64 = 0;
            for (col, &vc) in v.iter().enumerate() {
                acc += self.m[col * 4 + row] as i64 * vc as i64;
            }
            *o = (acc >> 12) as i32;
        }
        out
    }

    /// A full 4×4 from 16 column-major parameters.
    pub fn from_4x4(p: &[i32]) -> Matrix {
        let mut m = [0i32; 16];
        m.copy_from_slice(&p[..16]);
        Matrix { m }
    }

    /// A 4×4 from 12 parameters: four columns of three rows, the fourth row implicit
    /// `(0, 0, 0, 1)`.
    pub fn from_4x3(p: &[i32]) -> Matrix {
        let mut m = [0i32; 16];
        for col in 0..4 {
            m[col * 4] = p[col * 3];
            m[col * 4 + 1] = p[col * 3 + 1];
            m[col * 4 + 2] = p[col * 3 + 2];
            m[col * 4 + 3] = if col == 3 { ONE } else { 0 };
        }
        Matrix { m }
    }

    /// A 4×4 from a 9-parameter 3×3 (rotation/scale only; translation zero).
    pub fn from_3x3(p: &[i32]) -> Matrix {
        let mut m = [0i32; 16];
        for col in 0..3 {
            m[col * 4] = p[col * 3];
            m[col * 4 + 1] = p[col * 3 + 1];
            m[col * 4 + 2] = p[col * 3 + 2];
        }
        m[15] = ONE;
        Matrix { m }
    }

    /// A scaling matrix `diag(sx, sy, sz, 1)`.
    pub fn scaling(sx: i32, sy: i32, sz: i32) -> Matrix {
        let mut m = Matrix::identity();
        m.m[0] = sx;
        m.m[5] = sy;
        m.m[10] = sz;
        m
    }

    /// A translation matrix (translation lives in the fourth column).
    pub fn translation(tx: i32, ty: i32, tz: i32) -> Matrix {
        let mut m = Matrix::identity();
        m.m[12] = tx;
        m.m[13] = ty;
        m.m[14] = tz;
        m
    }
}

/// Reinterpret a parameter word as a signed 4.12 fixed-point value.
fn fx(word: u32) -> i32 {
    word as i32
}

/// The 31-level position/vector stack depth (entry 31 is the overflow guard).
const POSVEC_DEPTH: u32 = 31;

/// The four current matrices, their stacks, the matrix mode, and the cached clip
/// matrix. Drives every `MTX_*` command.
pub struct MatrixEngine {
    /// Current matrix mode (`MTX_MODE`): 0 projection, 1 position, 2 position&vector,
    /// 3 texture.
    mode: u8,
    projection: Matrix,
    position: Matrix,
    vector: Matrix,
    texture: Matrix,
    /// Clip matrix = projection * position, recomputed on any projection/position
    /// change; transforms vertices.
    clip: Matrix,

    proj_stack: Matrix,
    proj_sp: u8, // 0 or 1
    pos_stack: [Matrix; 32],
    vec_stack: [Matrix; 32],
    posvec_sp: u32, // 0..=31
    tex_stack: Matrix,
    tex_sp: u8, // 0 or 1

    /// `GXSTAT` bit 15: a push overflow or pop underflow occurred (sticky until a
    /// write clears it).
    error: bool,
}

impl Default for MatrixEngine {
    fn default() -> Self {
        MatrixEngine::new()
    }
}

impl MatrixEngine {
    pub fn new() -> Self {
        MatrixEngine {
            mode: 0,
            projection: Matrix::identity(),
            position: Matrix::identity(),
            vector: Matrix::identity(),
            texture: Matrix::identity(),
            clip: Matrix::identity(),
            proj_stack: Matrix::identity(),
            proj_sp: 0,
            pos_stack: [Matrix::identity(); 32],
            vec_stack: [Matrix::identity(); 32],
            posvec_sp: 0,
            tex_stack: Matrix::identity(),
            tex_sp: 0,
            error: false,
        }
    }

    // --- read-back ----------------------------------------------------------

    /// The clip matrix (transforms vertices); read back via `CLIPMTX_RESULT`.
    pub fn clip(&self) -> &Matrix {
        &self.clip
    }
    pub fn position(&self) -> &Matrix {
        &self.position
    }
    pub fn vector(&self) -> &Matrix {
        &self.vector
    }
    pub fn texture(&self) -> &Matrix {
        &self.texture
    }

    /// One element of the clip matrix by column-major index (`CLIPMTX_RESULT`, 16).
    pub fn clip_read(&self, index: usize) -> u32 {
        self.clip.m[index & 15] as u32
    }

    /// One element of the vector matrix's 3×3 by index (`VECMTX_RESULT`, 9 entries).
    pub fn vector_read_3x3(&self, index: usize) -> u32 {
        let (col, row) = (index / 3, index % 3);
        self.vector.m[col * 4 + row] as u32
    }

    /// The `GXSTAT` bits the matrix engine owns: position/vector stack level (8-12),
    /// projection stack level (13), and the stack error flag (15).
    pub fn gxstat_bits(&self) -> u32 {
        let mut v = (self.posvec_sp & 0x1F) << 8;
        v |= ((self.proj_sp & 1) as u32) << 13;
        if self.error {
            v |= 1 << 15;
        }
        v
    }

    pub fn clear_error(&mut self) {
        self.error = false;
    }

    // --- command handlers ---------------------------------------------------

    pub fn set_mode(&mut self, mode: u8) {
        self.mode = mode & 3;
    }

    fn recompute_clip(&mut self) {
        self.clip = self.projection.mul(&self.position);
    }

    /// Load `mtx` into the current matrix (both position and vector in mode 2).
    fn load_current(&mut self, mtx: Matrix) {
        match self.mode {
            0 => {
                self.projection = mtx;
                self.recompute_clip();
            }
            1 => {
                self.position = mtx;
                self.recompute_clip();
            }
            2 => {
                self.position = mtx;
                self.vector = mtx;
                self.recompute_clip();
            }
            _ => self.texture = mtx,
        }
    }

    /// Post-multiply the current matrix by `rhs`. `scale_only_pos` suppresses the
    /// vector-matrix update in mode 2 (used by `MTX_SCALE`).
    fn mul_current(&mut self, rhs: &Matrix, scale_only_pos: bool) {
        match self.mode {
            0 => {
                self.projection = self.projection.mul(rhs);
                self.recompute_clip();
            }
            1 => {
                self.position = self.position.mul(rhs);
                self.recompute_clip();
            }
            2 => {
                self.position = self.position.mul(rhs);
                if !scale_only_pos {
                    self.vector = self.vector.mul(rhs);
                }
                self.recompute_clip();
            }
            _ => self.texture = self.texture.mul(rhs),
        }
    }

    pub fn load_identity(&mut self) {
        self.load_current(Matrix::identity());
    }
    pub fn load_4x4(&mut self, p: &[u32]) {
        let v: Vec<i32> = p.iter().map(|&w| fx(w)).collect();
        self.load_current(Matrix::from_4x4(&v));
    }
    pub fn load_4x3(&mut self, p: &[u32]) {
        let v: Vec<i32> = p.iter().map(|&w| fx(w)).collect();
        self.load_current(Matrix::from_4x3(&v));
    }
    pub fn mult_4x4(&mut self, p: &[u32]) {
        let v: Vec<i32> = p.iter().map(|&w| fx(w)).collect();
        self.mul_current(&Matrix::from_4x4(&v), false);
    }
    pub fn mult_4x3(&mut self, p: &[u32]) {
        let v: Vec<i32> = p.iter().map(|&w| fx(w)).collect();
        self.mul_current(&Matrix::from_4x3(&v), false);
    }
    pub fn mult_3x3(&mut self, p: &[u32]) {
        let v: Vec<i32> = p.iter().map(|&w| fx(w)).collect();
        self.mul_current(&Matrix::from_3x3(&v), false);
    }
    pub fn scale(&mut self, p: &[u32]) {
        self.mul_current(&Matrix::scaling(fx(p[0]), fx(p[1]), fx(p[2])), true);
    }
    pub fn translate(&mut self, p: &[u32]) {
        self.mul_current(&Matrix::translation(fx(p[0]), fx(p[1]), fx(p[2])), false);
    }

    pub fn push(&mut self) {
        match self.mode {
            0 => self.push_single(true),
            3 => self.push_single(false),
            _ => {
                // Position & vector share one stack; both are saved.
                if self.posvec_sp >= POSVEC_DEPTH {
                    self.error = true;
                } else {
                    let slot = self.posvec_sp as usize;
                    self.pos_stack[slot] = self.position;
                    self.vec_stack[slot] = self.vector;
                    self.posvec_sp += 1;
                }
            }
        }
    }

    fn push_single(&mut self, projection: bool) {
        let (sp, save) = if projection {
            (&mut self.proj_sp, &mut self.proj_stack)
        } else {
            (&mut self.tex_sp, &mut self.tex_stack)
        };
        if *sp >= 1 {
            self.error = true;
        } else {
            *save = if projection {
                self.projection
            } else {
                self.texture
            };
            *sp = 1;
        }
    }

    /// `MTX_POP`: the parameter is a signed 6-bit count of levels to pop.
    pub fn pop(&mut self, param: u32) {
        let offset = ((param & 0x3F) as i32) << 26 >> 26; // sign-extend 6 bits
        match self.mode {
            0 => {
                if self.proj_sp == 0 {
                    self.error = true;
                } else {
                    self.proj_sp = 0;
                    self.projection = self.proj_stack;
                    self.recompute_clip();
                }
            }
            3 => {
                if self.tex_sp == 0 {
                    self.error = true;
                } else {
                    self.tex_sp = 0;
                    self.texture = self.tex_stack;
                }
            }
            _ => {
                let new = self.posvec_sp as i32 - offset;
                if !(0..=POSVEC_DEPTH as i32).contains(&new) {
                    self.error = true;
                }
                self.posvec_sp = new.clamp(0, POSVEC_DEPTH as i32) as u32;
                let slot = self.posvec_sp as usize;
                self.position = self.pos_stack[slot];
                self.vector = self.vec_stack[slot];
                self.recompute_clip();
            }
        }
    }

    /// `MTX_STORE`: copy the current matrix to a stack slot (the parameter selects the
    /// slot for the position/vector stack; the 1-level stacks ignore it).
    pub fn store(&mut self, param: u32) {
        match self.mode {
            0 => self.proj_stack = self.projection,
            3 => self.tex_stack = self.texture,
            _ => {
                let slot = (param & 0x1F) as usize;
                self.pos_stack[slot] = self.position;
                self.vec_stack[slot] = self.vector;
            }
        }
    }

    /// `MTX_RESTORE`: load the current matrix from a stack slot.
    pub fn restore(&mut self, param: u32) {
        match self.mode {
            0 => {
                self.projection = self.proj_stack;
                self.recompute_clip();
            }
            3 => self.texture = self.tex_stack,
            _ => {
                let slot = (param & 0x1F) as usize;
                self.position = self.pos_stack[slot];
                self.vector = self.vec_stack[slot];
                self.recompute_clip();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(int: i32) -> i32 {
        int * ONE
    }

    #[test]
    fn identity_is_multiplicative_unit() {
        let i = Matrix::identity();
        let a = Matrix::translation(f(2), f(3), f(4));
        assert_eq!(i.mul(&a), a);
        assert_eq!(a.mul(&i), a);
    }

    #[test]
    fn translation_moves_a_point() {
        let t = Matrix::translation(f(2), 0, 0);
        // Point (1,0,0,1) → (3,0,0,1).
        assert_eq!(t.transform([f(1), 0, 0, f(1)]), [f(3), 0, 0, f(1)]);
    }

    #[test]
    fn scaling_scales_a_point() {
        let s = Matrix::scaling(f(2), f(3), f(4));
        assert_eq!(s.transform([f(1), f(1), f(1), f(1)]), [f(2), f(3), f(4), f(1)]);
    }

    #[test]
    fn post_multiply_applies_newest_transform_first() {
        // current = I; translate(+2x) then scale(2): a vertex is scaled first, then
        // translated. Post-multiply => current = T * S, so current*v = T*(S*v).
        let mut e = MatrixEngine::new();
        e.set_mode(1); // position
        e.translate(&[f(2) as u32, 0, 0]);
        e.scale(&[(f(2)) as u32, f(2) as u32, f(2) as u32]);
        // v = (1,0,0,1): scale → (2,0,0,1), translate → (4,0,0,1).
        let out = e.clip().transform([f(1), 0, 0, f(1)]);
        assert_eq!(out, [f(4), 0, 0, f(1)]);
    }

    #[test]
    fn clip_matrix_is_projection_times_position() {
        let mut e = MatrixEngine::new();
        e.set_mode(0); // projection
        e.translate(&[f(10) as u32, 0, 0]);
        e.set_mode(1); // position
        e.translate(&[f(1) as u32, f(2) as u32, 0]);
        // clip = Proj(T10x) * Pos(T1x2y); a point at origin → (11, 2, 0, 1).
        let out = e.clip().transform([0, 0, 0, f(1)]);
        assert_eq!(out, [f(11), f(2), 0, f(1)]);
    }

    #[test]
    fn push_pop_restores_the_position_matrix_exactly() {
        let mut e = MatrixEngine::new();
        e.set_mode(1);
        e.translate(&[f(5) as u32, 0, 0]);
        let saved = *e.position();
        e.push();
        e.translate(&[f(3) as u32, f(3) as u32, 0]); // perturb
        assert_ne!(*e.position(), saved);
        e.pop(1);
        assert_eq!(*e.position(), saved);
        assert_eq!(e.gxstat_bits() & (1 << 15), 0); // no error
    }

    #[test]
    fn store_and_restore_by_slot() {
        let mut e = MatrixEngine::new();
        e.set_mode(1);
        e.translate(&[f(7) as u32, 0, 0]);
        e.store(4);
        e.load_identity();
        assert_eq!(*e.position(), Matrix::identity());
        e.restore(4);
        assert_eq!(e.position().m[12], f(7));
    }

    #[test]
    fn position_vector_mode_updates_both_but_scale_only_position() {
        let mut e = MatrixEngine::new();
        e.set_mode(2); // position & vector
        e.translate(&[f(1) as u32, 0, 0]);
        // Both matrices got the translation.
        assert_eq!(e.position().m[12], f(1));
        assert_eq!(e.vector().m[12], f(1));
        // Scale hits the position matrix only (normals must not be scaled).
        e.scale(&[f(2) as u32, f(2) as u32, f(2) as u32]);
        assert_eq!(e.position().m[0], f(2));
        assert_eq!(e.vector().m[0], ONE); // vector unchanged
    }

    #[test]
    fn stack_overflow_and_underflow_set_the_error_flag() {
        let mut e = MatrixEngine::new();
        e.set_mode(1);
        for _ in 0..POSVEC_DEPTH {
            e.push();
        }
        assert_eq!(e.gxstat_bits() & (1 << 15), 0);
        e.push(); // one past the last usable level
        assert_ne!(e.gxstat_bits() & (1 << 15), 0);
        e.clear_error();

        // Projection stack is one level deep: a second push errors.
        e.set_mode(0);
        e.push();
        assert_eq!(e.gxstat_bits() & (1 << 15), 0);
        e.push();
        assert_ne!(e.gxstat_bits() & (1 << 15), 0);
    }

    #[test]
    fn gxstat_reports_stack_levels() {
        let mut e = MatrixEngine::new();
        e.set_mode(1);
        e.push();
        e.push();
        e.push();
        assert_eq!((e.gxstat_bits() >> 8) & 0x1F, 3); // pos/vec level 3
        e.set_mode(0);
        e.push();
        assert_eq!((e.gxstat_bits() >> 13) & 1, 1); // projection level 1
    }

    #[test]
    fn vector_read_back_is_column_major_3x3() {
        let mut e = MatrixEngine::new();
        e.set_mode(2);
        // Load a recognizable matrix into both position and vector.
        let mut p = [0u32; 16];
        for (i, w) in p.iter_mut().enumerate() {
            *w = (i as i32 * ONE) as u32;
        }
        e.load_4x4(&p);
        // VECMTX index 4 = (row 1, col 1) = m[1*4+1] = m[5].
        assert_eq!(e.vector_read_3x3(4), e.vector().m[5] as u32);
        assert_eq!(e.vector_read_3x3(4), (5 * ONE) as u32);
    }
}
