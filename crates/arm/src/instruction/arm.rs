//! Structural representation of a decoded 32-bit ARM instruction.
//!
//! The guiding rule for everything in this file: **the decoder preserves the
//! information in the encoding perfectly and calculates nothing early.** We
//! store the fields exactly as they appear (register indices, immediates,
//! shift amounts, direction/writeback bits) and leave every derived quantity —
//! rotated immediates, sign-extended branch targets, the effective address of a
//! load — to the interpreter. That keeps the decode layer lossless, which the
//! disassembler and the time-travel debugger both depend on.

use crate::condition::Condition;
use crate::register::Register;

/// A fully decoded ARM instruction: its condition plus the operation it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArmInstruction {
    pub condition: Condition,
    pub operation: ArmOperation,
}

/// The operation an ARM instruction performs, one variant per encoding class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmOperation {
    DataProcessing(DataProcessing),
    Multiply(Multiply),
    MultiplyLong(MultiplyLong),

    SingleTransfer(SingleTransfer),
    HalfwordTransfer(HalfwordTransfer),
    BlockTransfer(BlockTransfer),

    Branch(Branch),
    BranchExchange(BranchExchange),

    SoftwareInterrupt(SoftwareInterrupt),

    Mrs(Mrs),
    Msr(Msr),

    /// Anything the decoder does not assign to a known class: architecturally
    /// undefined encodings, and coprocessor space we don't model. The raw word
    /// is retained so nothing is lost.
    Undefined {
        raw: u32,
    },
}

// ---------------------------------------------------------------------------
// Data processing
// ---------------------------------------------------------------------------

/// ALU operations (`AND`, `ADD`, `MOV`, `CMP`, ...) — the largest class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataProcessing {
    pub opcode: DataProcessingOpcode,
    /// The `S` bit: whether the operation updates the CPSR flags.
    pub set_flags: bool,
    /// Destination register.
    pub rd: Register,
    /// First operand register.
    pub rn: Register,
    /// The flexible second operand.
    pub operand2: Operand2,
}

/// The 16 data-processing opcodes (bits 24..21).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataProcessingOpcode {
    And, // 0000
    Eor, // 0001
    Sub, // 0010
    Rsb, // 0011
    Add, // 0100
    Adc, // 0101
    Sbc, // 0110
    Rsc, // 0111
    Tst, // 1000
    Teq, // 1001
    Cmp, // 1010
    Cmn, // 1011
    Orr, // 1100
    Mov, // 1101
    Bic, // 1110
    Mvn, // 1111
}

/// The "flexible second operand" shared by data-processing instructions.
///
/// We keep the immediate's `value`/`rotate` split rather than folding it into a
/// single rotated constant, and we keep register shifts in their raw form so
/// that, for example, `ROR #0` (which the CPU treats as `RRX`) and `LSL #0`
/// (no shift) remain distinguishable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operand2 {
    Immediate {
        /// The 8-bit immediate.
        value: u8,
        /// The 4-bit rotate field; the effective constant is `value` rotated
        /// right by `rotate * 2`.
        rotate: u8,
    },
    Register {
        rm: Register,
        shift: Shift,
    },
}

/// A barrel-shifter operation applied to a register operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shift {
    pub kind: ShiftKind,
    pub source: ShiftSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShiftKind {
    Lsl, // 00
    Lsr, // 01
    Asr, // 10
    Ror, // 11
}

/// Where a shift amount comes from. Kept raw so the special cases
/// (`LSL #0`, `LSR #32` encoded as `#0`, `ROR #0` == `RRX`) survive decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShiftSource {
    /// A 5-bit immediate shift amount (0..=31), interpreted per `ShiftKind`.
    Immediate(u8),
    /// A register whose low byte gives the shift amount.
    Register(Register),
}

// ---------------------------------------------------------------------------
// Multiply
// ---------------------------------------------------------------------------

/// `MUL` / `MLA` — 32-bit multiply, optionally accumulating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Multiply {
    /// The `A` bit: accumulate `rn` into the product (`MLA` vs `MUL`).
    pub accumulate: bool,
    pub set_flags: bool,
    pub rd: Register,
    /// Accumulator operand; only meaningful when `accumulate` is set.
    pub rn: Register,
    pub rs: Register,
    pub rm: Register,
}

