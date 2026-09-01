//! Disassembly of decoded ARM instructions.

use crate::disasm::{condition_suffix, immediate, reg, register_list, signed_immediate};
use crate::instruction::arm::{
    ArmInstruction, ArmOperation, BlockTransfer, Branch, BranchExchange, DataProcessing,
    DataProcessingOpcode, DoublewordTransfer, DspMulOp, HalfwordKind, HalfwordOffset,
    HalfwordTransfer, Mrs, Msr, MsrSource, Multiply, MultiplyLong, Operand2, SaturatingOp, Shift,
    ShiftKind, ShiftSource, SingleOffset, SingleTransfer, SoftwareInterrupt, Swap,
};

/// Render a decoded ARM instruction as assembly text.
pub fn format_arm(inst: &ArmInstruction) -> String {
    let cond = condition_suffix(inst.condition);
    match &inst.operation {
        ArmOperation::DataProcessing(op) => format_data_processing(op, cond),
        ArmOperation::Multiply(op) => format_multiply(op, cond),
        ArmOperation::MultiplyLong(op) => format_multiply_long(op, cond),
        ArmOperation::CountLeadingZeros(op) => {
            format!("clz{cond}\t{}, {}", reg(op.rd), reg(op.rm))
        }
        ArmOperation::SaturatingArithmetic(op) => {
            let mnem = match op.op {
                SaturatingOp::QAdd => "qadd",
                SaturatingOp::QSub => "qsub",
                SaturatingOp::QDAdd => "qdadd",
                SaturatingOp::QDSub => "qdsub",
            };
            format!(
                "{mnem}{cond}\t{}, {}, {}",
                reg(op.rd),
                reg(op.rm),
                reg(op.rn)
            )
        }
        ArmOperation::HalfwordMultiply(op) => {
            let h = |t: bool| if t { "t" } else { "b" };
            match op.op {
                DspMulOp::SmulXY => format!(
                    "smul{}{}{cond}\t{}, {}, {}",
                    h(op.x),
                    h(op.y),
                    reg(op.rd),
                    reg(op.rm),
                    reg(op.rs)
                ),
                DspMulOp::SmlaXY => format!(
                    "smla{}{}{cond}\t{}, {}, {}, {}",
                    h(op.x),
                    h(op.y),
                    reg(op.rd),
                    reg(op.rm),
                    reg(op.rs),
                    reg(op.rn)
                ),
                DspMulOp::SmulWY => format!(
                    "smulw{}{cond}\t{}, {}, {}",
                    h(op.y),
                    reg(op.rd),
                    reg(op.rm),
                    reg(op.rs)
                ),
                DspMulOp::SmlaWY => format!(
                    "smlaw{}{cond}\t{}, {}, {}, {}",
                    h(op.y),
                    reg(op.rd),
                    reg(op.rm),
                    reg(op.rs),
                    reg(op.rn)
                ),
                DspMulOp::SmlalXY => format!(
                    "smlal{}{}{cond}\t{}, {}, {}, {}",
                    h(op.x),
                    h(op.y),
                    reg(op.rn),
                    reg(op.rd),
                    reg(op.rm),
                    reg(op.rs)
                ),
            }
        }
        ArmOperation::SingleTransfer(op) => format_single_transfer(op, cond),
        ArmOperation::HalfwordTransfer(op) => format_halfword_transfer(op, cond),
        ArmOperation::DoublewordTransfer(op) => format_doubleword_transfer(op, cond),
        ArmOperation::BlockTransfer(op) => format_block_transfer(op, cond),
        ArmOperation::Swap(op) => format_swap(op, cond),
        ArmOperation::Branch(op) => format_branch(op, cond),
        ArmOperation::BranchExchange(op) => format_branch_exchange(op, cond),
        ArmOperation::BranchLinkExchange(op) => format!("blx\t#{}", op.offset),
        ArmOperation::Breakpoint(op) => format!("bkpt\t#0x{:04x}", op.comment),
        ArmOperation::CoprocessorRegisterTransfer(op) => {
            let mnemonic = if op.load { "mrc" } else { "mcr" };
            format!(
                "{mnemonic}{cond}\tp{}, {}, {}, c{}, c{}, {}",
                op.cp_num,
                op.opcode1,
                reg(op.rd),
                op.crn,
                op.crm,
                op.opcode2
            )
        }
        ArmOperation::SoftwareInterrupt(op) => format_software_interrupt(op, cond),
        ArmOperation::Mrs(op) => format_mrs(op, cond),
        ArmOperation::Msr(op) => format_msr(op, cond),
        ArmOperation::Undefined { raw } => format!(".word\t0x{raw:08x}"),
    }
}

