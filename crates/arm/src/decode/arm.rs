//! ARM (32-bit) instruction decoder.
//!
//! Decoding proceeds in two steps. First the 4-bit condition (bits 31..28) is
//! peeled off. Then the operation is classified.
//!
//! Classification cannot be a simple `match` on the class field (bits 27..25),
//! because several distinct instructions hide inside what otherwise looks like
//! the data-processing encoding space — `BX`, the multiplies, the single-data
//! swap, and the halfword/signed transfers all live in the `000` region and are
//! distinguished only by scattered fixed bits. So we test those specific
//! encodings first, in a fixed order, using masks; only once they are ruled out
//! do we fall back to the coarse class field.
//!
//! The order of the leading tests matters: each pattern is checked before any
//! broader pattern that would also match it.

use crate::condition::Condition;
use crate::decode::operand::{decode_operand2, decode_shift};
use crate::instruction::arm::{
    ArmInstruction, ArmOperation, BlockTransfer, Branch, BranchExchange, BranchLinkExchange,
    Breakpoint, CoprocessorRegisterTransfer, CountLeadingZeros, DataProcessing,
    DataProcessingOpcode, DoublewordTransfer, DspMulOp, HalfwordKind, HalfwordMultiply,
    HalfwordOffset, HalfwordTransfer, Mrs, Msr, MsrSource, Multiply, MultiplyLong,
    SaturatingArithmetic, SaturatingOp, SingleOffset, SingleTransfer, SoftwareInterrupt, Swap,
};
use crate::register::Register;

/// Decode a 32-bit ARM instruction word.
pub fn decode_arm(raw: u32) -> ArmInstruction {
    // `cond == 1111` is the ARMv5 unconditional-instruction space (BLX <label>,
    // PLD, …). On ARMv4T it means "never execute", so anything here that we do not
    // model stays a NOP via the `Nv` condition.
    if raw >> 28 == 0xF {
        return if (raw >> 25) & 0b111 == 0b101 {
            ArmInstruction { condition: Condition::Al, operation: decode_blx_immediate(raw) }
        } else {
            // PLD and reserved hints: never-execute keeps them NOP on both cores.
            ArmInstruction { condition: Condition::Nv, operation: ArmOperation::Undefined { raw } }
        };
    }
    ArmInstruction {
        condition: Condition::decode(raw >> 28),
        operation: decode_operation(raw),
    }
}

/// `BLX <label>`: `1111 101H imm24` — a link-and-exchange PC-relative branch. The
/// 24-bit offset is sign-extended and scaled by 4; the `H` bit adds a halfword.
fn decode_blx_immediate(raw: u32) -> ArmOperation {
    let offset = ((raw << 8) as i32 >> 6) | (((raw >> 24) & 1) as i32) << 1;
    ArmOperation::BranchLinkExchange(BranchLinkExchange { offset })
}

/// Classify and decode the operation portion of an ARM word (everything below
/// the condition).
fn decode_operation(raw: u32) -> ArmOperation {
    if matches_branch_exchange(raw) {
        decode_branch_exchange(raw)
    } else if matches_breakpoint(raw) {
        decode_breakpoint(raw)
    } else if matches_clz(raw) {
        decode_clz(raw)
    } else if matches_saturating(raw) {
        decode_saturating(raw)
    } else if matches_halfword_multiply(raw) {
        decode_halfword_multiply(raw)
    } else if matches_swap(raw) {
        decode_swap(raw)
    } else if matches_multiply(raw) {
        decode_multiply(raw)
    } else if matches_multiply_long(raw) {
        decode_multiply_long(raw)
    } else if matches_doubleword_transfer(raw) {
        decode_doubleword_transfer(raw)
    } else if matches_halfword_transfer(raw) {
        decode_halfword_transfer(raw)
    } else {
        match (raw >> 25) & 0b111 {
            0b000 | 0b001 => decode_data_processing(raw),
            0b010 | 0b011 => decode_single_transfer(raw),
            0b100 => decode_block_transfer(raw),
            0b101 => decode_branch(raw),
            // 0b110: coprocessor data transfer (LDC/STC) — not modelled.
            0b110 => ArmOperation::Undefined { raw },
            0b111 => decode_swi_or_coprocessor(raw),
            _ => unreachable!(),
        }
    }
}

// ---------------------------------------------------------------------------
// Class matchers
//
// Each predicate isolates one encoding by masking the fixed bits that identify
// it and comparing against their required value. The tables in the ARM7TDMI
// reference give these bit patterns; the masks below encode them directly.
// ---------------------------------------------------------------------------

/// `BX`/`BLX Rn`: `cond 0001 0010 1111 1111 1111 00L1 Rn` (`L` = bit 5, masked out
/// here so both match; the decoder reads it for the link).
fn matches_branch_exchange(raw: u32) -> bool {
    (raw & 0x0FFF_FFD0) == 0x012F_FF10
}

