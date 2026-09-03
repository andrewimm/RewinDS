//! The ARM9 hardware division and square-root units (`0x4000280`/`0x40002B0`).
//!
//! The DS gives the ARM9 dedicated math units that games lean on heavily (fixed-point
//! scaling, touchscreen calibration, 3D geometry). Software writes the operands and
//! reads the result registers. We compute the result instantly, but we *do* model the
//! **busy period** (the `CNT` bit-15 flag): a starting operation is reported busy for
//! the hardware's cycle count, so a game that polls the flag before reading the result
//! spins the same number of times it would on hardware. That poll-loop time is not
//! cosmetic — it advances the ARM9's clock (and thus VCOUNT) relative to the ARM7, and
//! reporting not-busy made VCOUNT-sensitive code diverge. Cycle counts (in master ticks
//! = ARM9 cycles) follow GBATEK "DS Maths": division 36 (32/32) or 68 (64-bit operand),
//! square root 26. Both units live behind one I/O window (`0x280..0x2C0`).

use emu_core::Timestamp;

/// Division busy period in master ticks for `DIVCNT` mode bits 0-1.
const DIV_CYCLES_32: Timestamp = 36; // 32/32
const DIV_CYCLES_64: Timestamp = 68; // 64/32 and 64/64
/// Square-root busy period in master ticks.
const SQRT_CYCLES: Timestamp = 26;

/// The division (`0x280`) and square-root (`0x2B0`) units.
#[derive(Default)]
pub struct Math {
    /// `DIVCNT`: bits 0-1 = mode (0 = 32/32, 1 = 64/32, 2 = 64/64), bit 14 = div-by-
    /// zero flag, bit 15 = busy (set until [`Self::div_done_at`]).
    divcnt: u16,
    numer: u64,
    denom: u64,
    div_result: u64,
    divrem_result: u64,
    /// Master-clock time the current division finishes; `DIVCNT` reads busy until then.
    div_done_at: Timestamp,
    /// `SQRTCNT`: bit 0 = mode (0 = 32-bit input, 1 = 64-bit), bit 15 = busy.
    sqrtcnt: u16,
    sqrt_param: u64,
    sqrt_result: u32,
    /// Master-clock time the current square root finishes.
    sqrt_done_at: Timestamp,
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

