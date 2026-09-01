//! Disassembly of decoded Thumb instructions.

use crate::disasm::{condition_suffix, immediate, reg, register_list};
use crate::instruction::thumb::{
    AddSubOperand, LoadAddressSource, ThumbAluOp, ThumbHiRegOp, ThumbImmediateOp, ThumbInstruction,
    ThumbShiftOp, ThumbSignExtendOp,
};
use crate::register::Register;

/// Render a decoded Thumb instruction as assembly text.
pub fn format_thumb(inst: &ThumbInstruction) -> String {
    match inst {
        ThumbInstruction::MoveShifted { op, amount, rs, rd } => {
            let name = match op {
                ThumbShiftOp::Lsl => "lsl",
                ThumbShiftOp::Lsr => "lsr",
                ThumbShiftOp::Asr => "asr",
            };
            // For LSR/ASR the encoded amount 0 means 32; LSL keeps 0 as-is.
            let shown = match (op, amount) {
                (ThumbShiftOp::Lsr | ThumbShiftOp::Asr, 0) => 32,
                _ => *amount as u32,
            };
            format!("{name}\t{}, {}, #{shown}", reg(*rd), reg(*rs))
        }

        ThumbInstruction::AddSubtract {
            subtract,
            operand,
            rs,
            rd,
        } => {
            let name = if *subtract { "sub" } else { "add" };
            let operand = match operand {
                AddSubOperand::Register(rn) => reg(*rn).to_string(),
                AddSubOperand::Immediate(imm) => format!("#{imm}"),
            };
            format!("{name}\t{}, {}, {operand}", reg(*rd), reg(*rs))
        }

        ThumbInstruction::AluImmediate {
            op,
            rd,
            immediate: imm,
        } => {
            let name = match op {
                ThumbImmediateOp::Mov => "mov",
                ThumbImmediateOp::Cmp => "cmp",
                ThumbImmediateOp::Add => "add",
                ThumbImmediateOp::Sub => "sub",
            };
            format!("{name}\t{}, {}", reg(*rd), immediate(*imm as u32))
        }

        ThumbInstruction::AluOperation { op, rs, rd } => {
            format!("{}\t{}, {}", thumb_alu_name(*op), reg(*rd), reg(*rs))
        }

        ThumbInstruction::HiRegister { op, rs, rd } => match op {
            ThumbHiRegOp::Bx => format!("bx\t{}", reg(*rs)),
            ThumbHiRegOp::Add => format!("add\t{}, {}", reg(*rd), reg(*rs)),
            ThumbHiRegOp::Cmp => format!("cmp\t{}, {}", reg(*rd), reg(*rs)),
            ThumbHiRegOp::Mov => format!("mov\t{}, {}", reg(*rd), reg(*rs)),
        },

        ThumbInstruction::PcRelativeLoad { rd, word8 } => {
            format!("ldr\t{}, [pc, {}]", reg(*rd), immediate(*word8 as u32 * 4))
        }

        ThumbInstruction::LoadStoreRegister {
            load,
            byte,
            ro,
            rb,
            rd,
        } => {
            let name = load_store_name(*load, *byte);
            format!("{name}\t{}, [{}, {}]", reg(*rd), reg(*rb), reg(*ro))
        }

        ThumbInstruction::LoadStoreSignExtended { op, ro, rb, rd } => {
            let name = match op {
                ThumbSignExtendOp::StoreHalfword => "strh",
                ThumbSignExtendOp::LoadHalfword => "ldrh",
                ThumbSignExtendOp::LoadSignedByte => "ldrsb",
                ThumbSignExtendOp::LoadSignedHalfword => "ldrsh",
            };
            format!("{name}\t{}, [{}, {}]", reg(*rd), reg(*rb), reg(*ro))
        }

        ThumbInstruction::LoadStoreImmediate {
            load,
            byte,
            offset,
            rb,
            rd,
        } => {
            let name = load_store_name(*load, *byte);
            let scale = if *byte { 1 } else { 4 };
            format!(
                "{name}\t{}, {}",
                reg(*rd),
                offset_address(*rb, *offset as u32 * scale)
            )
        }

        ThumbInstruction::LoadStoreHalfword {
            load,
            offset,
            rb,
            rd,
        } => {
            let name = if *load { "ldrh" } else { "strh" };
            format!(
                "{name}\t{}, {}",
                reg(*rd),
                offset_address(*rb, *offset as u32 * 2)
            )
        }

        ThumbInstruction::SpRelativeLoadStore { load, rd, word8 } => {
            let name = if *load { "ldr" } else { "str" };
            format!(
                "{name}\t{}, [sp, {}]",
                reg(*rd),
                immediate(*word8 as u32 * 4)
            )
        }

        ThumbInstruction::LoadAddress { source, rd, word8 } => {
            let base = match source {
                LoadAddressSource::Pc => "pc",
                LoadAddressSource::Sp => "sp",
            };
            format!(
                "add\t{}, {base}, {}",
                reg(*rd),
                immediate(*word8 as u32 * 4)
            )
        }

        ThumbInstruction::AdjustStackPointer { subtract, word7 } => {
            let name = if *subtract { "sub" } else { "add" };
            format!("{name}\tsp, {}", immediate(*word7 as u32 * 4))
        }

        ThumbInstruction::PushPop {
            pop,
            include_pc_lr,
            register_list: list,
        } => {
            let name = if *pop { "pop" } else { "push" };
            let mut bits = *list as u16;
            if *include_pc_lr {
                // The R bit adds LR when pushing, PC when popping.
                bits |= if *pop { 1 << 15 } else { 1 << 14 };
            }
            format!("{name}\t{}", register_list(bits))
        }

        ThumbInstruction::BlockTransfer {
            load,
            rb,
            register_list: list,
        } => {
            let name = if *load { "ldmia" } else { "stmia" };
            format!("{name}\t{}!, {}", reg(*rb), register_list(*list as u16))
        }

        ThumbInstruction::ConditionalBranch { condition, offset } => {
            format!("b{}\t#{offset}", condition_suffix(*condition))
        }

        ThumbInstruction::SoftwareInterrupt { comment } => {
            format!("swi\t#0x{comment:x}")
        }

        ThumbInstruction::Branch { offset } => format!("b\t#{offset}"),

        ThumbInstruction::LongBranchLink {
            second_half,
            exchange,
            offset,
        } => {
            // Each halfword is shown on its own; the PC-aware API combines the
            // two into a single resolved `bl`/`blx <target>`.
            let mnem = if *exchange { "blx" } else { "bl" };
            let half = if *second_half { "low" } else { "high" };
            format!("{mnem}\t({half}) #0x{offset:03x}")
        }

        ThumbInstruction::Undefined { raw } => format!(".hword\t0x{raw:04x}"),
    }
}

