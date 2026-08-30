//! The ARM7TDMI interpreter.
//!
//! This executes the decoded instructions from [`crate::decode`] against a
//! [`Bus`]. It models the visible programmer state — the register file, the
//! CPSR, and the fetch pipeline — and nothing about any particular machine.
//!
//! The pipeline is modeled by keeping `r15` at the address of the instruction
//! being executed and exposing it, when read as an operand, at the pipeline
//! offset (`+8` in ARM state, `+4` in Thumb). A write to `r15` redirects
//! execution; otherwise the fetch advances by one instruction.

mod bus;

pub use bus::{Bus, Timed};

use crate::condition::Condition;
use crate::decode::decode_arm;
use crate::instruction::arm::{
    ArmOperation, Branch, BranchExchange, DataProcessing, DataProcessingOpcode, Operand2, ShiftKind,
    ShiftSource,
};
use crate::register::Register;

/// A processor operating mode (the low five bits of the CPSR).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    User,
    Fiq,
    Irq,
    Supervisor,
    Abort,
    Undefined,
    System,
}

impl Mode {
    pub fn bits(self) -> u32 {
        match self {
            Mode::User => 0x10,
            Mode::Fiq => 0x11,
            Mode::Irq => 0x12,
            Mode::Supervisor => 0x13,
            Mode::Abort => 0x17,
            Mode::Undefined => 0x1B,
            Mode::System => 0x1F,
        }
    }

    pub fn from_bits(bits: u32) -> Option<Mode> {
        Some(match bits & 0x1F {
            0x10 => Mode::User,
            0x11 => Mode::Fiq,
            0x12 => Mode::Irq,
            0x13 => Mode::Supervisor,
            0x17 => Mode::Abort,
            0x1B => Mode::Undefined,
            0x1F => Mode::System,
            _ => return None,
        })
    }
}

/// A program status register (CPSR or an SPSR).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Psr(u32);

impl Psr {
    const N: u32 = 1 << 31;
    const Z: u32 = 1 << 30;
    const C: u32 = 1 << 29;
    const V: u32 = 1 << 28;
    const I: u32 = 1 << 7;
    const F: u32 = 1 << 6;
    const T: u32 = 1 << 5;

    pub fn from_bits(bits: u32) -> Psr {
        Psr(bits)
    }

    pub fn bits(self) -> u32 {
        self.0
    }

    fn flag(self, mask: u32) -> bool {
        self.0 & mask != 0
    }

    fn set_flag(&mut self, mask: u32, value: bool) {
        if value {
            self.0 |= mask;
        } else {
            self.0 &= !mask;
        }
    }

    pub fn n(self) -> bool {
        self.flag(Self::N)
    }
    pub fn z(self) -> bool {
        self.flag(Self::Z)
    }
    pub fn c(self) -> bool {
        self.flag(Self::C)
    }
    pub fn v(self) -> bool {
        self.flag(Self::V)
    }
    pub fn thumb(self) -> bool {
        self.flag(Self::T)
    }
    pub fn irq_disabled(self) -> bool {
        self.flag(Self::I)
    }
    pub fn fiq_disabled(self) -> bool {
        self.flag(Self::F)
    }

    pub fn set_n(&mut self, v: bool) {
        self.set_flag(Self::N, v);
    }
    pub fn set_z(&mut self, v: bool) {
        self.set_flag(Self::Z, v);
    }
    pub fn set_c(&mut self, v: bool) {
        self.set_flag(Self::C, v);
    }
    pub fn set_v(&mut self, v: bool) {
        self.set_flag(Self::V, v);
    }
    pub fn set_thumb(&mut self, v: bool) {
        self.set_flag(Self::T, v);
    }

    pub fn mode(self) -> Option<Mode> {
        Mode::from_bits(self.0)
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.0 = (self.0 & !0x1F) | mode.bits();
    }

    /// Whether a condition passes given the current flags.
    pub fn passes(self, condition: Condition) -> bool {
        use Condition::*;
        match condition {
            Eq => self.z(),
            Ne => !self.z(),
            Cs => self.c(),
            Cc => !self.c(),
            Mi => self.n(),
            Pl => !self.n(),
            Vs => self.v(),
            Vc => !self.v(),
            Hi => self.c() && !self.z(),
            Ls => !self.c() || self.z(),
            Ge => self.n() == self.v(),
            Lt => self.n() != self.v(),
            Gt => !self.z() && (self.n() == self.v()),
            Le => self.z() || (self.n() != self.v()),
            Al => true,
            Nv => false,
        }
    }
}