    /// Read a math register (`offset` is the low 12 bits of the address). `now` is the
    /// ARM9's current master-clock time, used to report the busy flag on `DIVCNT`/`SQRTCNT`.
    pub fn read(&self, offset: u32, bytes: u32, now: Timestamp) -> u32 {
        let word = |reg: u64, base: u32| (reg >> ((offset - base) * 8)) as u32;
        let busy = |done_at: Timestamp| if now < done_at { 1 << 15 } else { 0 };
        let value = match offset {
            0x280..=0x283 => (self.divcnt | busy(self.div_done_at)) as u32,
            0x290..=0x297 => word(self.numer, 0x290),
            0x298..=0x29F => word(self.denom, 0x298),
            0x2A0..=0x2A7 => word(self.div_result, 0x2A0),
            0x2A8..=0x2AF => word(self.divrem_result, 0x2A8),
            0x2B0..=0x2B3 => (self.sqrtcnt | busy(self.sqrt_done_at)) as u32,
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

    /// Write a math register, recomputing the affected unit's result. `now` is the ARM9's
    /// current master-clock time; writing any operand (or the mode) restarts the unit, so
    /// its busy flag is set for the operation's cycle count from this moment — exactly as
    /// the hardware does, so a following poll loop spins the right number of times.
    pub fn write(&mut self, offset: u32, value: u32, bytes: u32, now: Timestamp) {
        match offset {
            0x280..=0x283 => {
                self.divcnt = (self.divcnt & !0x3) | (value as u16 & 0x3);
                self.compute_div();
                self.div_done_at = now + self.div_cycles();
            }
            0x290..=0x297 => {
                splice(&mut self.numer, offset - 0x290, value, bytes);
                self.compute_div();
                self.div_done_at = now + self.div_cycles();
            }
            0x298..=0x29F => {
                splice(&mut self.denom, offset - 0x298, value, bytes);
                self.compute_div();
                self.div_done_at = now + self.div_cycles();
            }
            0x2B0..=0x2B3 => {
                self.sqrtcnt = (self.sqrtcnt & !0x1) | (value as u16 & 0x1);
                self.compute_sqrt();
                self.sqrt_done_at = now + SQRT_CYCLES;
            }
            0x2B8..=0x2BF => {
                splice(&mut self.sqrt_param, offset - 0x2B8, value, bytes);
                self.compute_sqrt();
                self.sqrt_done_at = now + SQRT_CYCLES;
            }
            _ => {}
        }
    }

    /// The current division's busy period: longer when a 64-bit operand is involved.
    fn div_cycles(&self) -> Timestamp {
        if self.divcnt & 3 == 0 {
            DIV_CYCLES_32
        } else {
            DIV_CYCLES_64
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
        m.write(off, v, 4, 0);
    }
    fn read32(m: &Math, off: u32) -> u32 {
        m.read(off, 4, u64::MAX) // read long after any busy period
    }

    #[test]
    fn signed_32_by_32_division() {
        let mut m = Math::new();
        write32(&mut m, 0x280, 0); // mode 0: 32/32
        write32(&mut m, 0x290, (-1000i32) as u32); // NUMER = -1000
        write32(&mut m, 0x298, 7); // DENOM = 7
        assert_eq!(read32(&m, 0x2A0) as i32, -142); // -1000 / 7
        assert_eq!(read32(&m, 0x2A8) as i32, -6); // -1000 % 7
        assert_eq!(m.divcnt & (1 << 14), 0); // no div-by-zero
    }

    #[test]
    fn reciprocal_matches_the_touchscreen_use() {
        // The touchscreen math computes 0x1000_0000 / range via the unit.
        let mut m = Math::new();
        write32(&mut m, 0x280, 0);
        write32(&mut m, 0x290, 0x1000_0000);
        write32(&mut m, 0x298, 0xC00); // adc range
        assert_eq!(read32(&m, 0x2A0), 0x1000_0000 / 0xC00);
    }

    #[test]
    fn division_by_zero_sets_the_flag() {
        let mut m = Math::new();
        write32(&mut m, 0x280, 0);
        write32(&mut m, 0x290, 5);
        write32(&mut m, 0x298, 0);
        assert_ne!(m.divcnt & (1 << 14), 0);
        assert_eq!(read32(&m, 0x2A8), 5); // remainder = numerator
    }

    #[test]
    fn square_root() {
        let mut m = Math::new();
        write32(&mut m, 0x2B0, 0); // 32-bit mode
        write32(&mut m, 0x2B8, 0x1_0000); // param
        assert_eq!(read32(&m, 0x2B4), 0x100); // sqrt(65536)
        write32(&mut m, 0x2B8, 12345);
        assert_eq!(read32(&m, 0x2B4), 111); // floor(sqrt(12345))
    }

    #[test]
    fn division_reports_busy_until_its_cycle_count_elapses() {
        let mut m = Math::new();
        // Start a 32/32 division at t=100: busy for 36 ticks, ready at t=136.
        m.write(0x280, 0, 4, 100); // mode 0
        m.write(0x290, 1000, 4, 100); // NUMER (restarts the unit at t=100)
        m.write(0x298, 3, 4, 100); // DENOM (restarts the unit at t=100)
        assert_ne!(m.read(0x280, 4, 100) & (1 << 15), 0); // busy at t=100
        assert_ne!(m.read(0x280, 4, 135) & (1 << 15), 0); // still busy at t=135
        assert_eq!(m.read(0x280, 4, 136) & (1 << 15), 0); // ready at t=136
        // A 64-bit operand takes longer (68 ticks): started at t=136, ready at t=204.
        m.write(0x280, 2, 4, 136); // mode 2 (64/64)
        assert_ne!(m.read(0x280, 4, 203) & (1 << 15), 0);
        assert_eq!(m.read(0x280, 4, 204) & (1 << 15), 0);
    }

    #[test]
    fn square_root_reports_busy_for_26_ticks() {
        let mut m = Math::new();
        m.write(0x2B8, 0x1_0000, 4, 50); // start sqrt at t=50, ready at t=76
        assert_ne!(m.read(0x2B0, 4, 75) & (1 << 15), 0);
        assert_eq!(m.read(0x2B0, 4, 76) & (1 << 15), 0);
    }
}
