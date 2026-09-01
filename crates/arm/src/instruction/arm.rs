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

    /// ARMv5TE-only arithmetic; traps as Undefined on an ARMv4T core.
    CountLeadingZeros(CountLeadingZeros),
    SaturatingArithmetic(SaturatingArithmetic),
    HalfwordMultiply(HalfwordMultiply),

    SingleTransfer(SingleTransfer),
    HalfwordTransfer(HalfwordTransfer),

    /// ARMv5TE-only doubleword transfer; traps as Undefined on an ARMv4T core.
    DoublewordTransfer(DoublewordTransfer),

    BlockTransfer(BlockTransfer),
    Swap(Swap),

    Branch(Branch),
    BranchExchange(BranchExchange),
    BranchLinkExchange(BranchLinkExchange),
    Breakpoint(Breakpoint),

    /// `MRC`/`MCR` — ARM ↔ coprocessor register transfer (the ARM9's CP15 system
    /// control interface). ARMv5TE-only; traps as Undefined on an ARMv4T core.
    CoprocessorRegisterTransfer(CoprocessorRegisterTransfer),

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

impl ArmOperation {
    /// Whether this is a dedicated control-transfer instruction — one whose
    /// purpose is to redirect execution regardless of its operands (`B`/`BL`,
    /// `BX`, `SWI`).
    ///
    /// Instructions that merely *can* write the PC as a side effect (`LDR pc`,
    /// `MOV pc`, `LDM {…, pc}`) are not control-transfer instructions and are
    /// reported by [`ArmOperation::may_modify_pc`] instead. Undefined encodings
    /// trap, but are a decode catch-all rather than a control-flow instruction,
    /// so they are excluded here.
    pub fn is_control_flow(&self) -> bool {
        matches!(
            self,
            ArmOperation::Branch(_)
                | ArmOperation::BranchExchange(_)
                | ArmOperation::BranchLinkExchange(_)
                | ArmOperation::Breakpoint(_)
                | ArmOperation::SoftwareInterrupt(_)
        )
    }

    /// Whether executing this operation, per its defined semantics, can write a
    /// new value to r15 (the PC) and so redirect execution.
    ///
    /// This is the superset [`ArmOperation::is_control_flow`] belongs to: it
    /// adds every result-writing instruction whose destination is r15. It is the
    /// question a code generator asks to decide whether an instruction can end a
    /// straight-line run of code.
    pub fn may_modify_pc(&self) -> bool {
        match self {
            ArmOperation::Branch(_)
            | ArmOperation::BranchExchange(_)
            | ArmOperation::BranchLinkExchange(_)
            | ArmOperation::Breakpoint(_)
            | ArmOperation::SoftwareInterrupt(_) => true,

            // Result-writing operations redirect execution only when their
            // destination is r15.
            ArmOperation::DataProcessing(op) => op.rd.is_pc() && !op.opcode.is_comparison(),
            ArmOperation::Mrs(op) => op.rd.is_pc(),
            ArmOperation::SingleTransfer(op) => op.load && op.rd.is_pc(),
            ArmOperation::HalfwordTransfer(op) => op.load && op.rd.is_pc(),
            ArmOperation::Swap(op) => op.rd.is_pc(),
            // `LDRD` targets an even/odd register pair, so a defined `LDRD`
            // never writes r15 (an r14 base pair is architecturally
            // unpredictable rather than a defined PC write).
            ArmOperation::DoublewordTransfer(_) => false,
            ArmOperation::BlockTransfer(op) => op.load && op.register_list & (1 << 15) != 0,

            // Multiplies to r15 are unpredictable (not a defined PC write), MSR
            // targets a status register, the ARMv5 arithmetic ops to r15 are
            // unpredictable, and Undefined writes nothing itself.
            ArmOperation::Multiply(_)
            | ArmOperation::MultiplyLong(_)
            | ArmOperation::CountLeadingZeros(_)
            | ArmOperation::SaturatingArithmetic(_)
            | ArmOperation::HalfwordMultiply(_)
            // `MRC` to r15 updates the condition flags (APSR), never the PC.
            | ArmOperation::CoprocessorRegisterTransfer(_)
            | ArmOperation::Msr(_)
            | ArmOperation::Undefined { .. } => false,
        }
    }
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

impl DataProcessingOpcode {
    /// The four opcodes (`TST`/`TEQ`/`CMP`/`CMN`) that only update the flags and
    /// never write a destination register.
    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            DataProcessingOpcode::Tst
                | DataProcessingOpcode::Teq
                | DataProcessingOpcode::Cmp
                | DataProcessingOpcode::Cmn
        )
    }
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
// ARMv5TE arithmetic (ARM9)
// ---------------------------------------------------------------------------

