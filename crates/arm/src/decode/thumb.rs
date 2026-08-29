//! Thumb (16-bit) instruction decoder.
//!
//! Not yet implemented. Every halfword decodes to `Undefined`, preserving the
//! raw value.

use crate::instruction::thumb::ThumbInstruction;

/// Decode a 16-bit Thumb instruction halfword.
pub fn decode_thumb(raw: u16) -> ThumbInstruction {
    ThumbInstruction::Undefined { raw }
}
