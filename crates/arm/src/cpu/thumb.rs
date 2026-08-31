//! Thumb instruction execution.
//!
//! Thumb is a 16-bit re-encoding of a subset of ARM, so execution reuses the
//! ARM ALU helpers (`adc`/`sub`/`sbc`), the barrel shifter, and the exception
//! machinery from the parent module. The differences that matter here are the
//! implicit flag-setting, the `PC+4` pipeline offset, and a handful of dedicated
//! forms (PC/SP-relative addressing, push/pop, the two-halfword `BL`).

use super::{adc, multiply_cycles, sbc, shift_by_immediate, shift_by_register, sub, Bus, Cpu, Mode};
use crate::instruction::arm::ShiftKind;
use crate::instruction::thumb::{
    AddSubOperand, LoadAddressSource, ThumbAluOp, ThumbHiRegOp, ThumbImmediateOp, ThumbInstruction,
    ThumbShiftOp, ThumbSignExtendOp,
};
use crate::register::Register;

impl Cpu {
    fn set_nz(&mut self, result: u32) {
        self.cpsr.set_n(result & (1 << 31) != 0);
        self.cpsr.set_z(result == 0);
    }

    fn set_logical_flags(&mut self, result: u32, carry: bool) {
        self.set_nz(result);
        self.cpsr.set_c(carry);
    }

    fn set_arithmetic_flags(&mut self, result: u32, carry: bool, overflow: bool) {
        self.set_nz(result);
        self.cpsr.set_c(carry);
        self.cpsr.set_v(overflow);
    }

