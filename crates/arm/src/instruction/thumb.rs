//! Structural representation of a decoded 16-bit Thumb instruction.
//!
//! Thumb decoding is not yet implemented; this placeholder exists so the
//! top-level `Instruction` type is complete. Every 16-bit word currently
//! decodes to `Undefined`, preserving the raw halfword.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbInstruction {
    /// A halfword the Thumb decoder does not yet handle. The raw value is kept.
    Undefined { raw: u16 },
}
