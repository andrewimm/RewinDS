//! Thumb (16-bit) instruction decoder.
//!
//! Thumb classification is a prefix decode tree on the high bits: bits 15..13
//! select a coarse group, and lower bits refine it. Several formats share a top
//! prefix and are split further down (format 1 vs 2 on bits 12..11; the
//! register-offset load/stores on bit 9; the `1101` group into conditional
//! branch, `SWI`, and undefined on bits 11..8). The tree is total — every one of
//! the 65536 halfwords resolves to some variant, `Undefined` included.
//!
//! Scope is ARMv4T (the GBA's ARM7TDMI): the `BLX` encoding space and the
//! reserved `cond == 1110` conditional branch decode to `Undefined`.
//!
//! The dispatch is complete; the per-format bodies that build each variant's
//! fields are not yet implemented and currently preserve the raw halfword.

use crate::instruction::thumb::{
    AddSubOperand, ThumbAluOp, ThumbHiRegOp, ThumbImmediateOp, ThumbInstruction, ThumbShiftOp,
};
use crate::register::Register;

/// Decode a 16-bit Thumb instruction halfword.
pub fn decode_thumb(raw: u16) -> ThumbInstruction {
    match raw >> 13 {
        0b000 => {
            // Format 1 (move shifted) unless the shift-op field is 0b11, which
            // is instead the format-2 add/subtract encoding.
            if (raw >> 11) & 0b11 == 0b11 {
                decode_add_subtract(raw)
            } else {
                decode_move_shifted(raw)
            }
        }
        0b001 => decode_alu_immediate(raw),
        0b010 => {
            if raw & (1 << 12) != 0 {
                // 0101 — register-offset load/store; bit 9 splits plain from
                // sign-extended.
                if raw & (1 << 9) != 0 {
                    decode_load_store_sign_extended(raw)
                } else {
                    decode_load_store_register(raw)
                }
            } else if raw & (1 << 11) != 0 {
                decode_pc_relative_load(raw) // 01001
            } else if raw & (1 << 10) != 0 {
                decode_hi_register(raw) // 010001
            } else {
                decode_alu_operation(raw) // 010000
            }
        }
        0b011 => decode_load_store_immediate(raw),
        0b100 => {
            if raw & (1 << 12) != 0 {
                decode_sp_relative_load_store(raw) // 1001
            } else {
                decode_load_store_halfword(raw) // 1000
            }
        }
        0b101 => {
            if raw & (1 << 12) != 0 {
                decode_misc_1011(raw) // 1011 — adjust SP / push-pop / undefined
            } else {
                decode_load_address(raw) // 1010
            }
        }
        0b110 => {
            if raw & (1 << 12) != 0 {
                // 1101 — conditional branch, unless the condition field names
                // SWI (1111) or the reserved-undefined slot (1110).
                match (raw >> 8) & 0xF {
                    0xF => decode_software_interrupt(raw),
                    0xE => ThumbInstruction::Undefined { raw },
                    _ => decode_conditional_branch(raw),
                }
            } else {
                decode_block_transfer(raw) // 1100
            }
        }
        0b111 => match (raw >> 11) & 0b11 {
            0b00 => decode_unconditional_branch(raw), // 11100
            0b10 | 0b11 => decode_long_branch_link(raw), // 11110 / 11111
            0b01 => ThumbInstruction::Undefined { raw }, // 11101 — BLX, ARMv5 only
            _ => unreachable!(),
        },
        _ => unreachable!(),
    }
}

/// Refine the `1011` group into adjust-SP (format 13), push/pop (format 14), or
/// undefined, on bits 11..9.
fn decode_misc_1011(raw: u16) -> ThumbInstruction {
    match (raw >> 9) & 0b111 {
        0b000 => decode_adjust_stack_pointer(raw),
        0b010 | 0b110 => decode_push_pop(raw),
        _ => ThumbInstruction::Undefined { raw },
    }
}

// ---------------------------------------------------------------------------
// Per-format decoders.
//
// The dispatch tree above is complete; these bodies are not yet implemented and
// preserve the raw halfword.
// ---------------------------------------------------------------------------

/// Format 1 — move shifted register. The op field is bits 12..11 (never `0b11`,
/// which the dispatch routes to add/subtract), the 5-bit amount is bits 10..6.
fn decode_move_shifted(raw: u16) -> ThumbInstruction {
    ThumbInstruction::MoveShifted {
        op: decode_thumb_shift_op((raw >> 11) & 0b11),
        amount: ((raw >> 6) & 0x1F) as u8,
        rs: low_register(raw >> 3),
        rd: low_register(raw),
    }
}

