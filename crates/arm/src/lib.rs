//! Decoding and interpreting ARM and Thumb instructions.
//!
//! This crate is deliberately ignorant of any particular machine: it knows the
//! ARM7TDMI instruction sets and nothing about the GBA, the DS, memory maps, or
//! scheduling. It turns raw instruction words into a lossless structured form
//! ([`Instruction`]) that higher layers interpret.

pub mod condition;
pub mod decode;
pub mod instruction;
pub mod register;

pub use condition::Condition;
pub use decode::{decode_arm, decode_thumb};
pub use instruction::{ArmInstruction, ArmOperation, Instruction, ThumbInstruction};
pub use register::Register;