fn dp_mnemonic(opcode: DataProcessingOpcode) -> &'static str {
    use DataProcessingOpcode::*;
    match opcode {
        And => "and",
        Eor => "eor",
        Sub => "sub",
        Rsb => "rsb",
        Add => "add",
        Adc => "adc",
        Sbc => "sbc",
        Rsc => "rsc",
        Tst => "tst",
        Teq => "teq",
        Cmp => "cmp",
        Cmn => "cmn",
        Orr => "orr",
        Mov => "mov",
        Bic => "bic",
        Mvn => "mvn",
    }
}

fn format_data_processing(op: &DataProcessing, cond: &str) -> String {
    let mnem = dp_mnemonic(op.opcode);
    let operand2 = format_operand2(&op.operand2);
    if op.opcode.is_comparison() {
        // Comparisons take no destination and always set flags.
        format!("{mnem}{cond}\t{}, {operand2}", reg(op.rn))
    } else {
        let s = if op.set_flags { "s" } else { "" };
        if matches!(
            op.opcode,
            DataProcessingOpcode::Mov | DataProcessingOpcode::Mvn
        ) {
            // Moves take no first operand.
            format!("{mnem}{cond}{s}\t{}, {operand2}", reg(op.rd))
        } else {
            format!(
                "{mnem}{cond}{s}\t{}, {}, {operand2}",
                reg(op.rd),
                reg(op.rn)
            )
        }
    }
}

fn format_operand2(operand2: &Operand2) -> String {
    match operand2 {
        Operand2::Immediate { value, rotate } => {
            immediate((*value as u32).rotate_right(*rotate as u32 * 2))
        }
        Operand2::Register { rm, shift } => match format_shift(shift) {
            Some(shift) => format!("{}, {shift}", reg(*rm)),
            None => reg(*rm).to_string(),
        },
    }
}

fn shift_name(kind: ShiftKind) -> &'static str {
    match kind {
        ShiftKind::Lsl => "lsl",
        ShiftKind::Lsr => "lsr",
        ShiftKind::Asr => "asr",
        ShiftKind::Ror => "ror",
    }
}

/// Format a barrel-shift, or `None` when there is nothing to show (`LSL #0`).
/// The special encodings surface here: `LSR`/`ASR #0` mean `#32`, and `ROR #0`
/// is `RRX`.
fn format_shift(shift: &Shift) -> Option<String> {
    match shift.source {
        ShiftSource::Immediate(0) => match shift.kind {
            ShiftKind::Lsl => None,
            ShiftKind::Lsr => Some("lsr #32".to_string()),
            ShiftKind::Asr => Some("asr #32".to_string()),
            ShiftKind::Ror => Some("rrx".to_string()),
        },
        ShiftSource::Immediate(amount) => Some(format!("{} #{amount}", shift_name(shift.kind))),
        ShiftSource::Register(rs) => Some(format!("{} {}", shift_name(shift.kind), reg(rs))),
    }
}

fn format_multiply(op: &Multiply, cond: &str) -> String {
    let s = if op.set_flags { "s" } else { "" };
    if op.accumulate {
        format!(
            "mla{cond}{s}\t{}, {}, {}, {}",
            reg(op.rd),
            reg(op.rm),
            reg(op.rs),
            reg(op.rn)
        )
    } else {
        format!(
            "mul{cond}{s}\t{}, {}, {}",
            reg(op.rd),
            reg(op.rm),
            reg(op.rs)
        )
    }
}

fn format_multiply_long(op: &MultiplyLong, cond: &str) -> String {
    let mnem = match (op.signed, op.accumulate) {
        (false, false) => "umull",
        (false, true) => "umlal",
        (true, false) => "smull",
        (true, true) => "smlal",
    };
    let s = if op.set_flags { "s" } else { "" };
    format!(
        "{mnem}{cond}{s}\t{}, {}, {}, {}",
        reg(op.rd_lo),
        reg(op.rd_hi),
        reg(op.rm),
        reg(op.rs)
    )
}

