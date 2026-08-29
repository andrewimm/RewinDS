//! Shared helpers for decoding operand fields (shifts, immediates) reused
//! across instruction classes.

use crate::instruction::arm::{Operand2, Shift, ShiftKind, ShiftSource};
use crate::register::Register;

/// Decode the 12-bit second operand of a data-processing instruction.
///
/// `immediate` is the `I` bit (bit 25): when set, the field is a rotated 8-bit
/// immediate; otherwise it is a register with a barrel-shift. Both forms are
/// kept in their encoded shape — the rotate and shift amounts are not applied.
pub(crate) fn decode_operand2(raw: u32, immediate: bool) -> Operand2 {
    if immediate {
        Operand2::Immediate {
            value: raw as u8,
            rotate: (raw >> 8) as u8 & 0xF,
        }
    } else {
        Operand2::Register {
            rm: Register::new(raw as u8),
            shift: decode_shift(raw),
        }
    }
}

/// Decode the 8-bit shift field (bits 11..4) applied to a register operand.
///
/// Bit 4 selects the source of the shift amount: an immediate in bits 11..7, or
/// a register (`Rs`) named in bits 11..8. The shift kind is bits 6..5.
pub(crate) fn decode_shift(raw: u32) -> Shift {
    let kind = decode_shift_kind(raw >> 5);
    let source = if raw & (1 << 4) != 0 {
        ShiftSource::Register(Register::new((raw >> 8) as u8))
    } else {
        ShiftSource::Immediate((raw >> 7) as u8 & 0x1F)
    };
    Shift { kind, source }
}

/// Decode a two-bit shift-type field (bits 6..5 of a shift operand).
fn decode_shift_kind(bits: u32) -> ShiftKind {
    match bits & 0b11 {
        0b00 => ShiftKind::Lsl,
        0b01 => ShiftKind::Lsr,
        0b10 => ShiftKind::Asr,
        0b11 => ShiftKind::Ror,
        _ => unreachable!(),
    }
}