/// `UMULL` / `UMLAL` / `SMULL` / `SMLAL` — 64-bit multiply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultiplyLong {
    /// Signed (`SMULL`/`SMLAL`) vs unsigned (`UMULL`/`UMLAL`).
    pub signed: bool,
    pub accumulate: bool,
    pub set_flags: bool,
    /// High 32 bits of the result / accumulator.
    pub rd_hi: Register,
    /// Low 32 bits of the result / accumulator.
    pub rd_lo: Register,
    pub rs: Register,
    pub rm: Register,
}

// ---------------------------------------------------------------------------
// Memory transfers
// ---------------------------------------------------------------------------

/// `LDR` / `STR` — single word/byte transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SingleTransfer {
    /// Load (`LDR`) vs store (`STR`).
    pub load: bool,
    /// Byte (`B`) vs word access.
    pub byte: bool,
    /// Pre-indexed (`P` set) vs post-indexed addressing.
    pub pre_indexed: bool,
    /// Add (`U` set) vs subtract the offset.
    pub add: bool,
    /// Writeback (`W`): update the base register with the computed address.
    pub writeback: bool,
    pub rn: Register,
    pub rd: Register,
    pub offset: SingleOffset,
}

/// The offset for a single data transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SingleOffset {
    /// 12-bit immediate offset.
    Immediate(u16),
    /// Scaled register offset. The shift here always has an immediate source
    /// (this class cannot use a register-specified shift amount).
    Register { rm: Register, shift: Shift },
}

/// `LDRH`/`STRH`/`LDRSB`/`LDRSH` — halfword and signed-byte transfers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HalfwordTransfer {
    pub load: bool,
    pub pre_indexed: bool,
    pub add: bool,
    pub writeback: bool,
    /// Which access width/signedness (the `SH` field).
    pub kind: HalfwordKind,
    pub rn: Register,
    pub rd: Register,
    pub offset: HalfwordOffset,
}

/// The `SH` field of a halfword/signed transfer. `SH == 00` is not a transfer
/// (it decodes as SWP/multiply) and so has no variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HalfwordKind {
    UnsignedHalfword, // 01
    SignedByte,       // 10
    SignedHalfword,   // 11
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HalfwordOffset {
    /// 8-bit immediate, split across two nibbles in the encoding but stored
    /// recombined here.
    Immediate(u8),
    Register(Register),
}

/// `LDM` / `STM` — block (multiple register) transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockTransfer {
    pub load: bool,
    pub pre_indexed: bool,
    pub add: bool,
    pub writeback: bool,
    /// The `S` bit: transfer the user-mode bank / restore CPSR from SPSR.
    pub psr_force_user: bool,
    pub rn: Register,
    /// One bit per register r0..r15.
    pub register_list: u16,
}

// ---------------------------------------------------------------------------
// Control flow
// ---------------------------------------------------------------------------

/// `B` / `BL` — PC-relative branch, optionally saving a return address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Branch {
    /// Whether to write the return address to LR (`BL`).
    pub link: bool,
    /// The 24-bit signed word offset, sign-extended and multiplied by 4 to a
    /// byte offset. This `<< 2` is inherent to the encoding (not a runtime
    /// value), so we bake it in; the interpreter still adds the pipeline's
    /// `PC + 8`.
    pub offset: i32,
}

/// `BX` — branch and exchange instruction set (ARM <-> Thumb).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BranchExchange {
    pub rn: Register,
}

/// `SWI` — software interrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoftwareInterrupt {
    /// The 24-bit comment field. Ignored by the CPU but often used by the BIOS
    /// to select a service, so we keep it.
    pub comment: u32,
}

// ---------------------------------------------------------------------------
// Status register access
// ---------------------------------------------------------------------------

/// `MRS` — move a status register into a general register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mrs {
    /// Source is SPSR (`true`) or CPSR (`false`).
    pub source_spsr: bool,
    pub rd: Register,
}

/// `MSR` — move a general register or immediate into a status register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Msr {
    /// Destination is SPSR (`true`) or CPSR (`false`).
    pub dest_spsr: bool,
    /// `f` field mask — PSR bits 31..24 (the condition flags byte).
    pub write_flags: bool,
    /// `s` field mask — PSR bits 23..16.
    pub write_status: bool,
    /// `x` field mask — PSR bits 15..8.
    pub write_extension: bool,
    /// `c` field mask — PSR bits 7..0 (the control byte: mode/IRQ/Thumb).
    pub write_control: bool,
    pub source: MsrSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsrSource {
    Register(Register),
    Immediate { value: u8, rotate: u8 },
}