fn format_single_transfer(op: &SingleTransfer, cond: &str) -> String {
    let ldst = if op.load { "ldr" } else { "str" };
    let b = if op.byte { "b" } else { "" };
    // Post-indexed with writeback is the translated (user-mode) `T` variant.
    let t = if !op.pre_indexed && op.writeback {
        "t"
    } else {
        ""
    };
    format!(
        "{ldst}{cond}{b}{t}\t{}, {}",
        reg(op.rd),
        format_single_address(op)
    )
}

fn format_single_address(op: &SingleTransfer) -> String {
    let zero_offset = matches!(op.offset, SingleOffset::Immediate(0));
    let offset = format_single_offset(&op.offset, op.add);
    if op.pre_indexed {
        if zero_offset && !op.writeback {
            format!("[{}]", reg(op.rn))
        } else {
            let wb = if op.writeback { "!" } else { "" };
            format!("[{}, {offset}]{wb}", reg(op.rn))
        }
    } else {
        // Post-indexed always writes back, so the `!` is implicit.
        format!("[{}], {offset}", reg(op.rn))
    }
}

fn format_single_offset(offset: &SingleOffset, add: bool) -> String {
    match offset {
        SingleOffset::Immediate(imm) => signed_immediate(*imm as u32, add),
        SingleOffset::Register { rm, shift } => {
            let sign = if add { "" } else { "-" };
            let base = format!("{sign}{}", reg(*rm));
            match format_shift(shift) {
                Some(shift) => format!("{base}, {shift}"),
                None => base,
            }
        }
    }
}

fn format_halfword_transfer(op: &HalfwordTransfer, cond: &str) -> String {
    let ldst = if op.load { "ldr" } else { "str" };
    let kind = match op.kind {
        HalfwordKind::UnsignedHalfword => "h",
        HalfwordKind::SignedByte => "sb",
        HalfwordKind::SignedHalfword => "sh",
    };
    format!(
        "{ldst}{cond}{kind}\t{}, {}",
        reg(op.rd),
        format_halfword_address(op)
    )
}

fn format_halfword_address(op: &HalfwordTransfer) -> String {
    let zero_offset = matches!(op.offset, HalfwordOffset::Immediate(0));
    let offset = match &op.offset {
        HalfwordOffset::Immediate(imm) => signed_immediate(*imm as u32, op.add),
        HalfwordOffset::Register(rm) => {
            format!("{}{}", if op.add { "" } else { "-" }, reg(*rm))
        }
    };
    if op.pre_indexed {
        if zero_offset && !op.writeback {
            format!("[{}]", reg(op.rn))
        } else {
            let wb = if op.writeback { "!" } else { "" };
            format!("[{}, {offset}]{wb}", reg(op.rn))
        }
    } else {
        format!("[{}], {offset}", reg(op.rn))
    }
}

fn format_doubleword_transfer(op: &DoublewordTransfer, cond: &str) -> String {
    let ldst = if op.store { "strd" } else { "ldrd" };
    let zero_offset = matches!(op.offset, HalfwordOffset::Immediate(0));
    let offset = match &op.offset {
        HalfwordOffset::Immediate(imm) => signed_immediate(*imm as u32, op.add),
        HalfwordOffset::Register(rm) => {
            format!("{}{}", if op.add { "" } else { "-" }, reg(*rm))
        }
    };
    let address = if op.pre_indexed {
        if zero_offset && !op.writeback {
            format!("[{}]", reg(op.rn))
        } else {
            let wb = if op.writeback { "!" } else { "" };
            format!("[{}, {offset}]{wb}", reg(op.rn))
        }
    } else {
        format!("[{}], {offset}", reg(op.rn))
    };
    format!("{ldst}{cond}\t{}, {address}", reg(op.rd))
}

fn format_block_transfer(op: &BlockTransfer, cond: &str) -> String {
    let ldst = if op.load { "ldm" } else { "stm" };
    let mode = match (op.pre_indexed, op.add) {
        (false, true) => "ia",
        (true, true) => "ib",
        (false, false) => "da",
        (true, false) => "db",
    };
    let wb = if op.writeback { "!" } else { "" };
    let caret = if op.psr_force_user { "^" } else { "" };
    format!(
        "{ldst}{cond}{mode}\t{}{wb}, {}{caret}",
        reg(op.rn),
        register_list(op.register_list)
    )
}

fn format_swap(op: &Swap, cond: &str) -> String {
    let b = if op.byte { "b" } else { "" };
    format!(
        "swp{cond}{b}\t{}, {}, [{}]",
        reg(op.rd),
        reg(op.rm),
        reg(op.rn)
    )
}