/// `BKPT`: `cond 0001 0010 imm12 0111 imm4` (ARMv5).
fn matches_breakpoint(raw: u32) -> bool {
    (raw & 0x0FF0_00F0) == 0x0120_0070
}

/// `CLZ Rd,Rm`: `cond 0001 0110 1111 Rd 1111 0001 Rm` (ARMv5).
fn matches_clz(raw: u32) -> bool {
    (raw & 0x0FFF_0FF0) == 0x016F_0F10
}

/// `QADD`/`QSUB`/`QDADD`/`QDSUB`: `cond 0001 0oo0 Rn Rd 0000 0101 Rm` (ARMv5TE).
fn matches_saturating(raw: u32) -> bool {
    (raw & 0x0F90_0FF0) == 0x0100_0050
}

/// `SWP`/`SWPB`: `cond 0001 0B00 Rn Rd 0000 1001 Rm`.
fn matches_swap(raw: u32) -> bool {
    (raw & 0x0FB0_0FF0) == 0x0100_0090
}

fn decode_clz(raw: u32) -> ArmOperation {
    ArmOperation::CountLeadingZeros(CountLeadingZeros {
        rd: Register::new((raw >> 12) as u8),
        rm: Register::new(raw as u8),
    })
}

fn decode_saturating(raw: u32) -> ArmOperation {
    let op = match (raw >> 21) & 0b11 {
        0b00 => SaturatingOp::QAdd,
        0b01 => SaturatingOp::QSub,
        0b10 => SaturatingOp::QDAdd,
        _ => SaturatingOp::QDSub,
    };
    ArmOperation::SaturatingArithmetic(SaturatingArithmetic {
        op,
        rd: Register::new((raw >> 12) as u8),
        rm: Register::new(raw as u8),
        rn: Register::new((raw >> 16) as u8),
    })
}

/// DSP halfword multiplies: `cond 0001 0oo0 Rd Rn Rs 1yx0 Rm` (ARMv5TE).
fn matches_halfword_multiply(raw: u32) -> bool {
    (raw & 0x0F90_0090) == 0x0100_0080
}

fn decode_halfword_multiply(raw: u32) -> ArmOperation {
    let x = raw & (1 << 5) != 0;
    let y = raw & (1 << 6) != 0;
    let op = match (raw >> 21) & 0b11 {
        0b00 => DspMulOp::SmlaXY,
        0b01 if x => DspMulOp::SmulWY,
        0b01 => DspMulOp::SmlaWY,
        0b10 => DspMulOp::SmlalXY,
        _ => DspMulOp::SmulXY,
    };
    ArmOperation::HalfwordMultiply(HalfwordMultiply {
        op,
        x,
        y,
        rd: Register::new((raw >> 16) as u8),
        rn: Register::new((raw >> 12) as u8),
        rs: Register::new((raw >> 8) as u8),
        rm: Register::new(raw as u8),
    })
}

/// `MUL`/`MLA`: `cond 0000 00AS Rd Rn Rs 1001 Rm`.
fn matches_multiply(raw: u32) -> bool {
    (raw & 0x0FC0_00F0) == 0x0000_0090
}

/// `UMULL`/`UMLAL`/`SMULL`/`SMLAL`: `cond 0000 1UAS RdHi RdLo Rs 1001 Rm`.
fn matches_multiply_long(raw: u32) -> bool {
    (raw & 0x0F80_00F0) == 0x0080_0090
}

/// Halfword / signed-byte transfers: bits 27..25 = `000`, bit 7 = 1, bit 4 = 1,
/// and `SH` (bits 6..5) != `00`. The `SH == 00` case is SWP/multiply and is
/// excluded here (and matched earlier), so this predicate is order-independent.
fn matches_halfword_transfer(raw: u32) -> bool {
    (raw & 0x0E00_0090) == 0x0000_0090 && (raw & 0x0000_0060) != 0
}

/// `LDRD`/`STRD` (ARMv5TE): the extra-load/store slot with `L` (bit 20) = 0 and
/// `S` (bit 6) = 1, i.e. `SH` = 10 (`LDRD`) or 11 (`STRD`). It must be tried
/// before [`matches_halfword_transfer`], which would otherwise capture these as
/// nonexistent "store signed byte/halfword".
fn matches_doubleword_transfer(raw: u32) -> bool {
    (raw & 0x0E10_00D0) == 0x0000_00D0
}

// ---------------------------------------------------------------------------
// Per-class decoders.
//
// The classification skeleton above is complete; the bodies that build each
// operation's fields are not yet implemented and currently preserve the raw
// word as `Undefined`.
// ---------------------------------------------------------------------------