/// `CLZ` — count leading zeros of `rm` into `rd` (0..=32). ARMv5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CountLeadingZeros {
    pub rd: Register,
    pub rm: Register,
}

/// The four saturating add/subtract operations (`QADD`/`QSUB`/`QDADD`/`QDSUB`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaturatingOp {
    QAdd,  // Rd = Rm + Rn
    QSub,  // Rd = Rm - Rn
    QDAdd, // Rd = Rm + Rn*2
    QDSub, // Rd = Rm - Rn*2
}

/// `QADD` / `QSUB` / `QDADD` / `QDSUB` — signed saturating arithmetic. The result
/// clamps to the signed 32-bit range and any saturation sets the sticky `Q` flag.
/// The `QD` variants first double `rn` (also saturating). ARMv5TE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaturatingArithmetic {
    pub op: SaturatingOp,
    pub rd: Register,
    /// First operand (bits 3..0) — the value added to / subtracted from.
    pub rm: Register,
    /// Second operand (bits 19..16) — doubled first in the `QD` variants.
    pub rn: Register,
}

/// The DSP 16×16 (and 32×16) signed multiply forms. `x`/`y` select which halfword
/// of `Rm`/`Rs` is used; the `W` forms multiply the full 32-bit `Rm` by a halfword.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DspMulOp {
    /// `SMLAxy` — `Rd = HalfRm*HalfRs + Rn` (32-bit accumulate, sets Q on overflow).
    SmlaXY,
    /// `SMLAWy` — `Rd = (Rm*HalfRs) >> 16 + Rn` (sets Q on overflow).
    SmlaWY,
    /// `SMULWy` — `Rd = (Rm*HalfRs) >> 16`.
    SmulWY,
    /// `SMLALxy` — `RdHi:RdLo += HalfRm*HalfRs` (64-bit, no Q).
    SmlalXY,
    /// `SMULxy` — `Rd = HalfRm*HalfRs`.
    SmulXY,
}

/// `SMULxy` / `SMLAxy` / `SMULWy` / `SMLAWy` / `SMLALxy` — ARMv5TE DSP multiplies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HalfwordMultiply {
    pub op: DspMulOp,
    /// `Rm` top-half select (unused by the `W` forms, where the bit chose the op).
    pub x: bool,
    /// `Rs` top-half select.
    pub y: bool,
    /// `Rd`, or `RdHi` for `SMLALxy`.
    pub rd: Register,
    /// `Rn` accumulator, or `RdLo` for `SMLALxy`.
    pub rn: Register,
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

/// `LDRD`/`STRD` — doubleword (two-register) transfer (ARMv5TE). Moves the
/// even/odd register pair `rd` and `rd`+1 as two words. Undefined on ARMv4T.
///
/// It shares the extra-load/store addressing shape with [`HalfwordTransfer`],
/// but the load/store choice is *not* the usual `L` bit (which is 0 for both);
/// it is the `H` bit of the `SH` field, captured here as `store`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoublewordTransfer {
    /// `false` = `LDRD` (load), `true` = `STRD` (store).
    pub store: bool,
    pub pre_indexed: bool,
    pub add: bool,
    pub writeback: bool,
    pub rn: Register,
    /// The first (even) register of the pair; `rd`+1 is the second.
    pub rd: Register,
    pub offset: HalfwordOffset,
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

/// `SWP` / `SWPB` — atomic swap of a register with a memory word/byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Swap {
    /// Byte (`SWPB`) vs word swap.
    pub byte: bool,
    /// Base register holding the address.
    pub rn: Register,
    /// Destination register (receives the old memory contents).
    pub rd: Register,
    /// Source register (written to memory).
    pub rm: Register,
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

/// `BX` / `BLX Rn` — branch and exchange instruction set (ARM <-> Thumb). `BLX`
/// also saves the return address in LR and is ARMv5-only (traps on ARMv4T).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BranchExchange {
    pub rn: Register,
    /// The `L` bit: `BLX` saves a return address (`BX` does not).
    pub link: bool,
}

/// `BLX <label>` — PC-relative branch-with-link that switches to Thumb. ARMv5-only;
/// a no-op on ARMv4T (it lives in the `cond == 1111` never-execute space). The
/// offset already folds in the encoding's `<< 2` and the halfword (`H`) bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BranchLinkExchange {
    pub offset: i32,
}