/// The ARM7TDMI processor state.
#[derive(Clone, Copy, Debug)]
pub struct Cpu {
    /// r0..r15; `r[15]` holds the address of the instruction being executed.
    r: [u32; 16],
    cpsr: Psr,
    /// Total guest cycles executed.
    cycles: u64,
    /// Whether the current instruction redirected the PC.
    branched: bool,
    /// Whether the next fetch is sequential with the previous access.
    sequential: bool,
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpu {
    pub fn new() -> Self {
        let mut cpsr = Psr::from_bits(0);
        cpsr.set_mode(Mode::System);
        Cpu {
            r: [0; 16],
            cpsr,
            cycles: 0,
            branched: false,
            sequential: false,
        }
    }

    pub fn cpsr(&self) -> Psr {
        self.cpsr
    }

    pub fn cycles(&self) -> u64 {
        self.cycles
    }

    /// Read the raw stored register (no pipeline adjustment). For `r15` this is
    /// the address of the instruction being executed.
    pub fn register(&self, index: usize) -> u32 {
        self.r[index]
    }

    /// Set the program counter and begin fetching from there.
    pub fn set_pc(&mut self, address: u32) {
        self.r[15] = address;
        self.sequential = false;
    }

    /// The pipeline offset applied to `r15` when it is read as an operand.
    fn pc_offset(&self) -> u32 {
        if self.cpsr.thumb() {
            4
        } else {
            8
        }
    }

    /// Read a register as an operand: `r15` reads at the pipeline offset.
    fn reg(&self, register: Register) -> u32 {
        if register.is_pc() {
            self.r[15].wrapping_add(self.pc_offset())
        } else {
            self.r[register.index()]
        }
    }

    /// Write a register. Writing `r15` redirects execution.
    fn set_reg(&mut self, register: Register, value: u32) {
        if register.is_pc() {
            self.r[15] = value;
            self.branched = true;
        } else {
            self.r[register.index()] = value;
        }
    }

    /// Execute one instruction.
    pub fn step<B: Bus>(&mut self, bus: &mut B) {
        self.branched = false;
        if self.cpsr.thumb() {
            self.step_thumb(bus);
        } else {
            self.step_arm(bus);
        }
    }

    fn step_arm<B: Bus>(&mut self, bus: &mut B) {
        let pc = self.r[15] & !3;
        self.r[15] = pc;
        let fetched = bus.fetch32(pc, self.sequential);
        self.cycles += fetched.cycles as u64;

        let instruction = decode_arm(fetched.value);
        if self.cpsr.passes(instruction.condition) {
            self.execute_arm(instruction.operation, bus);
        }

        if self.branched {
            self.sequential = false;
        } else {
            self.r[15] = pc.wrapping_add(4);
            self.sequential = true;
        }
    }

    fn step_thumb<B: Bus>(&mut self, _bus: &mut B) {
        // Thumb execution is not yet implemented.
        self.r[15] = (self.r[15] & !1).wrapping_add(2);
        self.sequential = true;
    }

    fn execute_arm<B: Bus>(&mut self, operation: ArmOperation, _bus: &mut B) {
        match operation {
            ArmOperation::DataProcessing(op) => self.execute_data_processing(op),
            ArmOperation::Branch(op) => self.execute_branch(op),
            ArmOperation::BranchExchange(op) => self.execute_branch_exchange(op),
            // Remaining classes are implemented in later phases.
            _ => {}
        }
    }

    fn execute_branch(&mut self, branch: Branch) {
        if branch.link {
            let return_address = self.r[15].wrapping_add(4);
            self.set_reg(Register::LR, return_address);
        }
        let target = self.reg(Register::PC).wrapping_add(branch.offset as u32);
        self.set_reg(Register::PC, target);
    }

    fn execute_branch_exchange(&mut self, bx: BranchExchange) {
        let target = self.reg(bx.rn);
        self.cpsr.set_thumb(target & 1 != 0);
        self.set_reg(Register::PC, target & !1);
    }

    fn execute_data_processing(&mut self, op: DataProcessing) {
        let (operand2, shifter_carry) = self.eval_operand2(&op.operand2);
        let rn = self.reg(op.rn);
        let carry_in = self.cpsr.c();

        use DataProcessingOpcode::*;
        // (result, carry, overflow, arithmetic?, writes destination?)
        let (result, carry, overflow, arithmetic, writes) = match op.opcode {
            And => (rn & operand2, shifter_carry, false, false, true),
            Eor => (rn ^ operand2, shifter_carry, false, false, true),
            Orr => (rn | operand2, shifter_carry, false, false, true),
            Bic => (rn & !operand2, shifter_carry, false, false, true),
            Mov => (operand2, shifter_carry, false, false, true),
            Mvn => (!operand2, shifter_carry, false, false, true),
            Tst => (rn & operand2, shifter_carry, false, false, false),
            Teq => (rn ^ operand2, shifter_carry, false, false, false),
            Sub => append(sub(rn, operand2), true),
            Rsb => append(sub(operand2, rn), true),
            Add => append(adc(rn, operand2, false), true),
            Adc => append(adc(rn, operand2, carry_in), true),
            Sbc => append(sbc(rn, operand2, carry_in), true),
            Rsc => append(sbc(operand2, rn, carry_in), true),
            Cmp => append(sub(rn, operand2), false),
            Cmn => append(adc(rn, operand2, false), false),
        };

        if writes {
            self.set_reg(op.rd, result);
        }

        if op.set_flags {
            self.cpsr.set_n(result & (1 << 31) != 0);
            self.cpsr.set_z(result == 0);
            self.cpsr.set_c(carry);
            if arithmetic {
                self.cpsr.set_v(overflow);
            }
        }
    }

    /// Evaluate the flexible second operand, returning its value and the carry
    /// the barrel shifter produced (used as the C flag for logical operations).
    fn eval_operand2(&self, operand2: &Operand2) -> (u32, bool) {
        match operand2 {
            Operand2::Immediate { value, rotate } => {
                let amount = *rotate as u32 * 2;
                let result = (*value as u32).rotate_right(amount);
                let carry = if amount == 0 {
                    self.cpsr.c()
                } else {
                    result & (1 << 31) != 0
                };
                (result, carry)
            }
            Operand2::Register { rm, shift } => {
                let value = self.reg(*rm);
                match shift.source {
                    ShiftSource::Immediate(amount) => {
                        shift_by_immediate(shift.kind, value, amount, self.cpsr.c())
                    }
                    ShiftSource::Register(rs) => {
                        let amount = self.reg(rs) & 0xFF;
                        shift_by_register(shift.kind, value, amount, self.cpsr.c())
                    }
                }
            }
        }
    }
}

/// Attach the "arithmetic" flag and destination-write flag to an ALU result.
fn append(result: (u32, bool, bool), writes: bool) -> (u32, bool, bool, bool, bool) {
    (result.0, result.1, result.2, true, writes)
}

/// `a + b + carry`, returning `(result, carry_out, signed_overflow)`.
fn adc(a: u32, b: u32, carry: bool) -> (u32, bool, bool) {
    let sum = a as u64 + b as u64 + carry as u64;
    let result = sum as u32;
    let carry_out = sum > 0xFFFF_FFFF;
    let overflow = (a ^ result) & (b ^ result) & (1 << 31) != 0;
    (result, carry_out, overflow)
}

/// `a - b - !carry`, via `a + !b + carry`.
fn sbc(a: u32, b: u32, carry: bool) -> (u32, bool, bool) {
    adc(a, !b, carry)
}

/// `a - b`.
fn sub(a: u32, b: u32) -> (u32, bool, bool) {
    sbc(a, b, true)
}

/// A barrel shift by an immediate amount, honoring the special `#0` encodings:
/// `LSR`/`ASR #0` mean 32, and `ROR #0` is `RRX`.
fn shift_by_immediate(kind: ShiftKind, value: u32, amount: u8, carry_in: bool) -> (u32, bool) {
    match kind {
        ShiftKind::Lsl => {
            if amount == 0 {
                (value, carry_in)
            } else {
                lsl(value, amount as u32)
            }
        }
        ShiftKind::Lsr => lsr(value, if amount == 0 { 32 } else { amount as u32 }),
        ShiftKind::Asr => asr(value, if amount == 0 { 32 } else { amount as u32 }),
        ShiftKind::Ror => {
            if amount == 0 {
                // RRX: rotate right through carry by one.
                let result = (value >> 1) | ((carry_in as u32) << 31);
                (result, value & 1 != 0)
            } else {
                ror(value, amount as u32)
            }
        }
    }
}

/// A barrel shift by a register amount. An amount of zero leaves the value and
/// carry untouched; amounts of 32 or more are handled per the shift kind.
fn shift_by_register(kind: ShiftKind, value: u32, amount: u32, carry_in: bool) -> (u32, bool) {
    if amount == 0 {
        return (value, carry_in);
    }
    match kind {
        ShiftKind::Lsl => {
            if amount < 32 {
                lsl(value, amount)
            } else if amount == 32 {
                (0, value & 1 != 0)
            } else {
                (0, false)
            }
        }
        ShiftKind::Lsr => {
            if amount < 32 {
                lsr(value, amount)
            } else if amount == 32 {
                (0, value & (1 << 31) != 0)
            } else {
                (0, false)
            }
        }
        ShiftKind::Asr => {
            if amount < 32 {
                asr(value, amount)
            } else {
                let carry = value & (1 << 31) != 0;
                ((value as i32 >> 31) as u32, carry)
            }
        }
        ShiftKind::Ror => {
            let effective = amount % 32;
            if effective == 0 {
                // A non-zero multiple of 32: value unchanged, carry from bit 31.
                (value, value & (1 << 31) != 0)
            } else {
                ror(value, effective)
            }
        }
    }
}

fn lsl(value: u32, amount: u32) -> (u32, bool) {
    let carry = (value >> (32 - amount)) & 1 != 0;
    (value << amount, carry)
}

fn lsr(value: u32, amount: u32) -> (u32, bool) {
    if amount == 32 {
        (0, value & (1 << 31) != 0)
    } else {
        let carry = (value >> (amount - 1)) & 1 != 0;
        (value >> amount, carry)
    }
}

fn asr(value: u32, amount: u32) -> (u32, bool) {
    if amount >= 32 {
        let carry = value & (1 << 31) != 0;
        ((value as i32 >> 31) as u32, carry)
    } else {
        let carry = (value >> (amount - 1)) & 1 != 0;
        ((value as i32 >> amount) as u32, carry)
    }
}

fn ror(value: u32, amount: u32) -> (u32, bool) {
    let result = value.rotate_right(amount);
    let carry = result & (1 << 31) != 0;
    (result, carry)
}

#[cfg(test)]
mod tests;