fn decode_data_processing(raw: u32) -> ArmOperation {
    let opcode = decode_dp_opcode(raw >> 21);
    let set_flags = raw & (1 << 20) != 0;
    let immediate = raw & (1 << 25) != 0;

    // `TST`/`TEQ`/`CMP`/`CMN` exist only to set flags, so they are always
    // encoded with S set. The S-clear encoding of those four opcodes is reused
    // for status-register access (MRS/MSR), which we split out here.
    if !set_flags && is_status_opcode(opcode) {
        return decode_psr_transfer(raw, immediate);
    }

    ArmOperation::DataProcessing(DataProcessing {
        opcode,
        set_flags,
        rd: Register::new((raw >> 12) as u8),
        rn: Register::new((raw >> 16) as u8),
        operand2: decode_operand2(raw, immediate),
    })
}

/// Decode the 4-bit data-processing opcode (bits 24..21).
fn decode_dp_opcode(bits: u32) -> DataProcessingOpcode {
    use DataProcessingOpcode::*;
    match bits & 0xF {
        0x0 => And,
        0x1 => Eor,
        0x2 => Sub,
        0x3 => Rsb,
        0x4 => Add,
        0x5 => Adc,
        0x6 => Sbc,
        0x7 => Rsc,
        0x8 => Tst,
        0x9 => Teq,
        0xA => Cmp,
        0xB => Cmn,
        0xC => Orr,
        0xD => Mov,
        0xE => Bic,
        0xF => Mvn,
        _ => unreachable!(),
    }
}

/// Whether an opcode is one of the four comparison ops (`TST`/`TEQ`/`CMP`/`CMN`)
/// whose S-clear encoding is repurposed for MRS/MSR.
fn is_status_opcode(opcode: DataProcessingOpcode) -> bool {
    use DataProcessingOpcode::*;
    matches!(opcode, Tst | Teq | Cmp | Cmn)
}

/// Decode the MRS/MSR encodings that share the S-clear comparison opcode space.
/// Bit 21 selects between them: clear is `MRS` (read PSR), set is `MSR` (write
/// PSR). Bit 22 selects CPSR vs SPSR.
fn decode_psr_transfer(raw: u32, immediate: bool) -> ArmOperation {
    let spsr = raw & (1 << 22) != 0;
    if raw & (1 << 21) == 0 {
        ArmOperation::Mrs(Mrs {
            source_spsr: spsr,
            rd: Register::new((raw >> 12) as u8),
        })
    } else {
        let source = if immediate {
            MsrSource::Immediate {
                value: raw as u8,
                rotate: (raw >> 8) as u8 & 0xF,
            }
        } else {
            MsrSource::Register(Register::new(raw as u8))
        };
        ArmOperation::Msr(Msr {
            dest_spsr: spsr,
            write_flags: raw & (1 << 19) != 0,
            write_status: raw & (1 << 18) != 0,
            write_extension: raw & (1 << 17) != 0,
            write_control: raw & (1 << 16) != 0,
            source,
        })
    }
}

/// `LDR` / `STR` — single word/byte transfer.
///
/// Note the `I` bit (bit 25) has the *opposite* sense to data processing here:
/// set means a register offset, clear means an immediate offset.
fn decode_single_transfer(raw: u32) -> ArmOperation {
    let register_offset = raw & (1 << 25) != 0;

    // The register-offset form only permits an immediate shift amount (bit 4
    // clear). Bit 4 set is not a valid single transfer on this core, so it
    // falls through to undefined rather than being decoded as a register shift.
    if register_offset && raw & (1 << 4) != 0 {
        return ArmOperation::Undefined { raw };
    }

    let offset = if register_offset {
        SingleOffset::Register {
            rm: Register::new(raw as u8),
            shift: decode_shift(raw),
        }
    } else {
        SingleOffset::Immediate((raw & 0xFFF) as u16)
    };

    ArmOperation::SingleTransfer(SingleTransfer {
        load: raw & (1 << 20) != 0,
        byte: raw & (1 << 22) != 0,
        pre_indexed: raw & (1 << 24) != 0,
        add: raw & (1 << 23) != 0,
        writeback: raw & (1 << 21) != 0,
        rn: Register::new((raw >> 16) as u8),
        rd: Register::new((raw >> 12) as u8),
        offset,
    })
}

/// `LDRH` / `STRH` / `LDRSB` / `LDRSH` — halfword and signed-byte transfers.
///
/// The offset is a register (bit 22 clear) or an 8-bit immediate split across
/// two nibbles (bit 22 set); the `SH` field (bits 6..5) names the access.
fn decode_halfword_transfer(raw: u32) -> ArmOperation {
    let immediate = raw & (1 << 22) != 0;
    let offset = if immediate {
        let hi = (raw >> 8) & 0xF;
        let lo = raw & 0xF;
        HalfwordOffset::Immediate(((hi << 4) | lo) as u8)
    } else {
        HalfwordOffset::Register(Register::new(raw as u8))
    };

    ArmOperation::HalfwordTransfer(HalfwordTransfer {
        load: raw & (1 << 20) != 0,
        pre_indexed: raw & (1 << 24) != 0,
        add: raw & (1 << 23) != 0,
        writeback: raw & (1 << 21) != 0,
        kind: decode_halfword_kind(raw >> 5),
        rn: Register::new((raw >> 16) as u8),
        rd: Register::new((raw >> 12) as u8),
        offset,
    })
}

