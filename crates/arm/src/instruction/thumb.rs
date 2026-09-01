//! Structural representation of a decoded 16-bit Thumb instruction.
//!
//! Thumb has its own representation, kept entirely separate from
//! [`crate::instruction::arm`]: a Thumb `LSL` is a Thumb `LSL`, not a widened
//! ARM `MOV`. Only the architecture-shared primitives [`Register`] and
//! [`Condition`] are reused; every opcode set here is Thumb's own, because they
//! differ from ARM's (the format-4 ALU set, for instance, is not the ARM
//! data-processing set).
//!
//! As with ARM, the decoder is lossless: fields are stored as encoded. The one
//! normalization, matching [`crate::instruction::arm::Branch`], is that **branch
//! displacements are sign-extended and scaled to a byte offset**, since a branch
//! offset is conceptually a byte displacement. All other immediates are stored
//! as the **raw encoded field, unscaled**; each field's doc note gives the scale
//! the interpreter/disassembler applies (`×4` for word accesses, `×2` for
//! halfword, `×1` for byte). This keeps the encoding perfectly reconstructible.

use crate::condition::Condition;
use crate::register::Register;

/// A decoded 16-bit Thumb instruction, one variant per Thumb encoding format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbInstruction {
    /// Format 1 — move shifted register: `LSL`/`LSR`/`ASR Rd, Rs, #amount`.
    MoveShifted {
        op: ThumbShiftOp,
        /// 5-bit shift amount.
        amount: u8,
        rs: Register,
        rd: Register,
    },

    /// Format 2 — add/subtract with a register or 3-bit immediate operand.
    AddSubtract {
        subtract: bool,
        operand: AddSubOperand,
        rs: Register,
        rd: Register,
    },

    /// Format 3 — move/compare/add/subtract with an 8-bit immediate.
    AluImmediate {
        op: ThumbImmediateOp,
        rd: Register,
        immediate: u8,
    },

    /// Format 4 — ALU operation between two low registers.
    AluOperation {
        op: ThumbAluOp,
        rs: Register,
        rd: Register,
    },

    /// Format 5 — high-register operations and `BX`. The `H` bits are folded
    /// into full 4-bit `rd`/`rs`, so either operand may be r8..r15. For `Bx`,
    /// `rs` is the target and `rd` is unused.
    HiRegister {
        op: ThumbHiRegOp,
        rs: Register,
        rd: Register,
    },

    /// Format 6 — PC-relative load: `LDR Rd, [PC, #word8 * 4]`.
    PcRelativeLoad {
        rd: Register,
        /// Word offset; effective byte offset is `word8 * 4`.
        word8: u8,
    },

    /// Format 7 — load/store with a register offset (word or byte).
    LoadStoreRegister {
        load: bool,
        byte: bool,
        ro: Register,
        rb: Register,
        rd: Register,
    },

    /// Format 8 — load/store sign-extended byte or halfword, register offset.
    LoadStoreSignExtended {
        op: ThumbSignExtendOp,
        ro: Register,
        rb: Register,
        rd: Register,
    },

    /// Format 9 — load/store with a 5-bit immediate offset (word or byte).
    LoadStoreImmediate {
        load: bool,
        byte: bool,
        /// Raw 5-bit offset; effective byte offset is `offset * 4` for word
        /// accesses and `offset` for byte accesses.
        offset: u8,
        rb: Register,
        rd: Register,
    },

    /// Format 10 — load/store halfword with a 5-bit immediate offset.
    LoadStoreHalfword {
        load: bool,
        /// Raw 5-bit offset; effective byte offset is `offset * 2`.
        offset: u8,
        rb: Register,
        rd: Register,
    },

    /// Format 11 — SP-relative load/store: `[SP, #word8 * 4]`.
    SpRelativeLoadStore {
        load: bool,
        rd: Register,
        /// Word offset; effective byte offset is `word8 * 4`.
        word8: u8,
    },

    /// Format 12 — load address: `ADD Rd, (PC|SP), #word8 * 4`.
    LoadAddress {
        source: LoadAddressSource,
        rd: Register,
        /// Word offset; effective byte offset is `word8 * 4`.
        word8: u8,
    },

    /// Format 13 — adjust the stack pointer: `ADD SP, #±(word7 * 4)`.
    AdjustStackPointer {
        subtract: bool,
        /// Raw 7-bit offset; effective byte offset is `word7 * 4`.
        word7: u8,
    },

    /// Format 14 — `PUSH`/`POP` a low-register list, optionally including LR
    /// (on push) or PC (on pop).
    PushPop {
        /// Load (`POP`) vs store (`PUSH`).
        pop: bool,
        /// The `R` bit: include LR when pushing / PC when popping.
        include_pc_lr: bool,
        /// One bit per register r0..r7.
        register_list: u8,
    },

    /// Format 15 — block transfer `LDMIA`/`STMIA Rb!, {register_list}` (always
    /// increment-after with writeback).
    BlockTransfer {
        load: bool,
        rb: Register,
        /// One bit per register r0..r7.
        register_list: u8,
    },

    /// Format 16 — conditional branch. The `offset` is already sign-extended and
    /// scaled to a byte displacement.
    ConditionalBranch { condition: Condition, offset: i32 },

    /// Format 17 — software interrupt with an 8-bit comment.
    SoftwareInterrupt { comment: u8 },

    /// Format 18 — unconditional branch. The `offset` is already sign-extended
    /// and scaled to a byte displacement.
    Branch { offset: i32 },

    /// Format 19 — long branch with link, encoded as two consecutive halfwords.
    /// The `H` bit selects the half: the first (`second_half == false`) sets up
    /// the high part of the target in LR, the second completes the branch. The
    /// raw 11-bit field is kept; the interpreter combines the two halves.
    LongBranchLink {
        second_half: bool,
        /// The second half is `BLX` (switch to ARM) rather than `BL`. ARMv5-only.
        exchange: bool,
        offset: u16,
    },

    /// A halfword the decoder does not assign to a known format (including the
    /// ARMv5 `BLX` space and the reserved `cond == 1110` conditional branch,
    /// both undefined on the ARM7TDMI). The raw value is retained.
    Undefined { raw: u16 },
}

