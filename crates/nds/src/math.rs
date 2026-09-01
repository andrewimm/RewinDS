//! The ARM9 hardware division and square-root units (`0x4000280`/`0x40002B0`).
//!
//! The DS gives the ARM9 dedicated math units that games lean on heavily (fixed-point
//! scaling, touchscreen calibration, 3D geometry). Software writes the operands and
//! reads the result registers; the hardware would take a handful of cycles (a busy
//! flag), but we compute instantly and report not-busy. Both units live behind one
//! I/O window (`0x280..0x2C0`), handled by [`Math::read`]/[`Math::write`].

/// The division (`0x280`) and square-root (`0x2B0`) units.
#[derive(Default)]
pub struct Math {
    /// `DIVCNT`: bits 0-1 = mode (0 = 32/32, 1 = 64/32, 2 = 64/64), bit 14 = div-by-
    /// zero flag, bit 15 = busy (always 0 here).
    divcnt: u16,
    numer: u64,
    denom: u64,
    div_result: u64,
    divrem_result: u64,
    /// `SQRTCNT`: bit 0 = mode (0 = 32-bit input, 1 = 64-bit), bit 15 = busy.
    sqrtcnt: u16,
    sqrt_param: u64,
    sqrt_result: u32,
}

/// Splice a `bytes`-wide `value` into a 64-bit register at `byte_off`.
fn splice(reg: &mut u64, byte_off: u32, value: u32, bytes: u32) {
    let shift = byte_off * 8;
    let width: u64 = match bytes {
        1 => 0xFF,
        2 => 0xFFFF,
        _ => 0xFFFF_FFFF,
    };
    let mask = width << shift;
    *reg = (*reg & !mask) | ((value as u64) << shift & mask);
}

impl Math {
    pub fn new() -> Self {
        Math::default()
    }

    /// Read a math register (`offset` is the low 12 bits of the address).
    pub fn read(&self, offset: u32, bytes: u32) -> u32 {
        let word = |reg: u64, base: u32| (reg >> ((offset - base) * 8)) as u32;
        let value = match offset {
            0x280..=0x283 => self.divcnt as u32, // busy (bit 15) already clear
            0x290..=0x297 => word(self.numer, 0x290),
            0x298..=0x29F => word(self.denom, 0x298),
            0x2A0..=0x2A7 => word(self.div_result, 0x2A0),
            0x2A8..=0x2AF => word(self.divrem_result, 0x2A8),
            0x2B0..=0x2B3 => self.sqrtcnt as u32,
            0x2B4..=0x2B7 => self.sqrt_result,
            0x2B8..=0x2BF => word(self.sqrt_param, 0x2B8),
            _ => 0,
        };
        // Narrow reads take the low bytes of the register word.
        match bytes {
            1 => value & 0xFF,
            2 => value & 0xFFFF,
            _ => value,
        }
    }

    /// Write a math register, recomputing the affected unit's result.
    pub fn write(&mut self, offset: u32, value: u32, bytes: u32) {
        match offset {
            0x280..=0x283 => {
                self.divcnt = (self.divcnt & !0x3) | (value as u16 & 0x3);
                self.compute_div();
            }
            0x290..=0x297 => {
                splice(&mut self.numer, offset - 0x290, value, bytes);
                self.compute_div();
            }
            0x298..=0x29F => {
                splice(&mut self.denom, offset - 0x298, value, bytes);
                self.compute_div();
            }
            0x2B0..=0x2B3 => {
                self.sqrtcnt = (self.sqrtcnt & !0x1) | (value as u16 & 0x1);
                self.compute_sqrt();
            }
            0x2B8..=0x2BF => {
                splice(&mut self.sqrt_param, offset - 0x2B8, value, bytes);
                self.compute_sqrt();
            }
            _ => {}
        }
    }

