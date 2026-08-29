//! Disassembly of decoded instructions into human-readable assembly text.
//!
//! [`format_arm`] and [`format_thumb`] render a single decoded instruction. They
//! take no program counter, so PC-relative operands (branch displacements, the
//! Thumb long-branch halves) are shown as displacements rather than resolved
//! target addresses; a PC-aware caller — such as the emulator's
//! `disassemble(pc, n)` inspection API — resolves those against an address.
//!
//! The syntax is classic ARM assembly: `mnemonic{cond}{flags}` then a tab then
//! the operands, lowercase, with r13/r14/r15 shown as `sp`/`lr`/`pc`.

mod arm;
mod thumb;

pub use arm::format_arm;
pub use thumb::format_thumb;

use crate::condition::Condition;
use crate::register::Register;

/// Assembly name of a register: `r0`..`r12`, then `sp`, `lr`, `pc`.
pub(crate) fn reg(r: Register) -> &'static str {
    match r.0 & 0xF {
        0 => "r0",
        1 => "r1",
        2 => "r2",
        3 => "r3",
        4 => "r4",
        5 => "r5",
        6 => "r6",
        7 => "r7",
        8 => "r8",
        9 => "r9",
        10 => "r10",
        11 => "r11",
        12 => "r12",
        13 => "sp",
        14 => "lr",
        15 => "pc",
        _ => unreachable!(),
    }
}

/// The lowercase condition-code suffix, empty for `AL`.
pub(crate) fn condition_suffix(condition: Condition) -> &'static str {
    match condition {
        Condition::Eq => "eq",
        Condition::Ne => "ne",
        Condition::Cs => "cs",
        Condition::Cc => "cc",
        Condition::Mi => "mi",
        Condition::Pl => "pl",
        Condition::Vs => "vs",
        Condition::Vc => "vc",
        Condition::Hi => "hi",
        Condition::Ls => "ls",
        Condition::Ge => "ge",
        Condition::Lt => "lt",
        Condition::Gt => "gt",
        Condition::Le => "le",
        Condition::Al => "",
        Condition::Nv => "nv",
    }
}

/// Format an unsigned immediate operand: decimal below 10, hex otherwise.
pub(crate) fn immediate(value: u32) -> String {
    if value < 10 {
        format!("#{value}")
    } else {
        format!("#0x{value:x}")
    }
}

/// Format a memory-offset immediate with an explicit sign taken from `add`
/// (the `U` bit): magnitude below 10 in decimal, hex otherwise.
pub(crate) fn signed_immediate(value: u32, add: bool) -> String {
    let sign = if add { "" } else { "-" };
    if value < 10 {
        format!("#{sign}{value}")
    } else {
        format!("#{sign}0x{value:x}")
    }
}

/// Format a register-list bitmask as `{r0-r3, lr}`, compressing runs of three
/// or more consecutive registers into `first-last` ranges.
pub(crate) fn register_list(bits: u16) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i: u16 = 0;
    while i < 16 {
        if bits & (1 << i) == 0 {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i;
        while end + 1 < 16 && bits & (1 << (end + 1)) != 0 {
            end += 1;
        }
        if end - start >= 2 {
            parts.push(format!(
                "{}-{}",
                reg(Register::new(start as u8)),
                reg(Register::new(end as u8))
            ));
        } else {
            for r in start..=end {
                parts.push(reg(Register::new(r as u8)).to_string());
            }
        }
        i = end + 1;
    }
    format!("{{{}}}", parts.join(", "))
}