/// Format 1 shift operations. Thumb's move-shifted form cannot encode `ROR`,
/// which is why this is a distinct, smaller set than the ARM shift kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThumbShiftOp {
    Lsl, // 00
    Lsr, // 01
    Asr, // 10
}

/// The second operand of a format-2 add/subtract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddSubOperand {
    Register(Register),
    /// A 3-bit immediate.
    Immediate(u8),
}

/// Format 3 operations against an 8-bit immediate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThumbImmediateOp {
    Mov, // 00
    Cmp, // 01
    Add, // 10
    Sub, // 11
}

/// Format 4 ALU operations. This 16-entry set is Thumb's own and does not match
/// the ARM data-processing opcodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThumbAluOp {
    And, // 0000
    Eor, // 0001
    Lsl, // 0010
    Lsr, // 0011
    Asr, // 0100
    Adc, // 0101
    Sbc, // 0110
    Ror, // 0111
    Tst, // 1000
    Neg, // 1001
    Cmp, // 1010
    Cmn, // 1011
    Orr, // 1100
    Mul, // 1101
    Bic, // 1110
    Mvn, // 1111
}

/// Format 5 high-register operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThumbHiRegOp {
    Add, // 00
    Cmp, // 01
    Mov, // 10
    Bx,  // 11
}

/// Format 8 sign-extended load/store operations (the `S`/`H` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThumbSignExtendOp {
    StoreHalfword,      // S=0 H=0
    LoadHalfword,       // S=0 H=1
    LoadSignedByte,     // S=1 H=0
    LoadSignedHalfword, // S=1 H=1
}

/// The base register of a format-12 load-address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LoadAddressSource {
    Pc,
    Sp,
}