    /// Divide `NUMER / DENOM` per the mode, updating the result and remainder. On a
    /// zero divisor the busy-less hardware sets the div-by-zero flag and returns
    /// `±1` / the numerator (GBATEK).
    fn compute_div(&mut self) {
        self.divcnt &= !(1 << 14);
        let (numer, denom): (i64, i64) = match self.divcnt & 3 {
            0 => (self.numer as i32 as i64, self.denom as i32 as i64),
            1 => (self.numer as i64, self.denom as i32 as i64),
            _ => (self.numer as i64, self.denom as i64),
        };
        if denom == 0 {
            self.divcnt |= 1 << 14;
            self.divrem_result = numer as u64;
            let quotient = if numer < 0 { 1i64 } else { -1i64 };
            // In the 32-bit-numerator modes the upper half of the result is inverted.
            self.div_result = if self.divcnt & 3 == 2 {
                quotient as u64
            } else {
                (quotient as u64) ^ 0xFFFF_FFFF_0000_0000
            };
        } else {
            self.div_result = numer.wrapping_div(denom) as u64;
            self.divrem_result = numer.wrapping_rem(denom) as u64;
        }
    }

    /// Square-root of `SQRT_PARAM` (unsigned; 32- or 64-bit input per the mode).
    fn compute_sqrt(&mut self) {
        let param = if self.sqrtcnt & 1 == 0 {
            self.sqrt_param & 0xFFFF_FFFF
        } else {
            self.sqrt_param
        };
        self.sqrt_result = isqrt(param);
    }
}

/// Integer square root (floor) of a 64-bit value, computed bit by bit.
fn isqrt(value: u64) -> u32 {
    let mut result: u64 = 0;
    let mut bit: u64 = 1 << 62;
    let mut rem = value;
    while bit > rem {
        bit >>= 2;
    }
    while bit != 0 {
        if rem >= result + bit {
            rem -= result + bit;
            result = (result >> 1) + bit;
        } else {
            result >>= 1;
        }
        bit >>= 2;
    }
    result as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write32(m: &mut Math, off: u32, v: u32) {
        m.write(off, v, 4);
    }

    #[test]
    fn signed_32_by_32_division() {
        let mut m = Math::new();
        write32(&mut m, 0x280, 0); // mode 0: 32/32
        write32(&mut m, 0x290, (-1000i32) as u32); // NUMER = -1000
        write32(&mut m, 0x298, 7); // DENOM = 7
        assert_eq!(m.read(0x2A0, 4) as i32, -142); // -1000 / 7
        assert_eq!(m.read(0x2A8, 4) as i32, -6); // -1000 % 7
        assert_eq!(m.divcnt & (1 << 14), 0); // no div-by-zero
    }

    #[test]
    fn reciprocal_matches_the_touchscreen_use() {
        // The touchscreen math computes 0x1000_0000 / range via the unit.
        let mut m = Math::new();
        write32(&mut m, 0x280, 0);
        write32(&mut m, 0x290, 0x1000_0000);
        write32(&mut m, 0x298, 0xC00); // adc range
        assert_eq!(m.read(0x2A0, 4), 0x1000_0000 / 0xC00);
    }

    #[test]
    fn division_by_zero_sets_the_flag() {
        let mut m = Math::new();
        write32(&mut m, 0x280, 0);
        write32(&mut m, 0x290, 5);
        write32(&mut m, 0x298, 0);
        assert_ne!(m.divcnt & (1 << 14), 0);
        assert_eq!(m.read(0x2A8, 4), 5); // remainder = numerator
    }

    #[test]
    fn square_root() {
        let mut m = Math::new();
        write32(&mut m, 0x2B0, 0); // 32-bit mode
        write32(&mut m, 0x2B8, 0x1_0000); // param
        assert_eq!(m.read(0x2B4, 4), 0x100); // sqrt(65536)
        write32(&mut m, 0x2B8, 12345);
        assert_eq!(m.read(0x2B4, 4), 111); // floor(sqrt(12345))
    }
}
