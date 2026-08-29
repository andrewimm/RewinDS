//! Turning raw instruction words into the structured [`crate::instruction`]
//! representation.

pub mod arm;
pub mod operand;
pub mod thumb;

pub use arm::decode_arm;
pub use thumb::decode_thumb;
