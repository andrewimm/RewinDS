//! The DS geometry command set: opcodes, their parameter-word counts, and the
//! decoder that unpacks the packed GXFIFO word stream into individual command +
//! parameter entries.
//!
//! Commands reach the engine two ways (both converge on the same per-parameter
//! [`crate::fifo::Entry`] stream):
//!
//!   - **Packed GXFIFO** (`0x4000400`): a word holds up to four 8-bit command ids,
//!     followed by the parameter words for each in order. Zero-parameter commands
//!     take no following word. This is the bulk/DMA path — [`Decoder`] unpacks it.
//!   - **Direct command ports** (`0x4000440`+): the command id is implied by the
//!     address (`id = (addr - 0x4000400) / 4`), and each write is one parameter word
//!     (a dummy for a zero-parameter command). No unpacking needed.
//!
//! The parameter count is the decode key: the engine must know how many words each
//! command consumes before it can find the next command boundary.

/// Command opcodes (GBATEK "DS Geometry Commands"). The MMIO port for command `id`
/// is `0x4000400 + id * 4`.
pub mod op {
    pub const NOP: u8 = 0x00;
    pub const MTX_MODE: u8 = 0x10;
    pub const MTX_PUSH: u8 = 0x11;
    pub const MTX_POP: u8 = 0x12;
    pub const MTX_STORE: u8 = 0x13;
    pub const MTX_RESTORE: u8 = 0x14;
    pub const MTX_IDENTITY: u8 = 0x15;
    pub const MTX_LOAD_4X4: u8 = 0x16;
    pub const MTX_LOAD_4X3: u8 = 0x17;
    pub const MTX_MULT_4X4: u8 = 0x18;
    pub const MTX_MULT_4X3: u8 = 0x19;
    pub const MTX_MULT_3X3: u8 = 0x1A;
    pub const MTX_SCALE: u8 = 0x1B;
    pub const MTX_TRANS: u8 = 0x1C;
    pub const COLOR: u8 = 0x20;
    pub const NORMAL: u8 = 0x21;
    pub const TEXCOORD: u8 = 0x22;
    pub const VTX_16: u8 = 0x23;
    pub const VTX_10: u8 = 0x24;
    pub const VTX_XY: u8 = 0x25;
    pub const VTX_XZ: u8 = 0x26;
    pub const VTX_YZ: u8 = 0x27;
    pub const VTX_DIFF: u8 = 0x28;
    pub const POLYGON_ATTR: u8 = 0x29;
    pub const TEXIMAGE_PARAM: u8 = 0x2A;
    pub const PLTT_BASE: u8 = 0x2B;
    pub const DIF_AMB: u8 = 0x30;
    pub const SPE_EMI: u8 = 0x31;
    pub const LIGHT_VECTOR: u8 = 0x32;
    pub const LIGHT_COLOR: u8 = 0x33;
    pub const SHININESS: u8 = 0x34;
    pub const BEGIN_VTXS: u8 = 0x40;
    pub const END_VTXS: u8 = 0x41;
    pub const SWAP_BUFFERS: u8 = 0x50;
    pub const VIEWPORT: u8 = 0x60;
    pub const BOX_TEST: u8 = 0x70;
    pub const POS_TEST: u8 = 0x71;
    pub const VEC_TEST: u8 = 0x72;
}

/// The number of parameter words command `id` consumes. Unknown/invalid opcodes
/// (and `NOP`) take zero and are ignored downstream.
pub fn param_count(id: u8) -> u32 {
    use op::*;
    match id {
        MTX_MODE => 1,
        MTX_PUSH => 0,
        MTX_POP => 1,
        MTX_STORE => 1,
        MTX_RESTORE => 1,
        MTX_IDENTITY => 0,
        MTX_LOAD_4X4 => 16,
        MTX_LOAD_4X3 => 12,
        MTX_MULT_4X4 => 16,
        MTX_MULT_4X3 => 12,
        MTX_MULT_3X3 => 9,
        MTX_SCALE => 3,
        MTX_TRANS => 3,
        COLOR => 1,
        NORMAL => 1,
        TEXCOORD => 1,
        VTX_16 => 2,
        VTX_10 => 1,
        VTX_XY => 1,
        VTX_XZ => 1,
        VTX_YZ => 1,
        VTX_DIFF => 1,
        POLYGON_ATTR => 1,
        TEXIMAGE_PARAM => 1,
        PLTT_BASE => 1,
        DIF_AMB => 1,
        SPE_EMI => 1,
        LIGHT_VECTOR => 1,
        LIGHT_COLOR => 1,
        SHININESS => 32,
        BEGIN_VTXS => 1,
        END_VTXS => 0,
        SWAP_BUFFERS => 1,
        VIEWPORT => 1,
        BOX_TEST => 3,
        POS_TEST => 2,
        VEC_TEST => 1,
        _ => 0, // NOP and unknown opcodes
    }
}