/// `LDRD` / `STRD` — doubleword (register-pair) transfer. Same addressing shape
/// as a halfword transfer; the `H` bit (bit 5) selects load vs store.
fn decode_doubleword_transfer(raw: u32) -> ArmOperation {
    let immediate = raw & (1 << 22) != 0;
    let offset = if immediate {
        let hi = (raw >> 8) & 0xF;
        let lo = raw & 0xF;
        HalfwordOffset::Immediate(((hi << 4) | lo) as u8)
    } else {
        HalfwordOffset::Register(Register::new(raw as u8))
    };

    ArmOperation::DoublewordTransfer(DoublewordTransfer {
        store: raw & (1 << 5) != 0,
        pre_indexed: raw & (1 << 24) != 0,
        add: raw & (1 << 23) != 0,
        writeback: raw & (1 << 21) != 0,
        rn: Register::new((raw >> 16) as u8),
        rd: Register::new((raw >> 12) as u8),
        offset,
    })
}

/// Decode the `SH` field (bits 6..5) of a halfword/signed transfer. `SH == 00`
/// is the swap encoding and is matched before reaching here.
fn decode_halfword_kind(bits: u32) -> HalfwordKind {
    match bits & 0b11 {
        0b01 => HalfwordKind::UnsignedHalfword,
        0b10 => HalfwordKind::SignedByte,
        0b11 => HalfwordKind::SignedHalfword,
        _ => unreachable!(),
    }
}

/// `LDM` / `STM` — block (multiple register) transfer. The 16-bit register list
/// occupies bits 15..0, one bit per register.
fn decode_block_transfer(raw: u32) -> ArmOperation {
    ArmOperation::BlockTransfer(BlockTransfer {
        load: raw & (1 << 20) != 0,
        pre_indexed: raw & (1 << 24) != 0,
        add: raw & (1 << 23) != 0,
        writeback: raw & (1 << 21) != 0,
        psr_force_user: raw & (1 << 22) != 0,
        rn: Register::new((raw >> 16) as u8),
        register_list: raw as u16,
    })
}

/// `MUL` / `MLA` — 32-bit multiply. Note the operand layout differs from the
/// data-processing classes: `Rn` (the accumulator) is in bits 15..12, not 19..16.
fn decode_multiply(raw: u32) -> ArmOperation {
    ArmOperation::Multiply(Multiply {
        accumulate: raw & (1 << 21) != 0,
        set_flags: raw & (1 << 20) != 0,
        rd: Register::new((raw >> 16) as u8),
        rn: Register::new((raw >> 12) as u8),
        rs: Register::new((raw >> 8) as u8),
        rm: Register::new(raw as u8),
    })
}

/// `UMULL` / `UMLAL` / `SMULL` / `SMLAL` — 64-bit multiply. Bit 22 selects
/// signed vs unsigned; the 64-bit result/accumulator spans `RdHi`:`RdLo`.
fn decode_multiply_long(raw: u32) -> ArmOperation {
    ArmOperation::MultiplyLong(MultiplyLong {
        signed: raw & (1 << 22) != 0,
        accumulate: raw & (1 << 21) != 0,
        set_flags: raw & (1 << 20) != 0,
        rd_hi: Register::new((raw >> 16) as u8),
        rd_lo: Register::new((raw >> 12) as u8),
        rs: Register::new((raw >> 8) as u8),
        rm: Register::new(raw as u8),
    })
}

fn decode_swap(raw: u32) -> ArmOperation {
    ArmOperation::Swap(Swap {
        byte: raw & (1 << 22) != 0,
        rn: Register::new((raw >> 16) as u8),
        rd: Register::new((raw >> 12) as u8),
        rm: Register::new(raw as u8),
    })
}

/// `B` / `BL` — PC-relative branch.
///
/// The 24-bit field is a signed word count. We sign-extend it and multiply by 4
/// to a byte offset (that `<< 2` is inherent to the encoding); the `PC + 8`
/// pipeline adjustment is left to the interpreter. Shifting the word left by 8
/// drops the condition/class/link bits and lands the sign bit in bit 31, so a
/// single arithmetic right shift by 6 does the extend-and-scale.
fn decode_branch(raw: u32) -> ArmOperation {
    ArmOperation::Branch(Branch {
        link: raw & (1 << 24) != 0,
        offset: ((raw << 8) as i32) >> 6,
    })
}