    pub(super) fn execute_thumb<B: Bus>(&mut self, instruction: ThumbInstruction, bus: &mut B) {
        use ThumbInstruction::*;
        match instruction {
            MoveShifted { op, amount, rs, rd } => {
                let kind = match op {
                    ThumbShiftOp::Lsl => ShiftKind::Lsl,
                    ThumbShiftOp::Lsr => ShiftKind::Lsr,
                    ThumbShiftOp::Asr => ShiftKind::Asr,
                };
                let (result, carry) = shift_by_immediate(kind, self.reg(rs), amount, self.cpsr.c());
                self.set_reg(rd, result);
                self.set_logical_flags(result, carry);
            }
            AddSubtract { subtract, operand, rs, rd } => {
                let a = self.reg(rs);
                let b = match operand {
                    AddSubOperand::Register(r) => self.reg(r),
                    AddSubOperand::Immediate(imm) => imm as u32,
                };
                let (result, carry, overflow) = if subtract { sub(a, b) } else { adc(a, b, false) };
                self.set_reg(rd, result);
                self.set_arithmetic_flags(result, carry, overflow);
            }
            AluImmediate { op, rd, immediate } => {
                let imm = immediate as u32;
                let a = self.reg(rd);
                match op {
                    ThumbImmediateOp::Mov => {
                        self.set_reg(rd, imm);
                        self.set_nz(imm);
                    }
                    ThumbImmediateOp::Cmp => {
                        let (r, c, v) = sub(a, imm);
                        self.set_arithmetic_flags(r, c, v);
                    }
                    ThumbImmediateOp::Add => {
                        let (r, c, v) = adc(a, imm, false);
                        self.set_reg(rd, r);
                        self.set_arithmetic_flags(r, c, v);
                    }
                    ThumbImmediateOp::Sub => {
                        let (r, c, v) = sub(a, imm);
                        self.set_reg(rd, r);
                        self.set_arithmetic_flags(r, c, v);
                    }
                }
            }
            AluOperation { op, rs, rd } => self.execute_thumb_alu(op, rs, rd, bus),
            HiRegister { op, rs, rd } => match op {
                ThumbHiRegOp::Add => {
                    let result = self.reg(rd).wrapping_add(self.reg(rs));
                    self.set_reg(rd, result);
                }
                ThumbHiRegOp::Cmp => {
                    let (r, c, v) = sub(self.reg(rd), self.reg(rs));
                    self.set_arithmetic_flags(r, c, v);
                }
                ThumbHiRegOp::Mov => {
                    let value = self.reg(rs);
                    self.set_reg(rd, value);
                }
                ThumbHiRegOp::Bx => {
                    let target = self.reg(rs);
                    self.cpsr.set_thumb(target & 1 != 0);
                    self.set_reg(Register::PC, target & !1);
                }
            },
            PcRelativeLoad { rd, word8 } => {
                self.data_access = true;
                let base = self.reg(Register::PC) & !3;
                let address = base.wrapping_add(word8 as u32 * 4);
                let read = bus.load32(address, false);
                self.cycles += read.cycles as u64;
                self.internal_cycles(bus, 1);
                self.set_reg(rd, read.value);
            }
            LoadStoreRegister { load, byte, ro, rb, rd } => {
                let address = self.reg(rb).wrapping_add(self.reg(ro));
                self.thumb_load_store(bus, load, byte, address, rd);
            }
            LoadStoreSignExtended { op, ro, rb, rd } => {
                let address = self.reg(rb).wrapping_add(self.reg(ro));
                self.thumb_sign_extended(bus, op, address, rd);
            }
            LoadStoreImmediate { load, byte, offset, rb, rd } => {
                let scale = if byte { 1 } else { 4 };
                let address = self.reg(rb).wrapping_add(offset as u32 * scale);
                self.thumb_load_store(bus, load, byte, address, rd);
            }
            LoadStoreHalfword { load, offset, rb, rd } => {
                self.data_access = true;
                let address = self.reg(rb).wrapping_add(offset as u32 * 2);
                if load {
                    let read = bus.load16(address & !1, false);
                    self.cycles += read.cycles as u64;
                    self.internal_cycles(bus, 1);
                    let value = read.value as u32;
                    // A misaligned (odd) LDRH reads the aligned halfword and
                    // rotates the zero-extended word right by 8.
                    let value = if address & 1 != 0 { value.rotate_right(8) } else { value };
                    self.set_reg(rd, value);
                } else {
                    self.cycles += bus.store16(address, self.reg(rd) as u16, false) as u64;
                }
            }
            SpRelativeLoadStore { load, rd, word8 } => {
                let address = self.reg(Register::SP).wrapping_add(word8 as u32 * 4);
                self.thumb_load_store(bus, load, false, address, rd);
            }
            LoadAddress { source, rd, word8 } => {
                let base = match source {
                    LoadAddressSource::Pc => self.reg(Register::PC) & !3,
                    LoadAddressSource::Sp => self.reg(Register::SP),
                };
                self.set_reg(rd, base.wrapping_add(word8 as u32 * 4));
            }
            AdjustStackPointer { subtract, word7 } => {
                let sp = self.reg(Register::SP);
                let delta = word7 as u32 * 4;
                let result = if subtract {
                    sp.wrapping_sub(delta)
                } else {
                    sp.wrapping_add(delta)
                };
                self.set_reg(Register::SP, result);
            }
            PushPop { pop, include_pc_lr, register_list } => {
                self.thumb_push_pop(bus, pop, include_pc_lr, register_list);
            }
            BlockTransfer { load, rb, register_list } => {
                self.thumb_block_transfer(bus, load, rb, register_list);
            }
            ConditionalBranch { condition, offset } => {
                if self.cpsr.passes(condition) {
                    let target = self.reg(Register::PC).wrapping_add(offset as u32);
                    self.set_reg(Register::PC, target);
                }
            }
            SoftwareInterrupt { .. } => {
                let return_address = self.r[15].wrapping_add(2);
                self.enter_exception(0x08, Mode::Supervisor, return_address, false);
            }
            Branch { offset } => {
                let target = self.reg(Register::PC).wrapping_add(offset as u32);
                self.set_reg(Register::PC, target);
            }
            LongBranchLink { second_half, exchange, offset } => {
                if exchange && !self.version.is_v5() {
                    // Thumb `BLX` is ARMv5-only; undefined on the ARM7TDMI.
                    let return_address = self.r[15].wrapping_add(2);
                    self.enter_exception(0x04, Mode::Undefined, return_address, false);
                } else if !second_half {
                    // First half: LR = (PC+4) + sign_extend(offset) << 12.
                    let high = (((offset as u32) << 21) as i32 >> 21) << 12;
                    let lr = self.reg(Register::PC).wrapping_add(high as u32);
                    self.set_reg(Register::LR, lr);
                } else {
                    // Second half: branch to LR + (offset << 1); LR = return | 1.
                    // `BLX` switches to ARM and forces word alignment.
                    let target = self.reg(Register::LR).wrapping_add((offset as u32) << 1);
                    let return_address = self.r[15].wrapping_add(2) | 1;
                    self.set_reg(Register::LR, return_address);
                    if exchange {
                        self.cpsr.set_thumb(false);
                        self.set_reg(Register::PC, target & !3);
                    } else {
                        self.set_reg(Register::PC, target);
                    }
                }
            }
            Undefined { .. } => {
                let return_address = self.r[15].wrapping_add(2);
                self.enter_exception(0x04, Mode::Undefined, return_address, false);
            }
        }
    }

