//! ARM condition codes.
//!
//! Every 32-bit ARM instruction carries a 4-bit condition in bits 31..28 that
//! gates whether it executes based on the CPSR flags. The decoder only records
//! which condition was encoded; evaluating it against live flags is the
//! interpreter's job.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Condition {
    /// `EQ` — equal (Z set).
    Eq,
    /// `NE` — not equal (Z clear).
    Ne,
    /// `CS`/`HS` — carry set / unsigned higher or same (C set).
    Cs,
    /// `CC`/`LO` — carry clear / unsigned lower (C clear).
    Cc,
    /// `MI` — negative (N set).
    Mi,
    /// `PL` — positive or zero (N clear).
    Pl,
    /// `VS` — overflow (V set).
    Vs,
    /// `VC` — no overflow (V clear).
    Vc,
    /// `HI` — unsigned higher (C set and Z clear).
    Hi,
    /// `LS` — unsigned lower or same (C clear or Z set).
    Ls,
    /// `GE` — signed greater or equal (N == V).
    Ge,
    /// `LT` — signed less than (N != V).
    Lt,
    /// `GT` — signed greater than (Z clear and N == V).
    Gt,
    /// `LE` — signed less or equal (Z set or N != V).
    Le,
    /// `AL` — always.
    Al,
    /// `NV` — never / reserved. Unpredictable on ARMv4T; on later cores this
    /// slot is repurposed for unconditional instructions. We preserve it so the
    /// decoder never loses information.
    Nv,
}

impl Condition {
    /// Decode a condition from the low four bits of `bits`. Callers typically
    /// pass `raw >> 28`; any higher bits are ignored.
    #[inline]
    pub const fn decode(bits: u32) -> Condition {
        match bits & 0xF {
            0x0 => Condition::Eq,
            0x1 => Condition::Ne,
            0x2 => Condition::Cs,
            0x3 => Condition::Cc,
            0x4 => Condition::Mi,
            0x5 => Condition::Pl,
            0x6 => Condition::Vs,
            0x7 => Condition::Vc,
            0x8 => Condition::Hi,
            0x9 => Condition::Ls,
            0xA => Condition::Ge,
            0xB => Condition::Lt,
            0xC => Condition::Gt,
            0xD => Condition::Le,
            0xE => Condition::Al,
            0xF => Condition::Nv,
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Condition;

    #[test]
    fn decodes_every_nibble() {
        let expected = [
            Condition::Eq,
            Condition::Ne,
            Condition::Cs,
            Condition::Cc,
            Condition::Mi,
            Condition::Pl,
            Condition::Vs,
            Condition::Vc,
            Condition::Hi,
            Condition::Ls,
            Condition::Ge,
            Condition::Lt,
            Condition::Gt,
            Condition::Le,
            Condition::Al,
            Condition::Nv,
        ];
        for (bits, cond) in expected.iter().enumerate() {
            assert_eq!(Condition::decode(bits as u32), *cond);
        }
    }

    #[test]
    fn ignores_upper_bits() {
        // `raw >> 28` is only ever 0..=15, but decode should be robust to junk.
        assert_eq!(Condition::decode(0xE), Condition::Al);
        assert_eq!(Condition::decode(0xDEAD_BEE0 | 0xE), Condition::Al);
    }
}