/// `BX` — branch and exchange. The target register is bits 3..0.
fn decode_branch_exchange(raw: u32) -> ArmOperation {
    ArmOperation::BranchExchange(BranchExchange {
        rn: Register::new(raw as u8),
        link: raw & (1 << 5) != 0,
    })
}

fn decode_breakpoint(raw: u32) -> ArmOperation {
    let comment = (((raw >> 8) & 0xFFF) << 4) | (raw & 0xF);
    ArmOperation::Breakpoint(Breakpoint { comment: comment as u16 })
}

fn decode_swi_or_coprocessor(raw: u32) -> ArmOperation {
    // Bits 27..24 == 1111 is `SWI`. Otherwise bits 27..24 == 1110 is coprocessor
    // space: bit 4 set is a register transfer (`MRC`/`MCR`, the CP15 interface);
    // bit 4 clear is `CDP`, which CP15 does not use and which traps.
    if (raw & 0x0F00_0000) == 0x0F00_0000 {
        decode_software_interrupt(raw)
    } else if raw & (1 << 4) != 0 {
        decode_coprocessor_register_transfer(raw)
    } else {
        ArmOperation::Undefined { raw }
    }
}

/// `MRC` / `MCR` — `cond 1110 opc1 L CRn Rd cp_num opc2 1 CRm`. Bit 20 (`L`)
/// selects `MRC` (load, from coprocessor) vs `MCR` (store, to coprocessor).
fn decode_coprocessor_register_transfer(raw: u32) -> ArmOperation {
    ArmOperation::CoprocessorRegisterTransfer(CoprocessorRegisterTransfer {
        load: raw & (1 << 20) != 0,
        cp_num: ((raw >> 8) & 0xF) as u8,
        opcode1: ((raw >> 21) & 0x7) as u8,
        opcode2: ((raw >> 5) & 0x7) as u8,
        crn: ((raw >> 16) & 0xF) as u8,
        crm: (raw & 0xF) as u8,
        rd: Register::new((raw >> 12) as u8),
    })
}