    fn execute_thumb_alu<B: Bus>(&mut self, op: ThumbAluOp, rs: Register, rd: Register, bus: &mut B) {
        let a = self.reg(rd);
        let b = self.reg(rs);
        let carry_in = self.cpsr.c();
        use ThumbAluOp::*;
        match op {
            And => {
                let r = a & b;
                self.set_reg(rd, r);
                self.set_nz(r);
            }
            Eor => {
                let r = a ^ b;
                self.set_reg(rd, r);
                self.set_nz(r);
            }
            Orr => {
                let r = a | b;
                self.set_reg(rd, r);
                self.set_nz(r);
            }
            Bic => {
                let r = a & !b;
                self.set_reg(rd, r);
                self.set_nz(r);
            }
            Mvn => {
                let r = !b;
                self.set_reg(rd, r);
                self.set_nz(r);
            }
            Tst => {
                let r = a & b;
                self.set_nz(r);
            }
            Mul => {
                let r = a.wrapping_mul(b);
                self.set_reg(rd, r);
                self.set_nz(r);
                self.internal_cycles(bus, multiply_cycles(a));
            }
            Lsl => self.thumb_shift(ShiftKind::Lsl, a, b, rd),
            Lsr => self.thumb_shift(ShiftKind::Lsr, a, b, rd),
            Asr => self.thumb_shift(ShiftKind::Asr, a, b, rd),
            Ror => self.thumb_shift(ShiftKind::Ror, a, b, rd),
            Adc => {
                let (r, c, v) = adc(a, b, carry_in);
                self.set_reg(rd, r);
                self.set_arithmetic_flags(r, c, v);
            }
            Sbc => {
                let (r, c, v) = sbc(a, b, carry_in);
                self.set_reg(rd, r);
                self.set_arithmetic_flags(r, c, v);
            }
            Neg => {
                let (r, c, v) = sub(0, b);
                self.set_reg(rd, r);
                self.set_arithmetic_flags(r, c, v);
            }
            Cmp => {
                let (r, c, v) = sub(a, b);
                self.set_arithmetic_flags(r, c, v);
            }
            Cmn => {
                let (r, c, v) = adc(a, b, false);
                self.set_arithmetic_flags(r, c, v);
            }
        }
    }

    fn thumb_shift(&mut self, kind: ShiftKind, value: u32, amount: u32, rd: Register) {
        let (result, carry) = shift_by_register(kind, value, amount & 0xFF, self.cpsr.c());
        self.set_reg(rd, result);
        self.set_logical_flags(result, carry);
    }

    fn thumb_load_store<B: Bus>(
        &mut self,
        bus: &mut B,
        load: bool,
        byte: bool,
        address: u32,
        rd: Register,
    ) {
        self.data_access = true;
        if load {
            let value = if byte {
                let read = bus.load8(address, false);
                self.cycles += read.cycles as u64;
                read.value as u32
            } else {
                let read = bus.load32(address & !3, false);
                self.cycles += read.cycles as u64;
                read.value.rotate_right((address & 3) * 8)
            };
            self.internal_cycles(bus, 1);
            self.set_reg(rd, value);
        } else {
            let value = self.reg(rd);
            // Pass the unaligned address; the 8-bit GamePak bus selects the byte.
            self.cycles += if byte {
                bus.store8(address, value as u8, false)
            } else {
                bus.store32(address, value, false)
            } as u64;
        }
    }

    fn thumb_sign_extended<B: Bus>(
        &mut self,
        bus: &mut B,
        op: ThumbSignExtendOp,
        address: u32,
        rd: Register,
    ) {
        self.data_access = true;
        match op {
            ThumbSignExtendOp::StoreHalfword => {
                self.cycles += bus.store16(address, self.reg(rd) as u16, false) as u64;
            }
            ThumbSignExtendOp::LoadHalfword => {
                let read = bus.load16(address & !1, false);
                self.cycles += read.cycles as u64;
                self.internal_cycles(bus, 1);
                let value = read.value as u32;
                // A misaligned (odd) LDRH reads the aligned halfword and rotates
                // the zero-extended word right by 8.
                let value = if address & 1 != 0 { value.rotate_right(8) } else { value };
                self.set_reg(rd, value);
            }
            ThumbSignExtendOp::LoadSignedByte => {
                let read = bus.load8(address, false);
                self.cycles += read.cycles as u64;
                self.internal_cycles(bus, 1);
                self.set_reg(rd, read.value as i8 as i32 as u32);
            }
            ThumbSignExtendOp::LoadSignedHalfword => {
                // A misaligned (odd) LDRSH on ARM7TDMI loads a *signed byte* from
                // that address rather than a halfword.
                let value = if address & 1 != 0 {
                    let read = bus.load8(address, false);
                    self.cycles += read.cycles as u64;
                    read.value as i8 as i32 as u32
                } else {
                    let read = bus.load16(address, false);
                    self.cycles += read.cycles as u64;
                    read.value as i16 as i32 as u32
                };
                self.internal_cycles(bus, 1);
                self.set_reg(rd, value);
            }
        }
    }

