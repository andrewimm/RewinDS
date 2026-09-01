//! End-to-end disassembly checks against programs assembled by a real ARM
//! toolchain (Apple clang, `--target=armv4t`/`thumbv4t`).
//!
//! The word/halfword values below are the exact `.text` output of the `.s`
//! files in `tests/fixtures/`; the expected strings are the independent
//! disassembly of the source shown alongside each. Because the bytes come from
//! a real assembler, these exercise the decoder and disassembler together
//! against real-world encodings — not hand-computed ones that could share a
//! mistake with the code under test.

use arm::{decode_arm, decode_thumb, format_arm, format_thumb};

fn check_arm(program: &[(u32, &str)]) {
    for (raw, expected) in program {
        assert_eq!(
            format_arm(&decode_arm(*raw)),
            *expected,
            "raw = 0x{raw:08X}"
        );
    }
}

fn check_thumb(program: &[(u16, &str)]) {
    for (raw, expected) in program {
        assert_eq!(
            format_thumb(&decode_thumb(*raw)),
            *expected,
            "raw = 0x{raw:04X}"
        );
    }
}

/// `tests/fixtures/sum_array.s` — sums a `count`-long word array.
#[test]
fn arm_sum_array() {
    check_arm(&[
        (0xE3A02000, "mov\tr2, #0"),       // mov   r2, #0
        (0xE3510000, "cmp\tr1, #0"),       // cmp   r1, #0
        (0x0A000003, "beq\t#12"),          // beq   .Ldone
        (0xE4903004, "ldr\tr3, [r0], #4"), // ldr  r3, [r0], #4
        (0xE0822003, "add\tr2, r2, r3"),   // add   r2, r2, r3
        (0xE2511001, "subs\tr1, r1, #1"),  // subs  r1, r1, #1
        (0x1AFFFFFB, "bne\t#-20"),         // bne   .Lloop
        (0xE1A00002, "mov\tr0, r2"),       // mov   r0, r2
        (0xE12FFF1E, "bx\tlr"),            // bx    lr
    ]);
}

/// `tests/fixtures/leaf.s` — prologue/epilogue, multiply, halfword/byte access.
#[test]
fn arm_leaf_function() {
    check_arm(&[
        (0xE92D4030, "stmdb\tsp!, {r4, r5, lr}"), // push {r4, r5, lr}
        (0xE0040190, "mul\tr4, r0, r1"),          // mul   r4, r0, r1
        (0xE0252190, "mla\tr5, r0, r1, r2"),      // mla   r5, r0, r1, r2
        (0xE1D300B4, "ldrh\tr0, [r3, #4]"),       // ldrh  r0, [r3, #4]
        (0xE5C34001, "strb\tr4, [r3, #1]"),       // strb  r4, [r3, #1]
        (0xE10F0000, "mrs\tr0, cpsr"),            // mrs   r0, cpsr
        (0xE8BD8030, "ldmia\tsp!, {r4, r5, pc}"), // pop  {r4, r5, pc}
    ]);
}

/// `tests/fixtures/thumb_program.s` — push/pop, immediates, load-address, and a
/// long branch-with-link (decoded here as its two independent halfwords).
#[test]
fn thumb_program() {
    check_thumb(&[
        (0xB510, "push\t{r4, lr}"),    // push  {r4, lr}
        (0x200A, "mov\tr0, #0xa"),     // movs  r0, #10
        (0x0081, "lsl\tr1, r0, #2"),   // lsls  r1, r0, #2
        (0xAC02, "add\tr4, sp, #8"),   // add   r4, sp, #8
        (0x6822, "ldr\tr2, [r4]"),     // ldr   r2, [r4]
        (0xF000, "bl\t(high) #0x000"), // bl    target (high half)
        (0xF801, "bl\t(low) #0x001"),  // bl    target (low half)
        (0xBD10, "pop\t{r4, pc}"),     // pop   {r4, pc}
        (0x1C40, "add\tr0, r0, #1"),   // adds  r0, r0, #1
        (0x4770, "bx\tlr"),            // bx    lr
    ]);
}
