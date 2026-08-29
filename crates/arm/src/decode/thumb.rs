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

use crate::instruction::thumb::ThumbInstruction;

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

fn decode_move_shifted(raw: u16) -> ThumbInstruction {
    ThumbInstruction::Undefined { raw }
}

fn decode_add_subtract(raw: u16) -> ThumbInstruction {
    ThumbInstruction::Undefined { raw }
}

fn decode_alu_immediate(raw: u16) -> ThumbInstruction {
    ThumbInstruction::Undefined { raw }
}

fn decode_alu_operation(raw: u16) -> ThumbInstruction {
    ThumbInstruction::Undefined { raw }
}

fn decode_hi_register(raw: u16) -> ThumbInstruction {
    ThumbInstruction::Undefined { raw }
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
}