/// `BKPT` — software breakpoint; enters the Prefetch Abort vector. ARMv5-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Breakpoint {
    pub comment: u16,
}

/// `MRC`/`MCR` — move a word between an ARM register and a coprocessor register
/// (ARMv5TE). The five coprocessor selectors (`cp_num` plus the two opcodes and
/// two coprocessor-register fields) name a location within the coprocessor; the
/// `arm` crate stays device-agnostic and forwards them through the `Bus`. `CDP`,
/// `LDC`, `STC`, `MCRR`, and `MRRC` are not modelled (CP15 does not use them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoprocessorRegisterTransfer {
    /// `true` = `MRC` (coprocessor → ARM register), `false` = `MCR`.
    pub load: bool,
    pub cp_num: u8,
    pub opcode1: u8,
    pub opcode2: u8,
    pub crn: u8,
    pub crm: u8,
    pub rd: Register,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_flow_instructions_are_both() {
        for op in [
            ArmOperation::Branch(Branch {
                link: true,
                offset: 0,
            }),
            ArmOperation::BranchExchange(BranchExchange {
                rn: Register::LR,
                link: false,
            }),
            ArmOperation::SoftwareInterrupt(SoftwareInterrupt { comment: 0 }),
        ] {
            assert!(op.is_control_flow());
            assert!(op.may_modify_pc());
        }
    }

    #[test]
    fn pc_writes_modify_pc_but_are_not_control_flow() {
        // MOV pc, lr
        let mov_pc = ArmOperation::DataProcessing(DataProcessing {
            opcode: DataProcessingOpcode::Mov,
            set_flags: false,
            rd: Register::PC,
            rn: Register::new(0),
            operand2: Operand2::Register {
                rm: Register::LR,
                shift: Shift {
                    kind: ShiftKind::Lsl,
                    source: ShiftSource::Immediate(0),
                },
            },
        });
        assert!(mov_pc.may_modify_pc());
        assert!(!mov_pc.is_control_flow());

        // LDR pc, [r0]
        let ldr_pc = ArmOperation::SingleTransfer(SingleTransfer {
            load: true,
            byte: false,
            pre_indexed: true,
            add: true,
            writeback: false,
            rn: Register::new(0),
            rd: Register::PC,
            offset: SingleOffset::Immediate(0),
        });
        assert!(ldr_pc.may_modify_pc());
        assert!(!ldr_pc.is_control_flow());

        // LDM sp!, {pc}
        let ldm_pc = ArmOperation::BlockTransfer(BlockTransfer {
            load: true,
            pre_indexed: false,
            add: true,
            writeback: true,
            psr_force_user: false,
            rn: Register::SP,
            register_list: 1 << 15,
        });
        assert!(ldm_pc.may_modify_pc());
        assert!(!ldm_pc.is_control_flow());
    }

    #[test]
    fn non_pc_writes_do_not_modify_pc() {
        // ADD r0, r0, #1
        let add = ArmOperation::DataProcessing(DataProcessing {
            opcode: DataProcessingOpcode::Add,
            set_flags: false,
            rd: Register::new(0),
            rn: Register::new(0),
            operand2: Operand2::Immediate {
                value: 1,
                rotate: 0,
            },
        });
        assert!(!add.may_modify_pc());

        // A comparison never writes a register, even if the rd field reads r15.
        let cmp = ArmOperation::DataProcessing(DataProcessing {
            opcode: DataProcessingOpcode::Cmp,
            set_flags: true,
            rd: Register::PC,
            rn: Register::new(0),
            operand2: Operand2::Immediate {
                value: 0,
                rotate: 0,
            },
        });
        assert!(!cmp.may_modify_pc());

        // A store to r15 writes memory, not the PC.
        let str_pc = ArmOperation::SingleTransfer(SingleTransfer {
            load: false,
            byte: false,
            pre_indexed: true,
            add: true,
            writeback: false,
            rn: Register::new(0),
            rd: Register::PC,
            offset: SingleOffset::Immediate(0),
        });
        assert!(!str_pc.may_modify_pc());

        // A load that does not include r15 in its list.
        let ldm = ArmOperation::BlockTransfer(BlockTransfer {
            load: true,
            pre_indexed: false,
            add: true,
            writeback: true,
            psr_force_user: false,
            rn: Register::SP,
            register_list: 0x00FF,
        });
        assert!(!ldm.may_modify_pc());

        // Undefined writes nothing itself.
        let undef = ArmOperation::Undefined { raw: 0 };
        assert!(!undef.may_modify_pc());
        assert!(!undef.is_control_flow());
    }
}
