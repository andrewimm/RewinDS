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
use crate::instruction::arm::{ArmInstruction, ArmOperation, Swap};
use crate::register::Register;

/// Decode a 32-bit ARM instruction word.
pub fn decode_arm(raw: u32) -> ArmInstruction {
    let condition = Condition::decode(raw >> 28);
    let operation = decode_operation(raw);
    ArmInstruction {
        condition,
        operation,
    }
}

/// Classify and decode the operation portion of an ARM word (everything below
/// the condition).
fn decode_operation(raw: u32) -> ArmOperation {
    if matches_branch_exchange(raw) {
        decode_branch_exchange(raw)
    } else if matches_swap(raw) {
        decode_swap(raw)
    } else if matches_multiply(raw) {
        decode_multiply(raw)
    } else if matches_multiply_long(raw) {
        decode_multiply_long(raw)
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

/// `BX Rn`: `cond 0001 0010 1111 1111 1111 0001 Rn`.
fn matches_branch_exchange(raw: u32) -> bool {
    (raw & 0x0FFF_FFF0) == 0x012F_FF10
}

/// `SWP`/`SWPB`: `cond 0001 0B00 Rn Rd 0000 1001 Rm`.
fn matches_swap(raw: u32) -> bool {
    (raw & 0x0FB0_0FF0) == 0x0100_0090
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

// ---------------------------------------------------------------------------
// Per-class decoders.
//
// The classification skeleton above is complete; the bodies that build each
// operation's fields are not yet implemented and currently preserve the raw
// word as `Undefined`.
// ---------------------------------------------------------------------------

fn decode_data_processing(raw: u32) -> ArmOperation {
    // Also the home of MRS/MSR, which occupy the comparison opcodes with the
    // S bit clear and are split out here once implemented.
    ArmOperation::Undefined { raw }
}

fn decode_single_transfer(raw: u32) -> ArmOperation {
    ArmOperation::Undefined { raw }
}

fn decode_halfword_transfer(raw: u32) -> ArmOperation {
    ArmOperation::Undefined { raw }
}

fn decode_block_transfer(raw: u32) -> ArmOperation {
    ArmOperation::Undefined { raw }
}

fn decode_multiply(raw: u32) -> ArmOperation {
    ArmOperation::Undefined { raw }
}

fn decode_multiply_long(raw: u32) -> ArmOperation {
    ArmOperation::Undefined { raw }
}

fn decode_swap(raw: u32) -> ArmOperation {
    ArmOperation::Swap(Swap {
        byte: raw & (1 << 22) != 0,
        rn: Register::new((raw >> 16) as u8),
        rd: Register::new((raw >> 12) as u8),
        rm: Register::new(raw as u8),
    })
}

fn decode_branch(raw: u32) -> ArmOperation {
    ArmOperation::Undefined { raw }
}

fn decode_branch_exchange(raw: u32) -> ArmOperation {
    ArmOperation::Undefined { raw }
}

fn decode_swi_or_coprocessor(raw: u32) -> ArmOperation {
    // Bits 27..24 == 1111 is `SWI`; otherwise this is coprocessor space
    // (CDP/MRC/MCR), which we do not model.
    if (raw & 0x0F00_0000) == 0x0F00_0000 {
        decode_software_interrupt(raw)
    } else {
        ArmOperation::Undefined { raw }
    }
}

fn decode_software_interrupt(raw: u32) -> ArmOperation {
    ArmOperation::Undefined { raw }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::condition::Condition;

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
    fn coarse_classes_route_to_undefined_for_now() {
        // Until the per-class decoders are implemented, every well-formed
        // instruction still decodes without panicking and preserves its word.
        for raw in [
            0xE281_1001u32, // ADD (data processing, imm)
            0xE590_1000,    // LDR (single transfer)
            0xE8BD_00FF,    // LDM (block transfer)
            0xEA00_0000,    // B (branch)
            0xEF12_3456,    // SWI
        ] {
            assert_eq!(decode_arm(raw).operation, ArmOperation::Undefined { raw });
        }
    }
}