    fn thumb_push_pop<B: Bus>(&mut self, bus: &mut B, pop: bool, include_pc_lr: bool, list: u8) {
        self.data_access = true;
        let count = list.count_ones() + include_pc_lr as u32;
        let mut sequential = false;
        if pop {
            let mut address = self.reg(Register::SP);
            for i in 0..8 {
                if list & (1 << i) != 0 {
                    let read = bus.load32(address, sequential);
                    self.cycles += read.cycles as u64;
                    self.set_reg(Register::new(i), read.value);
                    address = address.wrapping_add(4);
                    sequential = true;
                }
            }
            if include_pc_lr {
                let read = bus.load32(address, sequential);
                self.cycles += read.cycles as u64;
                // ARMv4T stays in Thumb state on a popped PC (no interworking).
                self.set_reg(Register::PC, read.value & !1);
                address = address.wrapping_add(4);
            }
            self.internal_cycles(bus, 1);
            self.set_reg(Register::SP, address);
        } else {
            let base = self.reg(Register::SP).wrapping_sub(count * 4);
            let mut address = base;
            for i in 0..8 {
                if list & (1 << i) != 0 {
                    self.cycles += bus.store32(address, self.reg(Register::new(i)), sequential) as u64;
                    address = address.wrapping_add(4);
                    sequential = true;
                }
            }
            if include_pc_lr {
                self.cycles += bus.store32(address, self.reg(Register::LR), sequential) as u64;
            }
            self.set_reg(Register::SP, base);
        }
    }

    fn thumb_block_transfer<B: Bus>(&mut self, bus: &mut B, load: bool, rb: Register, list: u8) {
        self.data_access = true;
        let base = self.reg(rb);
        if list == 0 {
            // ARM7TDMI empty-list edge case: only r15 is transferred, and the base
            // is adjusted by 0x40 (as though all sixteen registers had moved).
            let writeback = base.wrapping_add(0x40);
            if load {
                let read = bus.load32(base, false);
                self.cycles += read.cycles as u64;
                self.internal_cycles(bus, 1);
                self.set_reg(rb, writeback);
                // ARMv4T stays in Thumb state on a loaded PC (no interworking).
                self.set_reg(Register::PC, read.value & !1);
            } else {
                // The stored PC is this instruction's address plus 6.
                let value = self.reg(Register::PC).wrapping_add(2);
                self.cycles += bus.store32(base, value, false) as u64;
                self.set_reg(rb, writeback);
            }
            return;
        }

        let base_index = rb.index();
        // When an STM writes back a base that is itself in the list, the value
        // stored for the base is the *new* (written-back) one unless the base is
        // the lowest register in the list (stored before the writeback happens).
        let store_written_back_base =
            !load && list & (1 << base_index) != 0 && list.trailing_zeros() as usize != base_index;
        let writeback_value = base.wrapping_add(list.count_ones() * 4);

        let mut address = base;
        let mut sequential = false;
        for i in 0..8 {
            if list & (1 << i) == 0 {
                continue;
            }
            let register = Register::new(i);
            if load {
                let read = bus.load32(address, sequential);
                self.cycles += read.cycles as u64;
                self.set_reg(register, read.value);
            } else {
                let value = if store_written_back_base && i as usize == base_index {
                    writeback_value
                } else {
                    self.reg(register)
                };
                self.cycles += bus.store32(address, value, sequential) as u64;
            }
            address = address.wrapping_add(4);
            sequential = true;
        }
        if load {
            self.internal_cycles(bus, 1);
        }
        // Writeback, unless LDM reloaded the base from memory.
        let base_loaded = load && list & (1 << base_index) != 0;
        if !base_loaded {
            self.set_reg(rb, writeback_value);
        }
    }
}