/// `SWI` — software interrupt. Bits 23..0 are the comment field, ignored by the
/// CPU but retained for the BIOS/handler.
fn decode_software_interrupt(raw: u32) -> ArmOperation {
    ArmOperation::SoftwareInterrupt(SoftwareInterrupt {
        comment: raw & 0x00FF_FFFF,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::condition::Condition;
    use crate::instruction::arm::{
        HalfwordKind, HalfwordOffset, HalfwordTransfer, Operand2, Shift, ShiftKind, ShiftSource,
        SingleOffset, SingleTransfer,
    };

    #[test]
    fn extracts_condition() {
        // `BXEQ r0` — condition nibble 0000.
        assert_eq!(decode_arm(0x012F_FF10).condition, Condition::Eq);
        // `B` with AL condition.
        assert_eq!(decode_arm(0xEA00_0000).condition, Condition::Al);
        // `SWINV` — condition nibble 1111.
        assert_eq!(decode_arm(0xFF00_0000).condition, Condition::Nv);
    }

    #[test]
    fn bx_is_matched() {
        assert!(matches_branch_exchange(0xE12F_FF1E)); // BX lr
        assert!(matches_branch_exchange(0x012F_FF13)); // BXEQ r3
        // A data-processing word must not look like BX.
        assert!(!matches_branch_exchange(0xE281_1001)); // ADD r1, r1, #1
    }

    #[test]
    fn multiply_family_is_matched() {
        assert!(matches_multiply(0xE001_0293)); // MUL r1, r3, r2
        assert!(matches_multiply(0xE021_3291)); // MLA r1, r1, r2, r3
        assert!(!matches_multiply_long(0xE001_0293));

        assert!(matches_multiply_long(0xE081_2394)); // UMULL
        assert!(matches_multiply_long(0xE0C1_2394)); // SMULL
        assert!(!matches_multiply(0xE081_2394));
    }

    #[test]
    fn swap_is_matched_and_not_confused() {
        assert!(matches_swap(0xE104_3092)); // SWP r3, r2, [r4]
        assert!(matches_swap(0xE144_3092)); // SWPB r3, r2, [r4]
        // A swap must not be picked up by the multiply or halfword matchers.
        assert!(!matches_multiply(0xE104_3092));
        assert!(!matches_halfword_transfer(0xE104_3092));
    }

    #[test]
    fn halfword_transfer_is_matched() {
        assert!(matches_halfword_transfer(0xE1D1_00B0)); // LDRH r0, [r1]
        assert!(matches_halfword_transfer(0xE1D1_00D0)); // LDRSB r0, [r1]
        assert!(matches_halfword_transfer(0xE1D1_00F0)); // LDRSH r0, [r1]
        // SWP has SH == 00 and must not be treated as a halfword transfer.
        assert!(!matches_halfword_transfer(0xE104_3092));
    }

    #[test]
    fn doubleword_transfer_is_matched_and_decodes() {
        // LDRD/STRD (L=0, SH=10/11) must be caught before the halfword matcher,
        // which would otherwise treat them as nonexistent signed stores.
        assert!(matches_doubleword_transfer(0xE1C2_00D0)); // LDRD r0, [r2]
        assert!(matches_doubleword_transfer(0xE1C2_00F0)); // STRD r0, [r2]
        // The L=1 signed loads are halfword transfers, not doubleword.
        assert!(!matches_doubleword_transfer(0xE1D1_00D0)); // LDRSB
        assert!(!matches_doubleword_transfer(0xE1D1_00F0)); // LDRSH

        let ArmOperation::DoublewordTransfer(ldrd) = decode_arm(0xE1C2_00D0).operation else {
            panic!("expected LDRD");
        };
        assert!(!ldrd.store);
        assert_eq!(ldrd.rd, Register::new(0));
        assert_eq!(ldrd.rn, Register::new(2));

        let ArmOperation::DoublewordTransfer(strd) = decode_arm(0xE1C2_00F0).operation else {
            panic!("expected STRD");
        };
        assert!(strd.store);
    }

    #[test]
    fn coprocessor_register_transfer_decodes() {
        // mcr p15, 0, r1, c1, c0, 0
        let ArmOperation::CoprocessorRegisterTransfer(mcr) = decode_arm(0xEE01_1F10).operation
        else {
            panic!("expected MCR");
        };
        assert!(!mcr.load);
        assert_eq!(mcr.cp_num, 15);
        assert_eq!(mcr.opcode1, 0);
        assert_eq!(mcr.opcode2, 0);
        assert_eq!(mcr.crn, 1);
        assert_eq!(mcr.crm, 0);
        assert_eq!(mcr.rd, Register::new(1));

        // mrc p15, 0, r2, c1, c0, 0 — same location, L bit set.
        let ArmOperation::CoprocessorRegisterTransfer(mrc) = decode_arm(0xEE11_2F10).operation
        else {
            panic!("expected MRC");
        };
        assert!(mrc.load);
        assert_eq!(mrc.rd, Register::new(2));

        // CDP (bit 4 clear) is not modelled and stays Undefined.
        assert!(matches!(
            decode_arm(0xEE01_1F00).operation,
            ArmOperation::Undefined { .. }
        ));
    }

    #[test]
    fn swap_decodes() {
        // SWP r3, r2, [r4]
        let op = decode_arm(0xE104_3092).operation;
        assert_eq!(
            op,
            ArmOperation::Swap(Swap {
                byte: false,
                rn: Register::new(4),
                rd: Register::new(3),
                rm: Register::new(2),
            })
        );
        // SWPB sets the byte flag.
        let ArmOperation::Swap(swapb) = decode_arm(0xE144_3092).operation else {
            panic!("expected swap");
        };
        assert!(swapb.byte);
    }

    #[test]
    fn dp_immediate_operand() {
        // MOV r0, #0x1F
        let op = decode_arm(0xE3A0_001F).operation;
        assert_eq!(
            op,
            ArmOperation::DataProcessing(DataProcessing {
                opcode: DataProcessingOpcode::Mov,
                set_flags: false,
                rd: Register::new(0),
                rn: Register::new(0),
                operand2: Operand2::Immediate {
                    value: 0x1F,
                    rotate: 0,
                },
            })
        );
    }

    #[test]
    fn dp_register_with_immediate_shift() {
        // ADD r0, r1, r2, LSL #3
        let op = decode_arm(0xE081_0182).operation;
        assert_eq!(
            op,
            ArmOperation::DataProcessing(DataProcessing {
                opcode: DataProcessingOpcode::Add,
                set_flags: false,
                rd: Register::new(0),
                rn: Register::new(1),
                operand2: Operand2::Register {
                    rm: Register::new(2),
                    shift: Shift {
                        kind: ShiftKind::Lsl,
                        source: ShiftSource::Immediate(3),
                    },
                },
            })
        );
    }

    #[test]
    fn dp_register_with_register_shift() {
        // MOV r0, r1, LSL r2
        let op = decode_arm(0xE1A0_0211).operation;
        assert_eq!(
            op,
            ArmOperation::DataProcessing(DataProcessing {
                opcode: DataProcessingOpcode::Mov,
                set_flags: false,
                rd: Register::new(0),
                rn: Register::new(0),
                operand2: Operand2::Register {
                    rm: Register::new(1),
                    shift: Shift {
                        kind: ShiftKind::Lsl,
                        source: ShiftSource::Register(Register::new(2)),
                    },
                },
            })
        );
    }

    #[test]
    fn dp_comparison_keeps_set_flags_and_is_not_psr() {
        // CMP r0, #1 — a real comparison (S set), not an MRS/MSR.
        let op = decode_arm(0xE350_0001).operation;
        assert_eq!(
            op,
            ArmOperation::DataProcessing(DataProcessing {
                opcode: DataProcessingOpcode::Cmp,
                set_flags: true,
                rd: Register::new(0),
                rn: Register::new(0),
                operand2: Operand2::Immediate {
                    value: 1,
                    rotate: 0,
                },
            })
        );
    }

    #[test]
    fn mrs_reads_cpsr_and_spsr() {
        // MRS r0, CPSR
        assert_eq!(
            decode_arm(0xE10F_0000).operation,
            ArmOperation::Mrs(Mrs {
                source_spsr: false,
                rd: Register::new(0),
            })
        );
        // MRS r0, SPSR
        assert_eq!(
            decode_arm(0xE14F_0000).operation,
            ArmOperation::Mrs(Mrs {
                source_spsr: true,
                rd: Register::new(0),
            })
        );
    }

    #[test]
    fn msr_register_all_fields() {
        // MSR CPSR_fsxc, r0 — every field-mask bit set.
        assert_eq!(
            decode_arm(0xE12F_F000).operation,
            ArmOperation::Msr(Msr {
                dest_spsr: false,
                write_flags: true,
                write_status: true,
                write_extension: true,
                write_control: true,
                source: MsrSource::Register(Register::new(0)),
            })
        );
    }

    #[test]
    fn msr_immediate_flags_only() {
        // MSR CPSR_f, #0xFF ROR 8  (field mask = flags byte only)
        assert_eq!(
            decode_arm(0xE328_F4FF).operation,
            ArmOperation::Msr(Msr {
                dest_spsr: false,
                write_flags: true,
                write_status: false,
                write_extension: false,
                write_control: false,
                source: MsrSource::Immediate {
                    value: 0xFF,
                    rotate: 4,
                },
            })
        );
    }

    #[test]
    fn single_transfer_immediate_offset() {
        // LDR r1, [r0, #4]
        assert_eq!(
            decode_arm(0xE590_1004).operation,
            ArmOperation::SingleTransfer(SingleTransfer {
                load: true,
                byte: false,
                pre_indexed: true,
                add: true,
                writeback: false,
                rn: Register::new(0),
                rd: Register::new(1),
                offset: SingleOffset::Immediate(4),
            })
        );
    }

    #[test]
    fn single_transfer_register_offset_post_indexed() {
        // STRB r2, [r3], -r4, LSL #2
        assert_eq!(
            decode_arm(0xE643_2104).operation,
            ArmOperation::SingleTransfer(SingleTransfer {
                load: false,
                byte: true,
                pre_indexed: false,
                add: false,
                writeback: false,
                rn: Register::new(3),
                rd: Register::new(2),
                offset: SingleOffset::Register {
                    rm: Register::new(4),
                    shift: Shift {
                        kind: ShiftKind::Lsl,
                        source: ShiftSource::Immediate(2),
                    },
                },
            })
        );
    }

    #[test]
    fn single_transfer_register_offset_with_bit4_is_undefined() {
        // LDR r0, [r1, r2] with bit 4 set is not a valid single transfer.
        assert_eq!(
            decode_arm(0xE791_0012).operation,
            ArmOperation::Undefined { raw: 0xE791_0012 }
        );
    }

    #[test]
    fn halfword_immediate_offset() {
        // LDRH r1, [r0, #4]
        assert_eq!(
            decode_arm(0xE1D0_10B4).operation,
            ArmOperation::HalfwordTransfer(HalfwordTransfer {
                load: true,
                pre_indexed: true,
                add: true,
                writeback: false,
                kind: HalfwordKind::UnsignedHalfword,
                rn: Register::new(0),
                rd: Register::new(1),
                offset: HalfwordOffset::Immediate(4),
            })
        );
    }

    #[test]
    fn halfword_immediate_recombines_nibbles() {
        // STRH r1, [r0, #0xB4] — offset high nibble 0xB, low nibble 0x4.
        let ArmOperation::HalfwordTransfer(t) = decode_arm(0xE1C0_1BB4).operation else {
            panic!("expected halfword transfer");
        };
        assert!(!t.load);
        assert_eq!(t.kind, HalfwordKind::UnsignedHalfword);
        assert_eq!(t.offset, HalfwordOffset::Immediate(0xB4));
    }

    #[test]
    fn halfword_register_offset_signed_byte() {
        // LDRSB r1, [r0, r2]
        assert_eq!(
            decode_arm(0xE190_10D2).operation,
            ArmOperation::HalfwordTransfer(HalfwordTransfer {
                load: true,
                pre_indexed: true,
                add: true,
                writeback: false,
                kind: HalfwordKind::SignedByte,
                rn: Register::new(0),
                rd: Register::new(1),
                offset: HalfwordOffset::Register(Register::new(2)),
            })
        );
    }

    #[test]
    fn branch_decodes_link_and_offset() {
        // B forward: offset field 0x0A -> byte offset 40.
        assert_eq!(
            decode_arm(0xEA00_000A).operation,
            ArmOperation::Branch(Branch {
                link: false,
                offset: 40,
            })
        );
        // BL with a negative field 0xFFFFFE -> byte offset -8.
        assert_eq!(
            decode_arm(0xEBFF_FFFE).operation,
            ArmOperation::Branch(Branch {
                link: true,
                offset: -8,
            })
        );
    }

    #[test]
    fn branch_exchange_decodes_register() {
        // BX lr
        assert_eq!(
            decode_arm(0xE12F_FF1E).operation,
            ArmOperation::BranchExchange(BranchExchange {
                rn: Register::new(14),
                link: false,
            })
        );
    }

    #[test]
    fn swi_decodes_comment() {
        assert_eq!(
            decode_arm(0xEF12_3456).operation,
            ArmOperation::SoftwareInterrupt(SoftwareInterrupt { comment: 0x12_3456 })
        );
    }

    #[test]
    fn multiply_decodes() {
        // MUL r1, r3, r2
        assert_eq!(
            decode_arm(0xE001_0293).operation,
            ArmOperation::Multiply(Multiply {
                accumulate: false,
                set_flags: false,
                rd: Register::new(1),
                rn: Register::new(0),
                rs: Register::new(2),
                rm: Register::new(3),
            })
        );
        // MLA r1, r1, r2, r3 — accumulator Rn is r3 (bits 15..12).
        assert_eq!(
            decode_arm(0xE021_3291).operation,
            ArmOperation::Multiply(Multiply {
                accumulate: true,
                set_flags: false,
                rd: Register::new(1),
                rn: Register::new(3),
                rs: Register::new(2),
                rm: Register::new(1),
            })
        );
        // MULS r0, r1, r2 — S bit set.
        let ArmOperation::Multiply(muls) = decode_arm(0xE010_0291).operation else {
            panic!("expected multiply");
        };
        assert!(muls.set_flags);
        assert!(!muls.accumulate);
    }

    #[test]
    fn multiply_long_decodes() {
        // UMULL r2, r1, r4, r3  (RdLo=r2, RdHi=r1)
        assert_eq!(
            decode_arm(0xE081_2394).operation,
            ArmOperation::MultiplyLong(MultiplyLong {
                signed: false,
                accumulate: false,
                set_flags: false,
                rd_hi: Register::new(1),
                rd_lo: Register::new(2),
                rs: Register::new(3),
                rm: Register::new(4),
            })
        );
        // SMLALS r2, r1, r4, r3 — signed, accumulate, and set-flags all set.
        assert_eq!(
            decode_arm(0xE0F1_2394).operation,
            ArmOperation::MultiplyLong(MultiplyLong {
                signed: true,
                accumulate: true,
                set_flags: true,
                rd_hi: Register::new(1),
                rd_lo: Register::new(2),
                rs: Register::new(3),
                rm: Register::new(4),
            })
        );
    }

    #[test]
    fn block_transfer_decodes() {
        // LDMFD sp!, {pc}  (post-increment, writeback, load)
        assert_eq!(
            decode_arm(0xE8BD_8000).operation,
            ArmOperation::BlockTransfer(BlockTransfer {
                load: true,
                pre_indexed: false,
                add: true,
                writeback: true,
                psr_force_user: false,
                rn: Register::SP,
                register_list: 0x8000,
            })
        );
        // STMFD sp!, {r0-r3, lr}  (pre-decrement, writeback, store)
        assert_eq!(
            decode_arm(0xE92D_400F).operation,
            ArmOperation::BlockTransfer(BlockTransfer {
                load: false,
                pre_indexed: true,
                add: false,
                writeback: true,
                psr_force_user: false,
                rn: Register::SP,
                register_list: 0x400F,
            })
        );
    }

    #[test]
    fn block_transfer_s_bit_is_psr_force_user() {
        // LDM r0, {r0, pc}^ — the S bit maps to psr_force_user.
        let ArmOperation::BlockTransfer(t) = decode_arm(0xE8D0_8001).operation else {
            panic!("expected block transfer");
        };
        assert!(t.psr_force_user);
    }

    #[test]
    fn decoded_pc_loads_report_may_modify_pc() {
        // Now that these classes decode, the classification helper sees through
        // real encodings, not just hand-built values.
        assert!(decode_arm(0xE8BD_8000).operation.may_modify_pc()); // LDMFD sp!, {pc}
        assert!(decode_arm(0xE1A0_F00E).operation.may_modify_pc()); // MOV pc, lr
        assert!(!decode_arm(0xE001_0293).operation.may_modify_pc()); // MUL — never PC
    }
}