/// Format 2 — add/subtract. Bit 10 selects an immediate vs register operand,
/// bit 9 selects subtract, and the operand/offset is bits 8..6.
fn decode_add_subtract(raw: u16) -> ThumbInstruction {
    let value = ((raw >> 6) & 0b111) as u8;
    let operand = if raw & (1 << 10) != 0 {
        AddSubOperand::Immediate(value)
    } else {
        AddSubOperand::Register(Register::new(value))
    };
    ThumbInstruction::AddSubtract {
        subtract: raw & (1 << 9) != 0,
        operand,
        rs: low_register(raw >> 3),
        rd: low_register(raw),
    }
}

/// Format 3 — move/compare/add/subtract with an 8-bit immediate. The op is bits
/// 12..11, the destination is bits 10..8.
fn decode_alu_immediate(raw: u16) -> ThumbInstruction {
    let op = match (raw >> 11) & 0b11 {
        0b00 => ThumbImmediateOp::Mov,
        0b01 => ThumbImmediateOp::Cmp,
        0b10 => ThumbImmediateOp::Add,
        0b11 => ThumbImmediateOp::Sub,
        _ => unreachable!(),
    };
    ThumbInstruction::AluImmediate {
        op,
        rd: low_register(raw >> 8),
        immediate: raw as u8,
    }
}

/// Format 4 — ALU operation between two low registers. The op is bits 9..6.
fn decode_alu_operation(raw: u16) -> ThumbInstruction {
    ThumbInstruction::AluOperation {
        op: decode_thumb_alu_op((raw >> 6) & 0xF),
        rs: low_register(raw >> 3),
        rd: low_register(raw),
    }
}

/// Format 5 — high-register operations and `BX`. The `H1`/`H2` bits (7 and 6)
/// extend `rd`/`rs` to the full 4-bit register range.
fn decode_hi_register(raw: u16) -> ThumbInstruction {
    let op = match (raw >> 8) & 0b11 {
        0b00 => ThumbHiRegOp::Add,
        0b01 => ThumbHiRegOp::Cmp,
        0b10 => ThumbHiRegOp::Mov,
        0b11 => ThumbHiRegOp::Bx,
        _ => unreachable!(),
    };
    let rd_high = (raw >> 7) & 1;
    let rs_high = (raw >> 6) & 1;
    ThumbInstruction::HiRegister {
        op,
        rs: Register::new((((raw >> 3) & 0b111) | (rs_high << 3)) as u8),
        rd: Register::new(((raw & 0b111) | (rd_high << 3)) as u8),
    }
}

/// Build a low register (r0..r7) from the low three bits of `bits`.
fn low_register(bits: u16) -> Register {
    Register::new((bits & 0b111) as u8)
}

/// Decode a format-1 shift op (bits 12..11). `0b11` is add/subtract and never
/// reaches here.
fn decode_thumb_shift_op(bits: u16) -> ThumbShiftOp {
    match bits & 0b11 {
        0b00 => ThumbShiftOp::Lsl,
        0b01 => ThumbShiftOp::Lsr,
        0b10 => ThumbShiftOp::Asr,
        _ => unreachable!(),
    }
}

/// Decode a format-4 ALU op (bits 9..6).
fn decode_thumb_alu_op(bits: u16) -> ThumbAluOp {
    use ThumbAluOp::*;
    match bits & 0xF {
        0x0 => And,
        0x1 => Eor,
        0x2 => Lsl,
        0x3 => Lsr,
        0x4 => Asr,
        0x5 => Adc,
        0x6 => Sbc,
        0x7 => Ror,
        0x8 => Tst,
        0x9 => Neg,
        0xA => Cmp,
        0xB => Cmn,
        0xC => Orr,
        0xD => Mul,
        0xE => Bic,
        0xF => Mvn,
        _ => unreachable!(),
    }
}

fn decode_pc_relative_load(raw: u16) -> ThumbInstruction {
    ThumbInstruction::Undefined { raw }
}

fn decode_load_store_register(raw: u16) -> ThumbInstruction {
    ThumbInstruction::Undefined { raw }
}

fn decode_load_store_sign_extended(raw: u16) -> ThumbInstruction {
    ThumbInstruction::Undefined { raw }
}

fn decode_load_store_immediate(raw: u16) -> ThumbInstruction {
    ThumbInstruction::Undefined { raw }
}

fn decode_load_store_halfword(raw: u16) -> ThumbInstruction {
    ThumbInstruction::Undefined { raw }
}

fn decode_sp_relative_load_store(raw: u16) -> ThumbInstruction {
    ThumbInstruction::Undefined { raw }
}

fn decode_load_address(raw: u16) -> ThumbInstruction {
    ThumbInstruction::Undefined { raw }
}

fn decode_adjust_stack_pointer(raw: u16) -> ThumbInstruction {
    ThumbInstruction::Undefined { raw }
}

fn decode_push_pop(raw: u16) -> ThumbInstruction {
    ThumbInstruction::Undefined { raw }
}

fn decode_block_transfer(raw: u16) -> ThumbInstruction {
    ThumbInstruction::Undefined { raw }
}

