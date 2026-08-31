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
mod thumb;

pub use bus::{Bus, Timed};

use crate::condition::Condition;
use crate::decode::{decode_arm, decode_thumb};
use crate::instruction::arm::{
    ArmOperation, BlockTransfer, Branch, BranchExchange, BranchLinkExchange, Breakpoint,
    CountLeadingZeros, DataProcessing, DataProcessingOpcode, DspMulOp, HalfwordKind,
    HalfwordMultiply, HalfwordOffset, HalfwordTransfer, Mrs, Msr, MsrSource, Multiply, MultiplyLong,
    Operand2, SaturatingArithmetic, SaturatingOp, ShiftKind, ShiftSource, SingleOffset,
    SingleTransfer, SoftwareInterrupt, Swap,
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

/// The ARM architecture variant a CPU core implements.
///
/// ARMv4T is the ARM7TDMI — the GBA's CPU, and the DS's ARM7. ARMv5TE is the
/// ARM946E-S — the DS's ARM9 — a strict superset that adds `CLZ`, `BLX`, saturating
/// and DSP multiply instructions, coprocessor access, and richer interworking. One
/// interpreter serves both; this selects the handful of behaviours that differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ArmVersion {
    #[default]
    Armv4T,
    Armv5TE,
}

impl ArmVersion {
    /// Whether this core implements the ARMv5(TE) additions.
    pub fn is_v5(self) -> bool {
        matches!(self, ArmVersion::Armv5TE)
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
    /// Sticky saturation flag, set by the ARMv5TE saturating/DSP instructions. It
    /// is not present on ARMv4T (nothing there writes it), but lives in the same
    /// CPSR bit so it rides through SPSR save/restore for free.
    const Q: u32 = 1 << 27;
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
    pub fn q(self) -> bool {
        self.flag(Self::Q)
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
    pub fn set_q(&mut self, v: bool) {
        self.set_flag(Self::Q, v);
    }
    pub fn set_thumb(&mut self, v: bool) {
        self.set_flag(Self::T, v);
    }
    pub fn set_irq_disabled(&mut self, v: bool) {
        self.set_flag(Self::I, v);
    }
    pub fn set_fiq_disabled(&mut self, v: bool) {
        self.set_flag(Self::F, v);
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
    /// Banked r13/r14 for the inactive modes, indexed by [`bank13`].
    banked_r13_r14: [[u32; 2]; 6],
    /// Banked r8..r12: index 0 is the non-FIQ bank, index 1 is FIQ.
    banked_r8_r12: [[u32; 5]; 2],
    /// SPSRs for the exception modes, indexed by [`spsr_index`].
    spsr: [Psr; 5],
    /// Total guest cycles executed.
    cycles: u64,
    /// Whether the current instruction redirected the PC.
    branched: bool,
    /// Whether the next fetch is sequential with the previous access.
    sequential: bool,
    /// Whether the current instruction performed a data access (which breaks the
    /// fetch sequence for the next opcode).
    data_access: bool,
    /// The architecture variant this core implements. Gates the ARMv5TE-only
    /// behaviours (interworking rules, and trapping v5 instructions as undefined on
    /// ARMv4T). ARM7 cores keep the default [`ArmVersion::Armv4T`].
    version: ArmVersion,
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpu {
    /// A fresh ARMv4T (ARM7TDMI) core — the GBA CPU and the DS's ARM7.
    pub fn new() -> Self {
        Self::with_version(ArmVersion::Armv4T)
    }

    /// A fresh core of the given architecture variant. Use [`ArmVersion::Armv5TE`]
    /// for the DS's ARM9 (ARM946E-S).
    pub fn with_version(version: ArmVersion) -> Self {
        let mut cpsr = Psr::from_bits(0);
        cpsr.set_mode(Mode::System);
        Cpu {
            r: [0; 16],
            cpsr,
            banked_r13_r14: [[0; 2]; 6],
            banked_r8_r12: [[0; 5]; 2],
            spsr: [Psr::from_bits(0); 5],
            cycles: 0,
            branched: false,
            sequential: false,
            data_access: false,
            version,
        }
    }

    /// The architecture variant this core implements.
    pub fn version(&self) -> ArmVersion {
        self.version
    }

    /// The current operating mode.
    pub fn mode(&self) -> Option<Mode> {
        self.cpsr.mode()
    }

    /// The SPSR of the current mode, if it has one.
    pub fn spsr(&self) -> Option<Psr> {
        spsr_index(self.cpsr.mode()?).map(|i| self.spsr[i])
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

    /// Set a register's raw value.
    pub fn set_register(&mut self, index: usize, value: u32) {
        self.r[index] = value;
        if index == 15 {
            self.sequential = false;
        }
    }

    /// Set the program counter and begin fetching from there.
    pub fn set_pc(&mut self, address: u32) {
        self.set_register(15, address);
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

    /// Read a register operand of a data-processing instruction that uses a
    /// register-specified shift. The extra internal cycle to read the shift
    /// amount advances the prefetch one more instruction, so `r15` reads at
    /// PC+12 (ARM) rather than PC+8.
    fn reg_register_shifted(&self, register: Register) -> u32 {
        if register.is_pc() {
            self.r[15].wrapping_add(self.pc_offset() + 4)
        } else {
            self.r[register.index()]
        }
    }

    /// Read register `index` from the User-mode bank, for `LDM/STM {..}^` without
    /// r15 in the list, which always transfers the User registers.
    fn reg_user(&self, index: usize) -> u32 {
        match index {
            8..=12 if self.cpsr.mode() == Some(Mode::Fiq) => self.banked_r8_r12[0][index - 8],
            13 | 14 if !matches!(self.cpsr.mode(), Some(Mode::User | Mode::System)) => {
                self.banked_r13_r14[0][index - 13]
            }
            _ => self.r[index],
        }
    }

    /// Write register `index` in the User-mode bank (see [`Self::reg_user`]).
    fn set_reg_user(&mut self, index: usize, value: u32) {
        match index {
            8..=12 if self.cpsr.mode() == Some(Mode::Fiq) => self.banked_r8_r12[0][index - 8] = value,
            13 | 14 if !matches!(self.cpsr.mode(), Some(Mode::User | Mode::System)) => {
                self.banked_r13_r14[0][index - 13] = value
            }
            _ => self.r[index] = value,
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
        self.data_access = false;
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
            // A data access breaks the sequential fetch stream.
            self.sequential = !self.data_access;
        }
    }

    fn step_thumb<B: Bus>(&mut self, bus: &mut B) {
        let pc = self.r[15] & !1;
        self.r[15] = pc;
        self.data_access = false;
        let fetched = bus.fetch16(pc, self.sequential);
        self.cycles += fetched.cycles as u64;

        let instruction = decode_thumb(fetched.value);
        self.execute_thumb(instruction, bus);

        if self.branched {
            self.sequential = false;
        } else {
            self.r[15] = pc.wrapping_add(2);
            self.sequential = !self.data_access;
        }
    }

    fn execute_arm<B: Bus>(&mut self, operation: ArmOperation, bus: &mut B) {
        match operation {
            ArmOperation::DataProcessing(op) => self.execute_data_processing(op),
            ArmOperation::Branch(op) => self.execute_branch(op),
            ArmOperation::BranchExchange(op) => self.execute_branch_exchange(op),
            ArmOperation::BranchLinkExchange(op) => self.execute_blx_immediate(op),
            ArmOperation::Breakpoint(op) => self.execute_breakpoint(op),
            ArmOperation::SingleTransfer(op) => self.execute_single_transfer(op, bus),
            ArmOperation::HalfwordTransfer(op) => self.execute_halfword_transfer(op, bus),
            ArmOperation::BlockTransfer(op) => self.execute_block_transfer(op, bus),
            ArmOperation::Swap(op) => self.execute_swap(op, bus),
            ArmOperation::Multiply(op) => self.execute_multiply(op, bus),
            ArmOperation::MultiplyLong(op) => self.execute_multiply_long(op, bus),
            ArmOperation::CountLeadingZeros(op) => self.execute_clz(op),
            ArmOperation::SaturatingArithmetic(op) => self.execute_saturating(op),
            ArmOperation::HalfwordMultiply(op) => self.execute_halfword_multiply(op),
            ArmOperation::Mrs(op) => self.execute_mrs(op),
            ArmOperation::Msr(op) => self.execute_msr(op),
            ArmOperation::SoftwareInterrupt(op) => self.execute_software_interrupt(op),
            ArmOperation::Undefined { .. } => self.execute_undefined(),
        }
    }

    /// Consume `count` internal cycles, advancing time and any ROM prefetcher.
    fn internal_cycles<B: Bus>(&mut self, bus: &mut B, count: u32) {
        self.cycles += count as u64;
        bus.internal(count);
    }

    /// Apply the base-register writeback for a single/halfword transfer.
    /// Post-indexed always writes back; pre-indexed writes back only with `W`.
    fn apply_writeback(&mut self, pre_indexed: bool, writeback: bool, rn: Register, value: u32) {
        if !pre_indexed || writeback {
            self.set_reg(rn, value);
        }
    }

    fn execute_single_transfer<B: Bus>(&mut self, op: SingleTransfer, bus: &mut B) {
        self.data_access = true;
        let base = self.reg(op.rn);
        let offset = self.single_offset(&op.offset);
        let offset_addr = if op.add {
            base.wrapping_add(offset)
        } else {
            base.wrapping_sub(offset)
        };
        let address = if op.pre_indexed { offset_addr } else { base };

        if op.load {
            let value = if op.byte {
                let read = bus.load8(address, false);
                self.cycles += read.cycles as u64;
                read.value as u32
            } else {
                let read = bus.load32(address & !3, false);
                self.cycles += read.cycles as u64;
                // An unaligned word load rotates the aligned word into place.
                read.value.rotate_right((address & 3) * 8)
            };
            self.internal_cycles(bus, 1); // the load-use internal cycle
            self.apply_writeback(op.pre_indexed, op.writeback, op.rn, offset_addr);
            self.set_reg(op.rd, value); // rd after writeback, so it wins if rn == rd
        } else {
            // Storing r15 stores the address of this instruction plus 12.
            let mut value = self.reg(op.rd);
            if op.rd.is_pc() {
                value = value.wrapping_add(4);
            }
            // The unaligned address goes to memory as-is; word-aligned memory
            // ignores the low bits, but the 8-bit GamePak bus uses them to select
            // which byte a wide store latches.
            let cycles = if op.byte {
                bus.store8(address, value as u8, false)
            } else {
                bus.store32(address, value, false)
            };
            self.cycles += cycles as u64;
            self.apply_writeback(op.pre_indexed, op.writeback, op.rn, offset_addr);
        }
    }

    fn execute_halfword_transfer<B: Bus>(&mut self, op: HalfwordTransfer, bus: &mut B) {
        self.data_access = true;
        let base = self.reg(op.rn);
        let offset = match op.offset {
            HalfwordOffset::Immediate(imm) => imm as u32,
            HalfwordOffset::Register(rm) => self.reg(rm),
        };
        let offset_addr = if op.add {
            base.wrapping_add(offset)
        } else {
            base.wrapping_sub(offset)
        };
        let address = if op.pre_indexed { offset_addr } else { base };

        if op.load {
            let value = match op.kind {
                HalfwordKind::UnsignedHalfword => {
                    let read = bus.load16(address & !1, false);
                    self.cycles += read.cycles as u64;
                    let value = read.value as u32;
                    if address & 1 != 0 {
                        value.rotate_right(8)
                    } else {
                        value
                    }
                }
                HalfwordKind::SignedByte => {
                    let read = bus.load8(address, false);
                    self.cycles += read.cycles as u64;
                    read.value as i8 as i32 as u32
                }
                HalfwordKind::SignedHalfword => {
                    // A misaligned (odd) LDRSH on ARM7TDMI loads a *signed byte*
                    // from that address rather than a halfword.
                    if address & 1 != 0 {
                        let read = bus.load8(address, false);
                        self.cycles += read.cycles as u64;
                        read.value as i8 as i32 as u32
                    } else {
                        let read = bus.load16(address, false);
                        self.cycles += read.cycles as u64;
                        read.value as i16 as i32 as u32
                    }
                }
            };
            self.internal_cycles(bus, 1);
            self.apply_writeback(op.pre_indexed, op.writeback, op.rn, offset_addr);
            self.set_reg(op.rd, value);
        } else {
            // Only STRH stores; the signed kinds are load-only.
            let cycles = bus.store16(address, self.reg(op.rd) as u16, false);
            self.cycles += cycles as u64;
            self.apply_writeback(op.pre_indexed, op.writeback, op.rn, offset_addr);
        }
    }

    fn execute_block_transfer<B: Bus>(&mut self, op: BlockTransfer, bus: &mut B) {
        self.data_access = true;
        let base = self.reg(op.rn);
        let count = op.register_list.count_ones();
        if count == 0 {
            // ARM7TDMI empty-list edge case: only r15 is transferred, and the base
            // is adjusted by 0x40 (as though all sixteen registers had moved).
            let (address, writeback_value) = if op.add {
                let start = if op.pre_indexed { base.wrapping_add(4) } else { base };
                (start, base.wrapping_add(0x40))
            } else {
                let low = base.wrapping_sub(0x40);
                let start = if op.pre_indexed { low } else { low.wrapping_add(4) };
                (start, low)
            };
            if op.load {
                let read = bus.load32(address, false);
                self.cycles += read.cycles as u64;
                self.internal_cycles(bus, 1);
                if op.writeback {
                    self.set_reg(op.rn, writeback_value);
                }
                self.set_reg(Register::new(15), read.value);
            } else {
                let value = self.reg(Register::new(15)).wrapping_add(4);
                self.cycles += bus.store32(address, value, false) as u64;
                if op.writeback {
                    self.set_reg(op.rn, writeback_value);
                }
            }
            return;
        }

        // Registers always transfer lowest-first at the lowest address; the
        // base and direction set where that block sits.
        let (mut address, writeback_value) = if op.add {
            let start = if op.pre_indexed { base.wrapping_add(4) } else { base };
            (start, base.wrapping_add(count * 4))
        } else {
            let low = base.wrapping_sub(count * 4);
            let start = if op.pre_indexed { low } else { low.wrapping_add(4) };
            (start, low)
        };

        // `{..}^` with r15 absent transfers the User-mode banked registers,
        // regardless of the current mode. (With r15 present, an `LDM` is instead
        // an exception return, handled below.)
        let user_bank = op.psr_force_user && op.register_list & (1 << 15) == 0;

        // When an `STM` writes back a base that is itself in the list, the value
        // stored for the base is the *new* (written-back) one unless the base is
        // the lowest register in the list (stored before the writeback happens).
        let base_index = op.rn.index();
        let store_written_back_base = !op.load
            && op.writeback
            && op.register_list & (1 << base_index) != 0
            && op.register_list.trailing_zeros() as usize != base_index;

        let mut sequential = false;
        for i in 0..16 {
            if op.register_list & (1 << i) == 0 {
                continue;
            }
            let register = Register::new(i as u8);
            if op.load {
                let read = bus.load32(address, sequential);
                self.cycles += read.cycles as u64;
                if user_bank {
                    self.set_reg_user(i, read.value);
                } else {
                    self.set_reg(register, read.value);
                }
            } else {
                // STM stores r15 as this instruction's address plus 12.
                let mut value = if store_written_back_base && i == base_index {
                    writeback_value
                } else if user_bank {
                    self.reg_user(i)
                } else {
                    self.reg(register)
                };
                if register.is_pc() {
                    value = value.wrapping_add(4);
                }
                self.cycles += bus.store32(address, value, sequential) as u64;
            }
            address = address.wrapping_add(4);
            sequential = true;
        }

        if op.load {
            self.internal_cycles(bus, 1);
        }
        if op.writeback {
            // For LDM, a base loaded from memory keeps the loaded value.
            let base_loaded = op.load && op.register_list & (1 << op.rn.index()) != 0;
            if !base_loaded {
                self.set_reg(op.rn, writeback_value);
            }
        }

        // `LDM {..., pc}^` restores the CPSR from the SPSR (exception return).
        if op.load && op.psr_force_user && op.register_list & (1 << 15) != 0 {
            if let Some(spsr) = self.current_spsr() {
                self.write_cpsr(spsr);
            }
        }
    }

    fn execute_swap<B: Bus>(&mut self, op: Swap, bus: &mut B) {
        self.data_access = true;
        let address = self.reg(op.rn);
        let loaded = if op.byte {
            let read = bus.load8(address, false);
            self.cycles += read.cycles as u64;
            let stored = self.reg(op.rm) as u8;
            self.cycles += bus.store8(address, stored, false) as u64;
            read.value as u32
        } else {
            let read = bus.load32(address & !3, false);
            self.cycles += read.cycles as u64;
            let stored = self.reg(op.rm);
            self.cycles += bus.store32(address & !3, stored, false) as u64;
            read.value.rotate_right((address & 3) * 8)
        };
        self.internal_cycles(bus, 1);
        self.set_reg(op.rd, loaded);
    }

    fn execute_multiply<B: Bus>(&mut self, op: Multiply, bus: &mut B) {
        let rs = self.reg(op.rs);
        let mut result = self.reg(op.rm).wrapping_mul(rs);
        if op.accumulate {
            result = result.wrapping_add(self.reg(op.rn));
        }
        self.set_reg(op.rd, result);
        if op.set_flags {
            self.cpsr.set_n(result & (1 << 31) != 0);
            self.cpsr.set_z(result == 0);
            // C is left unpredictable (unchanged here); V is unaffected.
        }
        let extra = if op.accumulate { 1 } else { 0 };
        self.internal_cycles(bus, multiply_cycles(rs) + extra);
    }

    fn execute_multiply_long<B: Bus>(&mut self, op: MultiplyLong, bus: &mut B) {
        let rm = self.reg(op.rm);
        let rs = self.reg(op.rs);
        let mut result = if op.signed {
            ((rm as i32 as i64).wrapping_mul(rs as i32 as i64)) as u64
        } else {
            (rm as u64).wrapping_mul(rs as u64)
        };
        if op.accumulate {
            let acc = ((self.reg(op.rd_hi) as u64) << 32) | self.reg(op.rd_lo) as u64;
            result = result.wrapping_add(acc);
        }
        self.set_reg(op.rd_lo, result as u32);
        self.set_reg(op.rd_hi, (result >> 32) as u32);
        if op.set_flags {
            self.cpsr.set_n(result & (1 << 63) != 0);
            self.cpsr.set_z(result == 0);
        }
        let extra = 1 + if op.accumulate { 1 } else { 0 };
        self.internal_cycles(bus, multiply_cycles(rs) + extra);
    }

    /// `CLZ` (ARMv5): count leading zeros. Traps as undefined on an ARMv4T core.
    fn execute_clz(&mut self, op: CountLeadingZeros) {
        if !self.version.is_v5() {
            return self.execute_undefined();
        }
        let value = self.reg(op.rm);
        self.set_reg(op.rd, value.leading_zeros());
    }

    /// `QADD`/`QSUB`/`QDADD`/`QDSUB` (ARMv5TE): signed saturating arithmetic. Any
    /// clamp to the 32-bit range sets the sticky `Q` flag. Undefined on ARMv4T.
    fn execute_saturating(&mut self, op: SaturatingArithmetic) {
        if !self.version.is_v5() {
            return self.execute_undefined();
        }
        let rm = self.reg(op.rm) as i32;
        let mut rn = self.reg(op.rn) as i32;
        let mut saturated = false;
        // The `QD` variants double the second operand first, saturating.
        if matches!(op.op, SaturatingOp::QDAdd | SaturatingOp::QDSub) {
            saturated |= rn.checked_add(rn).is_none();
            rn = rn.saturating_add(rn);
        }
        let result = match op.op {
            SaturatingOp::QAdd | SaturatingOp::QDAdd => {
                saturated |= rm.checked_add(rn).is_none();
                rm.saturating_add(rn)
            }
            SaturatingOp::QSub | SaturatingOp::QDSub => {
                saturated |= rm.checked_sub(rn).is_none();
                rm.saturating_sub(rn)
            }
        };
        if saturated {
            self.cpsr.set_q(true); // sticky — only MSR clears it
        }
        self.set_reg(op.rd, result as u32);
    }

    /// The ARMv5TE DSP multiplies (`SMUL/SMLA` halfword and `W`/`L` forms).
    /// Undefined on ARMv4T. Only the accumulating 32-bit forms touch `Q`.
    fn execute_halfword_multiply(&mut self, op: HalfwordMultiply) {
        if !self.version.is_v5() {
            return self.execute_undefined();
        }
        // The selected 16-bit halfword of a register, sign-extended to i32.
        let half = |value: u32, top: bool| -> i32 {
            if top {
                (value >> 16) as i16 as i32
            } else {
                value as i16 as i32
            }
        };
        let rm = self.reg(op.rm);
        let rs_half = half(self.reg(op.rs), op.y);

        match op.op {
            DspMulOp::SmulXY => {
                let product = half(rm, op.x).wrapping_mul(rs_half);
                self.set_reg(op.rd, product as u32);
            }
            DspMulOp::SmlaXY => {
                let product = half(rm, op.x).wrapping_mul(rs_half);
                let (result, overflow) = product.overflowing_add(self.reg(op.rn) as i32);
                if overflow {
                    self.cpsr.set_q(true); // Q set, but the result is NOT saturated
                }
                self.set_reg(op.rd, result as u32);
            }
            DspMulOp::SmulWY => {
                let product = (rm as i32 as i64) * (rs_half as i64);
                self.set_reg(op.rd, (product >> 16) as i32 as u32);
            }
            DspMulOp::SmlaWY => {
                let product = (rm as i32 as i64) * (rs_half as i64);
                let (result, overflow) = ((product >> 16) as i32).overflowing_add(self.reg(op.rn) as i32);
                if overflow {
                    self.cpsr.set_q(true);
                }
                self.set_reg(op.rd, result as u32);
            }
            DspMulOp::SmlalXY => {
                let product = (half(rm, op.x) as i64) * (rs_half as i64);
                let acc = (((self.reg(op.rd) as u64) << 32) | self.reg(op.rn) as u64) as i64;
                let result = acc.wrapping_add(product);
                self.set_reg(op.rn, result as u32); // RdLo
                self.set_reg(op.rd, (result >> 32) as u32); // RdHi
            }
        }
    }

    /// The offset for a single data transfer: an immediate, or a register with an
    /// immediate barrel shift (this class cannot use a register shift amount).
    fn single_offset(&self, offset: &SingleOffset) -> u32 {
        match offset {
            SingleOffset::Immediate(imm) => *imm as u32,
            SingleOffset::Register { rm, shift } => {
                let value = self.reg(*rm);
                match shift.source {
                    ShiftSource::Immediate(amount) => {
                        shift_by_immediate(shift.kind, value, amount, self.cpsr.c()).0
                    }
                    ShiftSource::Register(_) => value,
                }
            }
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
        // `BLX Rn` is ARMv5-only; on ARMv4T the encoding is undefined.
        if bx.link && !self.version.is_v5() {
            return self.execute_undefined();
        }
        let target = self.reg(bx.rn);
        if bx.link {
            self.set_reg(Register::LR, self.r[15].wrapping_add(4));
        }
        self.cpsr.set_thumb(target & 1 != 0);
        self.set_reg(Register::PC, target & !1);
    }

    /// `BLX <label>` (ARMv5): branch-with-link into Thumb. On ARMv4T it sits in the
    /// never-execute (`cond == 1111`) space and is simply a NOP.
    fn execute_blx_immediate(&mut self, op: BranchLinkExchange) {
        if !self.version.is_v5() {
            return;
        }
        self.set_reg(Register::LR, self.r[15].wrapping_add(4));
        // Read PC (with the ARM +8 pipeline offset) before switching to Thumb.
        let target = self.reg(Register::PC).wrapping_add(op.offset as u32);
        self.cpsr.set_thumb(true);
        self.set_reg(Register::PC, target);
    }

    /// `BKPT` (ARMv5): take the Prefetch Abort exception. Undefined on ARMv4T.
    fn execute_breakpoint(&mut self, _op: Breakpoint) {
        if !self.version.is_v5() {
            return self.execute_undefined();
        }
        let return_address = self.r[15].wrapping_add(4);
        self.enter_exception(0x0C, Mode::Abort, return_address, false);
    }

    fn execute_data_processing(&mut self, op: DataProcessing) {
        // A register-specified shift makes any r15 operand read at PC+12.
        let register_shift = matches!(
            &op.operand2,
            Operand2::Register { shift, .. } if matches!(shift.source, ShiftSource::Register(_))
        );
        let (operand2, shifter_carry) = self.eval_operand2(&op.operand2);
        let rn = if register_shift {
            self.reg_register_shifted(op.rn)
        } else {
            self.reg(op.rn)
        };
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
            if op.rd.is_pc() {
                // With the S bit and r15 as destination, the CPSR is restored from
                // the SPSR rather than set from the result. This is the exception
                // return (`SUBS pc, lr, #4`) and the deprecated comparison forms
                // (`CMP/CMN/TST/TEQ` with Rd=15 — "TEQP" etc.), which restore the
                // mode instead of comparing.
                if let Some(spsr) = self.current_spsr() {
                    self.write_cpsr(spsr);
                }
            } else {
                self.cpsr.set_n(result & (1 << 31) != 0);
                self.cpsr.set_z(result == 0);
                self.cpsr.set_c(carry);
                if arithmetic {
                    self.cpsr.set_v(overflow);
                }
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
                match shift.source {
                    ShiftSource::Immediate(amount) => {
                        let value = self.reg(*rm);
                        shift_by_immediate(shift.kind, value, amount, self.cpsr.c())
                    }
                    ShiftSource::Register(rs) => {
                        // Register-specified shift: r15 operands read at PC+12.
                        let value = self.reg_register_shifted(*rm);
                        let amount = self.reg(rs) & 0xFF;
                        shift_by_register(shift.kind, value, amount, self.cpsr.c())
                    }
                }
            }
        }
    }

    // --- Status registers, banking, and exceptions ---

    fn current_spsr(&self) -> Option<Psr> {
        spsr_index(self.cpsr.mode()?).map(|i| self.spsr[i])
    }

    fn set_current_spsr(&mut self, value: Psr) {
        if let Some(i) = self.cpsr.mode().and_then(spsr_index) {
            self.spsr[i] = value;
        }
    }

    /// Switch to `new_mode`, moving the banked registers accordingly and setting
    /// the CPSR mode field.
    fn change_mode(&mut self, new_mode: Mode) {
        let old_mode = self.cpsr.mode().unwrap_or(Mode::System);
        if old_mode == new_mode {
            return;
        }
        let (old13, new13) = (bank13(old_mode), bank13(new_mode));
        if old13 != new13 {
            self.banked_r13_r14[old13] = [self.r[13], self.r[14]];
            let restored = self.banked_r13_r14[new13];
            self.r[13] = restored[0];
            self.r[14] = restored[1];
        }
        let old_fiq = old_mode == Mode::Fiq;
        let new_fiq = new_mode == Mode::Fiq;
        if old_fiq != new_fiq {
            self.banked_r8_r12[old_fiq as usize].copy_from_slice(&self.r[8..13]);
            let restored = self.banked_r8_r12[new_fiq as usize];
            self.r[8..13].copy_from_slice(&restored);
        }
        self.cpsr.set_mode(new_mode);
    }

    /// Write the whole CPSR, re-banking registers if the mode changed.
    fn write_cpsr(&mut self, value: Psr) {
        if let Some(new_mode) = value.mode() {
            self.change_mode(new_mode);
        }
        // change_mode has already applied the mode field; keep the rest.
        self.cpsr = value;
    }

    /// Enter an exception: save the CPSR to the target mode's SPSR, set its LR,
    /// switch to ARM state with IRQs masked, and jump to the vector.
    fn enter_exception(&mut self, vector: u32, mode: Mode, return_address: u32, disable_fiq: bool) {
        let saved = self.cpsr;
        self.change_mode(mode);
        self.set_current_spsr(saved);
        self.r[14] = return_address;
        self.cpsr.set_thumb(false);
        self.cpsr.set_irq_disabled(true);
        if disable_fiq {
            self.cpsr.set_fiq_disabled(true);
        }
        self.r[15] = vector;
        self.branched = true;
        self.sequential = false;
    }

    /// Whether an IRQ can currently be accepted (the CPSR I-bit is clear).
    pub fn irq_enabled(&self) -> bool {
        !self.cpsr.irq_disabled()
    }

    /// Take an IRQ exception. The machine decides when the line is asserted and
    /// interrupts are enabled; this performs the entry.
    pub fn take_irq(&mut self) {
        let return_address = self.r[15].wrapping_add(4);
        self.enter_exception(0x18, Mode::Irq, return_address, false);
    }

    fn execute_software_interrupt(&mut self, _swi: SoftwareInterrupt) {
        let return_address = self.r[15].wrapping_add(4);
        self.enter_exception(0x08, Mode::Supervisor, return_address, false);
    }

    fn execute_undefined(&mut self) {
        let return_address = self.r[15].wrapping_add(4);
        self.enter_exception(0x04, Mode::Undefined, return_address, false);
    }

    fn execute_mrs(&mut self, op: Mrs) {
        let value = if op.source_spsr {
            self.current_spsr().unwrap_or(self.cpsr).bits()
        } else {
            self.cpsr.bits()
        };
        self.set_reg(op.rd, value);
    }

    fn execute_msr(&mut self, op: Msr) {
        let source = match op.source {
            MsrSource::Register(rm) => self.reg(rm),
            MsrSource::Immediate { value, rotate } => (value as u32).rotate_right(rotate as u32 * 2),
        };
        let mut mask = 0u32;
        if op.write_flags {
            mask |= 0xFF00_0000;
        }
        if op.write_status {
            mask |= 0x00FF_0000;
        }
        if op.write_extension {
            mask |= 0x0000_FF00;
        }
        if op.write_control {
            mask |= 0x0000_00FF;
        }

        if op.dest_spsr {
            if let Some(current) = self.current_spsr() {
                let bits = (current.bits() & !mask) | (source & mask);
                self.set_current_spsr(Psr::from_bits(bits));
            }
        } else {
            // In User mode only the condition flags are writable; the control
            // byte (mode, interrupt masks, state) is protected.
            let effective = if self.cpsr.mode() == Some(Mode::User) {
                mask & 0xF000_0000
            } else {
                mask
            };
            let bits = (self.cpsr.bits() & !effective) | (source & effective);
            self.write_cpsr(Psr::from_bits(bits));
        }
    }
}

/// The r13/r14 bank index for a mode.
fn bank13(mode: Mode) -> usize {
    match mode {
        Mode::User | Mode::System => 0,
        Mode::Fiq => 1,
        Mode::Irq => 2,
        Mode::Supervisor => 3,
        Mode::Abort => 4,
        Mode::Undefined => 5,
    }
}

/// The SPSR index for a mode, or `None` for modes without an SPSR.
fn spsr_index(mode: Mode) -> Option<usize> {
    Some(match mode {
        Mode::Fiq => 0,
        Mode::Irq => 1,
        Mode::Supervisor => 2,
        Mode::Abort => 3,
        Mode::Undefined => 4,
        Mode::User | Mode::System => return None,
    })
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

/// The `m` internal cycles a multiply takes, from how many high bytes of the
/// `rs` operand are all-zero or all-one.
fn multiply_cycles(rs: u32) -> u32 {
    if rs & 0xFFFF_FF00 == 0 || rs & 0xFFFF_FF00 == 0xFFFF_FF00 {
        1
    } else if rs & 0xFFFF_0000 == 0 || rs & 0xFFFF_0000 == 0xFFFF_0000 {
        2
    } else if rs & 0xFF00_0000 == 0 || rs & 0xFF00_0000 == 0xFF00_0000 {
        3
    } else {
        4
    }
}

#[cfg(test)]
mod tests;
