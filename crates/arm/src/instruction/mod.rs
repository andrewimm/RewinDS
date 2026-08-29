//! Decoded-instruction representation, shared by the ARM and Thumb decoders.

pub mod arm;
pub mod thumb;

pub use arm::*;
pub use thumb::ThumbInstruction;

/// A decoded instruction from either instruction set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instruction {
    Arm(ArmInstruction),
    Thumb(ThumbInstruction),
}