fn decode_conditional_branch(raw: u16) -> ThumbInstruction {
    ThumbInstruction::Undefined { raw }
}

fn decode_software_interrupt(raw: u16) -> ThumbInstruction {
    ThumbInstruction::Undefined { raw }
}

fn decode_unconditional_branch(raw: u16) -> ThumbInstruction {
    ThumbInstruction::Undefined { raw }
}

fn decode_long_branch_link(raw: u16) -> ThumbInstruction {
    ThumbInstruction::Undefined { raw }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_is_total_and_never_panics() {
        // Every 16-bit halfword must resolve to some variant without panicking.
        for raw in 0..=u16::MAX {
            let _ = decode_thumb(raw);
        }
    }

    #[test]
    fn undefined_spaces_decode_to_undefined() {
        // BLX space, bits 15..11 = 11101, is ARMv5-only.
        let blx = 0b1110_1000_0000_0000;
        assert_eq!(
            decode_thumb(blx),
            ThumbInstruction::Undefined { raw: blx }
        );
        // Conditional-branch slot with the reserved cond == 1110.
        let reserved_cond = 0b1101_1110_0000_0000;
        assert_eq!(
            decode_thumb(reserved_cond),
            ThumbInstruction::Undefined {
                raw: reserved_cond
            }
        );
    }

    #[test]
    fn move_shifted_decodes() {
        // LSL r0, r1, #5
        assert_eq!(
            decode_thumb(0x0148),
            ThumbInstruction::MoveShifted {
                op: ThumbShiftOp::Lsl,
                amount: 5,
                rs: Register::new(1),
                rd: Register::new(0),
            }
        );
        // ASR r2, r3, #31
        assert_eq!(
            decode_thumb(0x17DA),
            ThumbInstruction::MoveShifted {
                op: ThumbShiftOp::Asr,
                amount: 31,
                rs: Register::new(3),
                rd: Register::new(2),
            }
        );
    }

    #[test]
    fn add_subtract_decodes() {
        // ADD r0, r1, r2
        assert_eq!(
            decode_thumb(0x1888),
            ThumbInstruction::AddSubtract {
                subtract: false,
                operand: AddSubOperand::Register(Register::new(2)),
                rs: Register::new(1),
                rd: Register::new(0),
            }
        );
        // SUB r0, r1, #3
        assert_eq!(
            decode_thumb(0x1EC8),
            ThumbInstruction::AddSubtract {
                subtract: true,
                operand: AddSubOperand::Immediate(3),
                rs: Register::new(1),
                rd: Register::new(0),
            }
        );
    }

    #[test]
    fn alu_immediate_decodes() {
        // MOV r5, #0x2A
        assert_eq!(
            decode_thumb(0x252A),
            ThumbInstruction::AluImmediate {
                op: ThumbImmediateOp::Mov,
                rd: Register::new(5),
                immediate: 0x2A,
            }
        );
        // CMP r7, #0xFF
        assert_eq!(
            decode_thumb(0x2FFF),
            ThumbInstruction::AluImmediate {
                op: ThumbImmediateOp::Cmp,
                rd: Register::new(7),
                immediate: 0xFF,
            }
        );
    }

    #[test]
    fn alu_operation_decodes() {
        // AND r0, r1
        assert_eq!(
            decode_thumb(0x4008),
            ThumbInstruction::AluOperation {
                op: ThumbAluOp::And,
                rs: Register::new(1),
                rd: Register::new(0),
            }
        );
        // MUL r2, r3
        assert_eq!(
            decode_thumb(0x435A),
            ThumbInstruction::AluOperation {
                op: ThumbAluOp::Mul,
                rs: Register::new(3),
                rd: Register::new(2),
            }
        );
        // MVN r0, r7
        assert_eq!(
            decode_thumb(0x43F8),
            ThumbInstruction::AluOperation {
                op: ThumbAluOp::Mvn,
                rs: Register::new(7),
                rd: Register::new(0),
            }
        );
    }

    #[test]
    fn hi_register_decodes() {
        // ADD r8, r1 — H1 extends rd to r8.
        assert_eq!(
            decode_thumb(0x4488),
            ThumbInstruction::HiRegister {
                op: ThumbHiRegOp::Add,
                rs: Register::new(1),
                rd: Register::new(8),
            }
        );
        // MOV r1, r8 — H2 extends rs to r8.
        assert_eq!(
            decode_thumb(0x4641),
            ThumbInstruction::HiRegister {
                op: ThumbHiRegOp::Mov,
                rs: Register::new(8),
                rd: Register::new(1),
            }
        );
        // BX lr — the canonical 0x4770; target is r14 via H2.
        assert_eq!(
            decode_thumb(0x4770),
            ThumbInstruction::HiRegister {
                op: ThumbHiRegOp::Bx,
                rs: Register::LR,
                rd: Register::new(0),
            }
        );
    }
}