fn format_branch(op: &Branch, cond: &str) -> String {
    let mnem = if op.link { "bl" } else { "b" };
    format!("{mnem}{cond}\t#{}", op.offset)
}

fn format_branch_exchange(op: &BranchExchange, cond: &str) -> String {
    let mnem = if op.link { "blx" } else { "bx" };
    format!("{mnem}{cond}\t{}", reg(op.rn))
}

fn format_software_interrupt(op: &SoftwareInterrupt, cond: &str) -> String {
    format!("swi{cond}\t#0x{:x}", op.comment)
}

fn format_mrs(op: &Mrs, cond: &str) -> String {
    let psr = if op.source_spsr { "spsr" } else { "cpsr" };
    format!("mrs{cond}\t{}, {psr}", reg(op.rd))
}

fn format_msr(op: &Msr, cond: &str) -> String {
    let psr = if op.dest_spsr { "spsr" } else { "cpsr" };
    let mut fields = String::new();
    if op.write_flags {
        fields.push('f');
    }
    if op.write_status {
        fields.push('s');
    }
    if op.write_extension {
        fields.push('x');
    }
    if op.write_control {
        fields.push('c');
    }
    let source = match &op.source {
        MsrSource::Register(rm) => reg(*rm).to_string(),
        MsrSource::Immediate { value, rotate } => {
            immediate((*value as u32).rotate_right(*rotate as u32 * 2))
        }
    };
    format!("msr{cond}\t{psr}_{fields}, {source}")
}

#[cfg(test)]
mod tests {
    use crate::decode::decode_arm;
    use crate::disasm::format_arm;

    fn disasm(raw: u32) -> String {
        format_arm(&decode_arm(raw))
    }

    #[test]
    fn data_processing() {
        assert_eq!(disasm(0xE281_1001), "add\tr1, r1, #1");
        assert_eq!(disasm(0xE1A0_0181), "mov\tr0, r1, lsl #3");
        assert_eq!(disasm(0xE350_0001), "cmp\tr0, #1");
        // MOVS with a rotated immediate: 0xFF ror 8 == 0xFF000000.
        assert_eq!(disasm(0xE3B0_04FF), "movs\tr0, #0xff000000");
    }

    #[test]
    fn multiply() {
        assert_eq!(disasm(0xE001_0293), "mul\tr1, r3, r2");
        assert_eq!(disasm(0xE021_3291), "mla\tr1, r1, r2, r3");
        assert_eq!(disasm(0xE081_2394), "umull\tr2, r1, r4, r3");
        assert_eq!(disasm(0xE0F1_2394), "smlals\tr2, r1, r4, r3");
    }

    #[test]
    fn transfers() {
        assert_eq!(disasm(0xE590_1004), "ldr\tr1, [r0, #4]");
        assert_eq!(disasm(0xE511_0008), "ldr\tr0, [r1, #-8]");
        assert_eq!(disasm(0xE643_2104), "strb\tr2, [r3], -r4, lsl #2");
        assert_eq!(disasm(0xE1D0_10B4), "ldrh\tr1, [r0, #4]");
        assert_eq!(disasm(0xE190_10D2), "ldrsb\tr1, [r0, r2]");
    }

    #[test]
    fn block_transfers() {
        assert_eq!(disasm(0xE8BD_8000), "ldmia\tsp!, {pc}");
        assert_eq!(disasm(0xE92D_400F), "stmdb\tsp!, {r0-r3, lr}");
    }

    #[test]
    fn control_flow() {
        assert_eq!(disasm(0xE12F_FF1E), "bx\tlr");
        assert_eq!(disasm(0xEBFF_FFFE), "bl\t#-8");
        assert_eq!(disasm(0xEA00_000A), "b\t#40");
        assert_eq!(disasm(0xEF12_3456), "swi\t#0x123456");
    }

    #[test]
    fn status_and_swap() {
        assert_eq!(disasm(0xE10F_0000), "mrs\tr0, cpsr");
        assert_eq!(disasm(0xE12F_F000), "msr\tcpsr_fsxc, r0");
        assert_eq!(disasm(0xE104_3092), "swp\tr3, r2, [r4]");
    }

    #[test]
    fn conditions_and_undefined() {
        assert_eq!(disasm(0x012F_FF1E), "bxeq\tlr");
        assert_eq!(disasm(0x1281_1001), "addne\tr1, r1, #1");
        assert_eq!(disasm(0xE791_0012), ".word\t0xe7910012");
    }
}