fn thumb_alu_name(op: ThumbAluOp) -> &'static str {
    use ThumbAluOp::*;
    match op {
        And => "and",
        Eor => "eor",
        Lsl => "lsl",
        Lsr => "lsr",
        Asr => "asr",
        Adc => "adc",
        Sbc => "sbc",
        Ror => "ror",
        Tst => "tst",
        Neg => "neg",
        Cmp => "cmp",
        Cmn => "cmn",
        Orr => "orr",
        Mul => "mul",
        Bic => "bic",
        Mvn => "mvn",
    }
}

fn load_store_name(load: bool, byte: bool) -> &'static str {
    match (load, byte) {
        (true, false) => "ldr",
        (true, true) => "ldrb",
        (false, false) => "str",
        (false, true) => "strb",
    }
}

/// Format `[rb, #offset]`, collapsing a zero offset to `[rb]`.
fn offset_address(rb: Register, offset: u32) -> String {
    if offset == 0 {
        format!("[{}]", reg(rb))
    } else {
        format!("[{}, {}]", reg(rb), immediate(offset))
    }
}

#[cfg(test)]
mod tests {
    use crate::decode::decode_thumb;
    use crate::disasm::format_thumb;

    fn disasm(raw: u16) -> String {
        format_thumb(&decode_thumb(raw))
    }

    #[test]
    fn alu_and_shift() {
        assert_eq!(disasm(0x0148), "lsl\tr0, r1, #5");
        assert_eq!(disasm(0x1888), "add\tr0, r1, r2");
        assert_eq!(disasm(0x1EC8), "sub\tr0, r1, #3");
        assert_eq!(disasm(0x252A), "mov\tr5, #0x2a");
        assert_eq!(disasm(0x435A), "mul\tr2, r3");
    }

    #[test]
    fn hi_register_and_bx() {
        assert_eq!(disasm(0x4488), "add\tr8, r1");
        assert_eq!(disasm(0x4641), "mov\tr1, r8");
        assert_eq!(disasm(0x4770), "bx\tlr");
    }

    #[test]
    fn loads_and_stores() {
        assert_eq!(disasm(0x4B10), "ldr\tr3, [pc, #0x40]");
        assert_eq!(disasm(0x5888), "ldr\tr0, [r1, r2]");
        assert_eq!(disasm(0x5688), "ldrsb\tr0, [r1, r2]");
        assert_eq!(disasm(0x6948), "ldr\tr0, [r1, #0x14]");
        assert_eq!(disasm(0x8888), "ldrh\tr0, [r1, #4]");
        assert_eq!(disasm(0x9A08), "ldr\tr2, [sp, #0x20]");
        assert_eq!(disasm(0xA440), "add\tr4, pc, #0x100");
    }

    #[test]
    fn stack_and_block() {
        assert_eq!(disasm(0xB010), "add\tsp, #0x40");
        assert_eq!(disasm(0xB084), "sub\tsp, #0x10");
        assert_eq!(disasm(0xB50F), "push\t{r0-r3, lr}");
        assert_eq!(disasm(0xBDFF), "pop\t{r0-r7, pc}");
        assert_eq!(disasm(0xC10F), "stmia\tr1!, {r0-r3}");
    }

    #[test]
    fn control_flow() {
        assert_eq!(disasm(0xD010), "beq\t#32");
        assert_eq!(disasm(0xD1FB), "bne\t#-10");
        assert_eq!(disasm(0xDFAB), "swi\t#0xab");
        assert_eq!(disasm(0xE100), "b\t#512");
        assert_eq!(disasm(0xF123), "bl\t(high) #0x123");
        assert_eq!(disasm(0xFC56), "bl\t(low) #0x456");
    }

    #[test]
    fn undefined() {
        assert_eq!(disasm(0xDE00), ".hword\t0xde00");
    }
}