/// Unpacks the packed GXFIFO word stream into `(command, parameter)` pairs. A word
/// with no command awaiting parameters is read as four packed opcode bytes; the
/// following words are parameters, dealt out to those opcodes in order. Zero-
/// parameter opcodes are emitted immediately with a dummy parameter of `0` and
/// consume no word.
#[derive(Default)]
pub struct Decoder {
    /// The four opcodes of the packing word currently being consumed.
    packed: [u8; 4],
    /// Index of the next packed opcode to process (0..=4).
    idx: usize,
    /// Parameters still expected for the opcode currently being filled.
    remaining: u32,
    /// The opcode currently being filled.
    cur: u8,
}

impl Decoder {
    /// Feed one GXFIFO word, emitting `(command, parameter)` for each entry it
    /// completes. `remaining == 0` uniquely means "the next word is a packing word",
    /// so no separate active flag is needed.
    pub fn feed(&mut self, word: u32, mut emit: impl FnMut(u8, u32)) {
        if self.remaining > 0 {
            emit(self.cur, word);
            self.remaining -= 1;
            if self.remaining == 0 {
                self.advance(&mut emit);
            }
        } else {
            self.packed = word.to_le_bytes();
            self.idx = 0;
            self.advance(&mut emit);
        }
    }

    /// Walk the packed opcodes, emitting zero-parameter ones immediately, until one
    /// needs parameters (then wait) or all four are consumed.
    fn advance(&mut self, emit: &mut impl FnMut(u8, u32)) {
        while self.idx < 4 {
            let cmd = self.packed[self.idx];
            self.idx += 1;
            if cmd == op::NOP {
                continue; // NOP is packing filler: no entry, no parameter word
            }
            let n = param_count(cmd);
            if n == 0 {
                emit(cmd, 0);
            } else {
                self.cur = cmd;
                self.remaining = n;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect the `(command, param)` entries a sequence of GXFIFO words decodes to.
    fn decode(words: &[u32]) -> Vec<(u8, u32)> {
        let mut d = Decoder::default();
        let mut out = Vec::new();
        for &w in words {
            d.feed(w, |c, p| out.push((c, p)));
        }
        out
    }

    #[test]
    fn single_one_param_command() {
        // Packing word = MTX_MODE in slot 0, rest NOP; then one parameter.
        let packing = op::MTX_MODE as u32;
        assert_eq!(decode(&[packing, 2]), vec![(op::MTX_MODE, 2)]);
    }

    #[test]
    fn zero_param_commands_emit_without_a_following_word() {
        // Three real zero-parameter commands plus a NOP filler byte; the reals emit
        // (with dummy param 0) and no parameter words follow, while NOP is skipped.
        let packing = u32::from_le_bytes([op::MTX_PUSH, op::MTX_IDENTITY, op::END_VTXS, op::NOP]);
        assert_eq!(
            decode(&[packing]),
            vec![(op::MTX_PUSH, 0), (op::MTX_IDENTITY, 0), (op::END_VTXS, 0)]
        );
    }

    #[test]
    fn mixed_packing_deals_params_in_order() {
        // IDENTITY(0), TRANS(3), MTX_MODE(1), NOP(filler): params are 3 for TRANS then
        // 1 for MTX_MODE; IDENTITY emits with no param, NOP is skipped entirely.
        let packing =
            u32::from_le_bytes([op::MTX_IDENTITY, op::MTX_TRANS, op::MTX_MODE, op::NOP]);
        let got = decode(&[packing, 10, 20, 30, 40]);
        assert_eq!(
            got,
            vec![
                (op::MTX_IDENTITY, 0),
                (op::MTX_TRANS, 10),
                (op::MTX_TRANS, 20),
                (op::MTX_TRANS, 30),
                (op::MTX_MODE, 40),
            ]
        );
    }

    #[test]
    fn params_spanning_writes_then_next_packing_word() {
        // A 16-param load fully consumes 16 words before the next packing word.
        let mut words = vec![op::MTX_LOAD_4X4 as u32];
        words.extend(1..=16u32);
        words.push(op::COLOR as u32); // next packing word
        words.push(0x7FFF);
        let got = decode(&words);
        assert_eq!(got.len(), 17);
        assert!(got[..16].iter().all(|&(c, _)| c == op::MTX_LOAD_4X4));
        assert_eq!(got[0], (op::MTX_LOAD_4X4, 1));
        assert_eq!(got[15], (op::MTX_LOAD_4X4, 16));
        assert_eq!(got[16], (op::COLOR, 0x7FFF));
    }
}
