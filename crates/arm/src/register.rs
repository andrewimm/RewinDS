//! CPU register references.
//!
//! A `Register` is just a 4-bit index (r0..r15) as it appears in an instruction
//! field. We keep it as a distinct newtype rather than a bare `u8` so decoded
//! instructions are self-documenting and so we can hang the architectural
//! aliases (SP/LR/PC) off it. No banking or mode knowledge lives here — that is
//! a runtime concern, not a decode concern.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Register(pub u8);

impl Register {
    /// r13, the stack pointer by convention.
    pub const SP: Register = Register(13);
    /// r14, the link register.
    pub const LR: Register = Register(14);
    /// r15, the program counter.
    pub const PC: Register = Register(15);

    /// Build a register from a raw 4-bit field. Any upper bits are masked off,
    /// since register fields are always exactly four bits wide.
    #[inline]
    pub const fn new(index: u8) -> Register {
        Register(index & 0xF)
    }

    /// The register index as a `usize`, convenient for indexing a register file.
    #[inline]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// Whether this is r15 (the program counter). PC as an operand or
    /// destination has special semantics in almost every instruction class.
    #[inline]
    pub const fn is_pc(self) -> bool {
        self.0 == 15
    }
}
