@ A from-scratch, freely redistributable GBA BIOS replacement.
@
@ Behavior is derived entirely from public hardware documentation (GBATEK, see
@ docs/nds_gba_arch.html) — never from disassembling Nintendo's BIOS. It runs on
@ the real CPU path: the emulator vectors reset to 0x00000000, SWI to
@ 0x00000008, and IRQ to 0x00000018, exactly as hardware does, so this image is
@ interchangeable with a real BIOS at those entry points.
@
@ Scope of this initial cut:
@   - Boot: set up the privileged-mode stacks and hand off to the cartridge.
@   - SWI dispatcher: the full comment-field decode + jump-table framework, with
@     Div/DivArm/Halt implemented as worked examples; the rest return as no-ops
@     until filled in.
@   - IRQ: the documented BIOS interrupt entry that forwards to the user handler.
@
@ Assembly constraints (so a linker is never needed — see build.sh):
@   - Control flow is PC-relative only (branches, `adr`); no `.word <label>` and
@     no `ldr rX, =<label>` (either would emit an absolute relocation).
@   - `ldr rX, =<constant>` is fine: a literal-pool *value* load is PC-relative.
@ The assembled `.text` is therefore position-independent and extracted verbatim.

.text
.arm
.global _bios_start
_bios_start:

@ ---------------------------------------------------------------------------
@ Exception vector table (0x00..0x1F): eight ARM branch opcodes, one per vector.
@ ---------------------------------------------------------------------------
vectors:
    b   reset               @ 0x00  Reset
    b   undef_handler       @ 0x04  Undefined instruction
    b   swi_handler         @ 0x08  Software interrupt (SWI)
    b   prefetch_abort      @ 0x0C  Prefetch abort
    b   data_abort          @ 0x10  Data abort
    b   reserved_vector     @ 0x14  Reserved (address exception; unused on GBA)
    b   irq_handler         @ 0x18  IRQ
    b   fiq_handler         @ 0x1C  FIQ (unused on GBA)

@ ---------------------------------------------------------------------------
@ Reset: establish the three privileged-mode stacks at their documented tops in
@ Work RAM, enter System mode, and jump to the cartridge ROM entry point in ARM
@ state. (GBATEK "BIOS RAM Usage": SP_svc=03007FE0, SP_irq=03007FA0,
@ SP_usr=03007F00; ROM entry point is 08000000.)
@ ---------------------------------------------------------------------------
reset:
    msr   cpsr_c, #0xD2         @ IRQ mode,        IRQ+FIQ disabled
    ldr   sp, =0x03007FA0       @ SP_irq
    msr   cpsr_c, #0xD3         @ Supervisor mode, IRQ+FIQ disabled
    ldr   sp, =0x03007FE0       @ SP_svc
    msr   cpsr_c, #0xDF         @ System mode,     IRQ+FIQ disabled
    ldr   sp, =0x03007F00       @ SP_usr (shared by System mode)
    ldr   r0, =0x08000000       @ cartridge ROM entry point
    bx    r0                    @ hand off (bit0 clear -> stays in ARM state)
    .pool

@ ---------------------------------------------------------------------------
@ Fault vectors. Reaching one means the guest went wrong; with no debug handler
@ yet, spin so the failure is observable rather than running off into garbage.
@ ---------------------------------------------------------------------------
undef_handler:      b undef_handler
prefetch_abort:     b prefetch_abort
data_abort:         b data_abort
reserved_vector:    b reserved_vector
fiq_handler:        b fiq_handler

@ ---------------------------------------------------------------------------
@ IRQ entry (GBATEK "BIOS Interrupt handling"). Save the caller-clobbered
@ registers on the IRQ stack, read the user handler pointer from [03007FFC], and
@ call it (in ARM or, via bx, Thumb). On return, restore and resume the
@ interrupted code with CPSR restored from SPSR_irq.
@ ---------------------------------------------------------------------------
irq_handler:
    stmfd sp!, {r0-r3, r12, lr}
    ldr   r0, =0x03007FFC       @ pointer to the 32-bit user IRQ handler
    ldr   r0, [r0]
    cmp   r0, #0                @ no handler installed -> just acknowledge/return
    beq   .Lirq_return
    adr   lr, .Lirq_return      @ user handler returns here (bx lr)
    bx    r0
.Lirq_return:
    ldmfd sp!, {r0-r3, r12, lr}
    subs  pc, lr, #4            @ PC = LR-4, CPSR = SPSR_irq
    .pool

@ ---------------------------------------------------------------------------
@ SWI entry (GBATEK "How BIOS Processes SWIs"). On entry the CPU is in
@ Supervisor mode with LR_svc = address after the SWI and SPSR_svc = caller CPSR.
@ Save SPSR + scratch, decode the comment field (ARM: bits[23:16] of the opcode
@ at LR-4; Thumb: bits[7:0] of the halfword at LR-2), and dispatch.
@ ---------------------------------------------------------------------------
swi_handler:
    stmfd sp!, {r11, r12, lr}
    mrs   r11, spsr
    stmfd sp!, {r11}            @ preserve SPSR_svc across the handler
    tst   r11, #0x20            @ SPSR T-bit set -> caller was in Thumb state
    bne   .Lswi_thumb
    ldr   r12, [lr, #-4]        @ ARM SWI opcode
    mov   r12, r12, lsr #16     @ only the upper 8 bits of the 24-bit field count
    and   r12, r12, #0xFF
    b     .Lswi_dispatch
.Lswi_thumb:
    ldrh  r12, [lr, #-2]        @ Thumb SWI opcode
    and   r12, r12, #0xFF
.Lswi_dispatch:
    cmp   r12, #SWI_COUNT       @ out-of-range comment fields are no-ops here
    bhs   swi_return
    add   pc, pc, r12, lsl #2   @ jump into the branch table (pc reads as here+8)
    nop                         @ aligns swi_table to the computed target
swi_table:
    b swi_soft_reset        @ 0x00 SoftReset
    b swi_reg_ram_reset     @ 0x01 RegisterRamReset
    b swi_halt              @ 0x02 Halt
    b swi_stub              @ 0x03 Stop/Sleep
    b swi_intr_wait         @ 0x04 IntrWait
    b swi_vblank_intr_wait  @ 0x05 VBlankIntrWait
    b swi_div               @ 0x06 Div
    b swi_div_arm           @ 0x07 DivArm
    b swi_sqrt              @ 0x08 Sqrt
    b swi_arc_tan           @ 0x09 ArcTan
    b swi_arc_tan2          @ 0x0A ArcTan2
    b swi_cpu_set           @ 0x0B CpuSet
    b swi_cpu_fast_set      @ 0x0C CpuFastSet
    b swi_get_bios_checksum @ 0x0D GetBiosChecksum
    b swi_bg_affine_set     @ 0x0E BgAffineSet
    b swi_obj_affine_set    @ 0x0F ObjAffineSet
    b swi_bit_unpack        @ 0x10 BitUnPack
    b swi_lz77_wram         @ 0x11 LZ77UnCompWram
    b swi_lz77_vram         @ 0x12 LZ77UnCompVram
    b swi_huff              @ 0x13 HuffUnComp
    b swi_rl_wram           @ 0x14 RLUnCompWram
    b swi_rl_vram           @ 0x15 RLUnCompVram
    b swi_diff8_wram        @ 0x16 Diff8bitUnFilterWram
    b swi_diff8_vram        @ 0x17 Diff8bitUnFilterVram
    b swi_diff16            @ 0x18 Diff16bitUnFilter
    b swi_stub              @ 0x19 SoundBias
    b swi_stub              @ 0x1A SoundDriverInit
    b swi_stub              @ 0x1B SoundDriverMode
    b swi_stub              @ 0x1C SoundDriverMain
    b swi_stub              @ 0x1D SoundDriverVSync
    b swi_stub              @ 0x1E SoundChannelClear
    b swi_stub              @ 0x1F MidiKey2Freq
    b swi_stub              @ 0x20 SoundWhatever0
    b swi_stub              @ 0x21 SoundWhatever1
    b swi_stub              @ 0x22 SoundWhatever2
    b swi_stub              @ 0x23 SoundWhatever3
    b swi_stub              @ 0x24 SoundWhatever4
    b swi_stub              @ 0x25 MultiBoot
    b swi_stub              @ 0x26 HardReset
    b swi_stub              @ 0x27 CustomHalt
    b swi_stub              @ 0x28 SoundDriverVSyncOff
    b swi_stub              @ 0x29 SoundDriverVSyncOn
    b swi_stub              @ 0x2A SoundGetJumpList
.equ SWI_COUNT, 0x2B            @ number of entries above; keep in sync

@ SWI return: undo the prologue and resume the caller with its CPSR restored.
swi_return:
    ldmfd sp!, {r11}
    msr   spsr_fsxc, r11        @ restore SPSR_svc (handlers may have touched it)
    ldmfd sp!, {r11, r12, lr}
    movs  pc, lr               @ return to caller; CPSR <- SPSR_svc

@ Unimplemented SWIs return without effect (real hardware varies; a no-op keeps
@ the machine well-defined until each function is implemented).
swi_stub:
    b swi_return

@ SWI 0x02 Halt: enter low-power mode via HALTCNT (0x04000301, value 0x00). The
@ GBA leaves all registers unchanged, so preserve the two scratch registers.
swi_halt:
    stmfd sp!, {r0, r1}
    ldr   r0, =0x04000301
    mov   r1, #0
    strb  r1, [r0]
    ldmfd sp!, {r0, r1}
    b     swi_return
    .pool

@ SWI 0x05 VBlankIntrWait: wait for a new V-Blank interrupt. Equivalent to
@ IntrWait with r0=1 (discard old) and r1=1 (the V-Blank flag); fall into it.
swi_vblank_intr_wait:
    mov   r0, #1
    mov   r1, #1
    @ fall through to swi_intr_wait

@ SWI 0x04 IntrWait: halt until one of the requested interrupt(s) occurs.
@   in: r0 = 0 return at once if a wanted flag is already set,
@             1 discard old flags and wait for a new one
@       r1 = interrupt flag(s) to wait for (IE/IF format)
@ The BIOS Interrupt Check Flags live at 0x03007FF8 (16-bit). The user IRQ
@ handler is responsible for ORing acknowledged interrupts into that word; this
@ function polls it across halts and clears the awaited bits before returning.
@ Interrupts are force-enabled (IME=1 and the CPSR I-bit cleared) so the wait can
@ actually be serviced; the caller's CPSR — including its I-bit — is restored by
@ the normal SWI return.
swi_intr_wait:
    stmfd sp!, {r4, r5}
    ldr   r4, =0x04000301       @ HALTCNT
    ldr   r5, =0x03007FF8       @ BIOS Interrupt Check Flags (16-bit)
    mov   r2, #0x04000000
    mov   r3, #1
    str   r3, [r2, #0x208]      @ IME = 1
    mrs   r2, cpsr
    bic   r2, r2, #0x80         @ clear the CPSR I-bit: accept IRQs during the wait
    msr   cpsr_c, r2
    cmp   r0, #0
    bne   .Liw_discard
    ldrh  r2, [r5]              @ r0==0: consume an already-pending wanted flag
    ands  r3, r2, r1
    bne   .Liw_consume
    b     .Liw_halt
.Liw_discard:
    ldrh  r2, [r5]             @ r0!=0: drop old flags so we wait for a fresh one
    bic   r2, r2, r1
    strh  r2, [r5]
.Liw_halt:
    mov   r3, #0
    strb  r3, [r4]             @ Halt; the serviced IRQ resumes at the next insn
    ldrh  r2, [r5]
    ands  r3, r2, r1
    beq   .Liw_halt
.Liw_consume:
    bic   r2, r2, r1           @ clear the awaited flag(s) on the way out
    strh  r2, [r5]
    ldmfd sp!, {r4, r5}
    b     swi_return
    .pool

@ SWI 0x07 DivArm: like Div but with the operands swapped (r0=denom, r1=number),
@ for ARM's library. Swap into Div's r0=number/r1=denom convention and fall in.
swi_div_arm:
    mov   r12, r0
    mov   r0, r1
    mov   r1, r12
    @ fall through to swi_div

@ SWI 0x06 Div: signed division, r0/r1.
@   in:  r0 = signed numerator, r1 = signed denominator
@   out: r0 = quotient (signed), r1 = remainder (signed, sign of numerator),
@        r3 = abs(quotient)
@ Truncated toward zero (e.g. -1234/10 -> -123, rem -4, abs 123). Division by
@ zero returns zeroes here rather than looping forever as real hardware does.
swi_div:
    stmfd sp!, {r2, r5}
    mov   r5, #0                @ bit0 = negate quotient, bit1 = negate remainder
    cmp   r0, #0
    rsblt r0, r0, #0            @ r0 = |numerator|
    orrlt r5, r5, #3            @ numerator < 0: flip quotient sign and remainder
    cmp   r1, #0
    rsblt r1, r1, #0            @ r1 = |denominator|
    eorlt r5, r5, #1            @ denominator < 0: flip quotient sign
    mov   r3, #0                @ quotient accumulator
    mov   r12, #0               @ remainder accumulator
    cmp   r1, #0
    beq   .Ldiv_apply           @ guard division by zero
    mov   r2, #32               @ restoring bit-by-bit division
.Ldiv_loop:
    mov   r12, r12, lsl #1
    tst   r0, #0x80000000
    orrne r12, r12, #1
    mov   r0, r0, lsl #1
    mov   r3, r3, lsl #1
    cmp   r12, r1
    subhs r12, r12, r1
    orrhs r3, r3, #1
    subs  r2, r2, #1
    bne   .Ldiv_loop
.Ldiv_apply:
    mov   r0, r3               @ quotient (r3 keeps the unsigned magnitude)
    tst   r5, #1
    rsbne r0, r0, #0
    mov   r1, r12              @ remainder
    tst   r5, #2
    rsbne r1, r1, #0
    ldmfd sp!, {r2, r5}
    b     swi_return

@ SWI 0x0B CpuSet: copy or fill memory in 4-byte or 2-byte units.
@   r0 = source, r1 = destination, r2 = length/mode:
@        bits 0-20 = unit count, bit 24 = fixed source (fill), bit 26 = 32-bit.
@ No return value; clobbers the scratch registers. GBA silently does nothing if
@ the source reaches into the BIOS area.
swi_cpu_set:
    mov   r3, r2, lsl #11
    movs  r3, r3, lsr #11       @ r3 = unit count (bits 0-20); Z set if zero
    beq   swi_return
    cmp   r0, #0x4000           @ reject a source in the BIOS region
    blo   swi_return
    tst   r2, #(1 << 26)        @ 32-bit units?
    bne   .Lcpuset_word
    tst   r2, #(1 << 24)        @ 16-bit: fixed source (fill)?
    bne   .Lcpuset_half_fill
.Lcpuset_half_copy:
    ldrh  r12, [r0], #2
    strh  r12, [r1], #2
    subs  r3, r3, #1
    bne   .Lcpuset_half_copy
    b     swi_return
.Lcpuset_half_fill:
    ldrh  r12, [r0]
.Lcpuset_half_fill_loop:
    strh  r12, [r1], #2
    subs  r3, r3, #1
    bne   .Lcpuset_half_fill_loop
    b     swi_return
.Lcpuset_word:
    tst   r2, #(1 << 24)        @ 32-bit: fixed source (fill)?
    bne   .Lcpuset_word_fill
.Lcpuset_word_copy:
    ldr   r12, [r0], #4
    str   r12, [r1], #4
    subs  r3, r3, #1
    bne   .Lcpuset_word_copy
    b     swi_return
.Lcpuset_word_fill:
    ldr   r12, [r0]
.Lcpuset_word_fill_loop:
    str   r12, [r1], #4
    subs  r3, r3, #1
    bne   .Lcpuset_word_fill_loop
    b     swi_return

@ SWI 0x0C CpuFastSet: copy or fill memory in 32-byte (8-word) blocks.
@   r0 = source, r1 = destination, r2 = length/mode:
@        bits 0-20 = word count (rounded up to a multiple of 8), bit 24 = fill.
@ Real hardware moves whole 8-word blocks; the observable result is identical to
@ the word-at-a-time loop used here.
swi_cpu_fast_set:
    mov   r3, r2, lsl #11
    movs  r3, r3, lsr #11       @ r3 = word count (bits 0-20)
    beq   swi_return
    cmp   r0, #0x4000
    blo   swi_return
    add   r3, r3, #7
    bic   r3, r3, #7            @ round up to a multiple of 8 words
    tst   r2, #(1 << 24)
    bne   .Lcfs_fill
.Lcfs_copy:
    ldr   r12, [r0], #4
    str   r12, [r1], #4
    subs  r3, r3, #1
    bne   .Lcfs_copy
    b     swi_return
.Lcfs_fill:
    ldr   r12, [r0]
.Lcfs_fill_loop:
    str   r12, [r1], #4
    subs  r3, r3, #1
    bne   .Lcfs_fill_loop
    b     swi_return

@ SWI 0x0D GetBiosChecksum (undocumented): sum the whole 16 KiB BIOS as 32-bit
@ words. Running inside the BIOS, its reads see the real bytes. Returns our own
@ image's checksum (not Nintendo's 0xBAAE187F), consistent with this BIOS.
@   out: r0 = checksum
swi_get_bios_checksum:
    mov   r0, #0
    mov   r1, #0               @ read from 0x00000000
    ldr   r2, =0x4000          @ up to 16 KiB
.Lgbc_loop:
    ldr   r3, [r1], #4
    add   r0, r0, r3
    cmp   r1, r2
    blo   .Lgbc_loop
    b     swi_return
    .pool

@ SWI 0x08 Sqrt: unsigned integer square root, floor(sqrt(r0)).
@   in:  r0 = unsigned 32-bit number
@   out: r0 = unsigned 16-bit result
@ Standard restoring bit-by-bit method (res, bit, remainder).
swi_sqrt:
    mov   r1, #0               @ result
    mov   r2, #0x40000000      @ bit = 1 << 30
.Lsqrt_align:
    cmp   r2, r0               @ shrink bit down to <= n
    bls   .Lsqrt_loop
    movs  r2, r2, lsr #2
    bne   .Lsqrt_align
.Lsqrt_loop:
    cmp   r2, #0
    beq   .Lsqrt_done
    add   r3, r1, r2           @ res + bit
    cmp   r0, r3
    bhs   .Lsqrt_setbit
    mov   r1, r1, lsr #1       @ res >>= 1
    b     .Lsqrt_shift
.Lsqrt_setbit:
    sub   r0, r0, r3           @ n -= res + bit
    add   r1, r2, r1, lsr #1   @ res = (res >> 1) + bit
.Lsqrt_shift:
    mov   r2, r2, lsr #2       @ bit >>= 2
    b     .Lsqrt_loop
.Lsqrt_done:
    mov   r0, r1
    b     swi_return

@ SWI 0x09 ArcTan: arctangent of a 1.14 signed fixed-point tangent.
@   in:  r0 = tan (bit15 sign, bit14 integer, bits13-0 fraction)
@   out: r0 = angle in 0xC000..0x4000 (-PI/2..PI/2), full circle = 0x10000
@ atan(t) = atan2(t, 1.0); 1.0 is 0x4000 in 1.14. See .Lcordic.
swi_arc_tan:
    mov   r1, r0, lsl #16
    mov   r1, r1, asr #16      @ y = sign-extended tangent
    mov   r0, #0x4000          @ x = 1.0 (1.14)
    adr   lr, .Lat_ret         @ (BL to a local label emits a relocation; set LR
    b     .Lcordic             @  by hand and branch so .text stays relocatable)
.Lat_ret:
    mov   r0, r0, lsl #16
    mov   r0, r0, lsr #16      @ present as 16-bit (negatives wrap to 0xC000..)
    b     swi_return

@ SWI 0x0A ArcTan2: full-circle arctangent of a 1.14 signed vector.
@   in:  r0 = X, r1 = Y (both bit15 sign, bit14 integer, bits13-0 fraction)
@   out: r0 = angle 0x0000..0xFFFF for 0 <= THETA < 2*PI
@ The CORDIC core needs X > 0; fold the X < 0 half-plane by rotating PI, and
@ handle the X == 0 axis explicitly.
swi_arc_tan2:
    mov   r0, r0, lsl #16
    mov   r0, r0, asr #16      @ sign-extend X
    mov   r1, r1, lsl #16
    mov   r1, r1, asr #16      @ sign-extend Y
    cmp   r0, #0
    bgt   .Lat2_call           @ X > 0: CORDIC directly
    blt   .Lat2_negx           @ X < 0: rotate by PI
    @ X == 0: +PI/2, -PI/2, or 0 on the Y sign
    cmp   r1, #0
    moveq r0, #0
    beq   .Lat2_finish
    movgt r0, #0x4000
    bgt   .Lat2_finish
    mov   r0, #0xC000
    b     .Lat2_finish
.Lat2_negx:
    rsb   r0, r0, #0           @ X = -X
    rsb   r1, r1, #0           @ Y = -Y
    adr   lr, .Lat2_negx_ret
    b     .Lcordic
.Lat2_negx_ret:
    add   r0, r0, #0x8000      @ + PI
    b     .Lat2_finish
.Lat2_call:
    adr   lr, .Lat2_finish     @ CORDIC returns straight into the finish path
    b     .Lcordic
.Lat2_finish:
    mov   r0, r0, lsl #16
    mov   r0, r0, lsr #16      @ present as 16-bit 0x0000..0xFFFF
    b     swi_return

@ CORDIC vectoring core (internal, reached by BL). Computes the signed 16-bit
@ angle atan2(y, x) for x > 0. Vectoring mode accumulates angle regardless of
@ magnitude scaling, so no gain correction is needed; the table entries are the
@ pure constants round(atan(2^-i) * 0x10000 / 2PI). Preserves LR.
@   in:  r0 = x (signed, must be > 0), r1 = y (signed)
@   out: r0 = angle (signed)
.Lcordic:
    cmp   r1, #0
    moveq r0, #0
    bxeq  lr                   @ y == 0 -> angle 0
    stmfd sp!, {r4, r5, r6}
    mov   r2, #0               @ angle accumulator
    mov   r4, #0               @ iteration i
    adr   r5, .Lcordic_table
.Lcordic_loop:
    ldr   r6, [r5, r4, lsl #2] @ atan_table[i]
    cmp   r6, #0               @ remaining entries are 0: converged
    beq   .Lcordic_done
    mov   r3, r0               @ old x
    cmp   r1, #0
    ble   .Lcordic_neg
    add   r0, r0, r1, asr r4   @ x += y >> i
    sub   r1, r1, r3, asr r4   @ y -= oldx >> i
    add   r2, r2, r6           @ angle += table[i]
    b     .Lcordic_next
.Lcordic_neg:
    sub   r0, r0, r1, asr r4   @ x -= y >> i
    add   r1, r1, r3, asr r4   @ y += oldx >> i
    sub   r2, r2, r6           @ angle -= table[i]
.Lcordic_next:
    add   r4, r4, #1
    cmp   r4, #16
    blo   .Lcordic_loop
.Lcordic_done:
    mov   r0, r2
    ldmfd sp!, {r4, r5, r6}
    bx    lr
.Lcordic_table:
    .word 0x2000, 0x12E4, 0x09FB, 0x0511, 0x028B, 0x0146, 0x00A3, 0x0051
    .word 0x0029, 0x0014, 0x000A, 0x0005, 0x0003, 0x0001, 0x0001, 0x0000

@ ---------------------------------------------------------------------------
@ BIOS decompression functions (GBATEK "BIOS Decompression Functions").
@ The "Wram"/"Vram" pairs share one core, differing only in write width: the
@ macro below emits one output byte either as an 8-bit store (Wram) or buffered
@ into 16-bit halfword stores (Vram, since VRAM cannot take 8-bit writes). Emit
@ state lives in fixed registers for the duration of a call:
@   r11 = destination write pointer
@   r10 = write mode (0 = 8-bit, 1 = 16-bit)
@   r9  = pending low byte for 16-bit mode (-1 = none)
@ r4-r10 are callee-saved, so each function stacks the ones it uses (r11/r12 are
@ already saved by the SWI prologue).
@ ---------------------------------------------------------------------------
.macro EMIT_BYTE reg
    cmp   r10, #0
    strbeq \reg, [r11], #1
    beq   .Lemit_done\@
    cmn   r9, #1               @ pending low byte? (r9 == -1 -> none)
    bne   .Lemit_flush\@
    and   r9, \reg, #0xFF       @ hold the low byte until its pair arrives
    b     .Lemit_done\@
.Lemit_flush\@:
    and   \reg, \reg, #0xFF
    orr   \reg, r9, \reg, lsl #8
    strh  \reg, [r11], #2
    mvn   r9, #0               @ clear pending
.Lemit_done\@:
.endm

@ SWI 0x11 LZ77UnCompWram / 0x12 LZ77UnCompVram.
@   r0 = source (LZ77 stream), r1 = destination.
swi_lz77_vram:
    stmfd sp!, {r4-r10}
    mov   r10, #1
    b     .Llz77_common
swi_lz77_wram:
    stmfd sp!, {r4-r10}
    mov   r10, #0
.Llz77_common:
    mvn   r9, #0               @ no pending byte
    mov   r11, r1              @ dest write pointer
    ldr   r2, [r0], #4         @ header
    mov   r2, r2, lsr #8       @ bytes remaining to output
.Llz77_block:
    cmp   r2, #0
    beq   .Llz77_done
    ldrb  r3, [r0], #1         @ flag byte, MSB first
    mov   r4, #8               @ eight blocks per flag byte
.Llz77_bit:
    cmp   r2, #0
    beq   .Llz77_done
    tst   r3, #0x80
    bne   .Llz77_ref
    ldrb  r12, [r0], #1        @ literal byte
    EMIT_BYTE r12
    sub   r2, r2, #1
    b     .Llz77_next
.Llz77_ref:
    ldrb  r5, [r0], #1         @ length high nibble + disp MSBs
    ldrb  r6, [r0], #1         @ disp LSBs
    mov   r7, r5, lsr #4
    add   r7, r7, #3           @ length = (b0 >> 4) + 3
    and   r5, r5, #0x0F
    orr   r5, r6, r5, lsl #8   @ disp
    add   r5, r5, #1           @ back-reference is at output - (disp + 1)
    cmn   r9, #1               @ a pending byte occupies one more logical position
    moveq r8, r11
    addne r8, r11, #1
    sub   r8, r8, r5           @ r8 = back-reference read pointer
.Llz77_copy:
    cmp   r2, #0
    beq   .Llz77_done
    ldrb  r12, [r8], #1
    EMIT_BYTE r12
    sub   r2, r2, #1
    subs  r7, r7, #1
    bne   .Llz77_copy
.Llz77_next:
    mov   r3, r3, lsl #1       @ next flag bit into bit 7
    subs  r4, r4, #1
    bne   .Llz77_bit
    b     .Llz77_block
.Llz77_done:
    ldmfd sp!, {r4-r10}
    b     swi_return

@ SWI 0x14 RLUnCompWram / 0x15 RLUnCompVram (run-length).
@   r0 = source, r1 = destination.
swi_rl_vram:
    stmfd sp!, {r4-r10}
    mov   r10, #1
    b     .Lrl_common
swi_rl_wram:
    stmfd sp!, {r4-r10}
    mov   r10, #0
.Lrl_common:
    mvn   r9, #0
    mov   r11, r1
    ldr   r2, [r0], #4
    mov   r2, r2, lsr #8       @ bytes remaining
.Lrl_loop:
    cmp   r2, #0
    beq   .Lrl_done
    ldrb  r3, [r0], #1         @ flag
    tst   r3, #0x80
    bne   .Lrl_run
    and   r4, r3, #0x7F
    add   r4, r4, #1           @ uncompressed run of N+1 bytes
.Lrl_lit:
    cmp   r2, #0
    beq   .Lrl_done
    ldrb  r12, [r0], #1
    EMIT_BYTE r12
    sub   r2, r2, #1
    subs  r4, r4, #1
    bne   .Lrl_lit
    b     .Lrl_loop
.Lrl_run:
    and   r4, r3, #0x7F
    add   r4, r4, #3           @ compressed run of N+3 copies
    ldrb  r5, [r0], #1         @ byte to repeat
.Lrl_runloop:
    cmp   r2, #0
    beq   .Lrl_done
    mov   r12, r5
    EMIT_BYTE r12
    sub   r2, r2, #1
    subs  r4, r4, #1
    bne   .Lrl_runloop
    b     .Lrl_loop
.Lrl_done:
    ldmfd sp!, {r4-r10}
    b     swi_return

@ SWI 0x16 Diff8bitUnFilterWram / 0x17 Diff8bitUnFilterVram.
@ Undoes a delta filter: out[0]=data[0]; out[i]=out[i-1]+data[i] (8-bit).
@   r0 = source, r1 = destination.
swi_diff8_vram:
    stmfd sp!, {r4-r10}
    mov   r10, #1
    b     .Ldiff8_common
swi_diff8_wram:
    stmfd sp!, {r4-r10}
    mov   r10, #0
.Ldiff8_common:
    mvn   r9, #0
    mov   r11, r1
    ldr   r2, [r0], #4
    mov   r2, r2, lsr #8       @ output size in bytes
    mov   r3, #0               @ running accumulator
.Ldiff8_loop:
    cmp   r2, #0
    beq   .Ldiff8_done
    ldrb  r12, [r0], #1
    add   r3, r3, r12
    and   r3, r3, #0xFF
    mov   r12, r3
    EMIT_BYTE r12
    sub   r2, r2, #1
    b     .Ldiff8_loop
.Ldiff8_done:
    ldmfd sp!, {r4-r10}
    b     swi_return

@ SWI 0x18 Diff16bitUnFilter. Like Diff8 but in 16-bit units; output is always
@ halfword writes, so it does not use the byte-emit path.
@   r0 = source, r1 = destination.
swi_diff16:
    ldr   r2, [r0], #4
    mov   r2, r2, lsr #8       @ output size in bytes
    mov   r3, #0               @ running accumulator (16-bit)
.Ldiff16_loop:
    cmp   r2, #2
    blo   .Ldiff16_done
    ldrh  r12, [r0], #2
    add   r3, r3, r12
    mov   r3, r3, lsl #16
    mov   r3, r3, lsr #16      @ wrap to 16 bits
    strh  r3, [r1], #2
    sub   r2, r2, #2
    b     .Ldiff16_loop
.Ldiff16_done:
    b     swi_return

@ SWI 0x10 BitUnPack: widen packed source units into wider destination units,
@ optionally adding a fixed offset, writing 32-bit words.
@   r0 = source, r1 = destination (word-aligned), r2 = pointer to unpack info:
@     u16 source length (bytes), u8 source unit bits, u8 dest unit bits,
@     u32 (bits0-30 offset added to units, bit31 add offset to zero units too).
swi_bit_unpack:
    stmfd sp!, {r4-r11}
    ldrh  r3, [r2]             @ source length in bytes
    ldrb  r4, [r2, #2]         @ source unit width (bits)
    ldrb  r5, [r2, #3]         @ dest unit width (bits)
    ldr   r6, [r2, #4]         @ offset + zero flag
    mov   r7, r6, lsr #31      @ zero-data flag
    bic   r6, r6, #0x80000000  @ data offset (bits 0-30)
    mov   r10, #1
    mov   r10, r10, lsl r4
    sub   r10, r10, #1         @ source unit mask
    mov   r8, #0               @ output accumulator
    mov   r9, #0               @ bits filled in accumulator
.Lbup_byte:
    cmp   r3, #0
    beq   .Lbup_done
    ldrb  r11, [r0], #1
    sub   r3, r3, #1
    mov   lr, #8               @ bits available in this byte
.Lbup_unit:
    and   r12, r11, r10        @ next source unit
    mov   r11, r11, lsr r4     @ consume its bits
    cmp   r12, #0
    bne   .Lbup_addoff
    cmp   r7, #0
    beq   .Lbup_placed         @ zero unit, no zero flag: leave as zero
.Lbup_addoff:
    add   r12, r12, r6
.Lbup_placed:
    orr   r8, r8, r12, lsl r9
    add   r9, r9, r5
    cmp   r9, #32
    blo   .Lbup_nostore
    str   r8, [r1], #4
    mov   r8, #0
    mov   r9, #0
.Lbup_nostore:
    subs  lr, lr, r4
    bgt   .Lbup_unit
    b     .Lbup_byte
.Lbup_done:
    ldmfd sp!, {r4-r11}
    b     swi_return

@ SWI 0x13 HuffUnComp. Walks the Huffman tree per bitstream bit; on reaching a
@ data node the value is emitted (data-size bits) into a 32-bit output word.
@   r0 = source (aligned by 4), r1 = destination (word-aligned).
@ Header: bits0-3 data size (bits/unit), bits4-7 type (2), bits8-31 output bytes.
@ Then a tree-size byte, the tree table (root first), and the 32-bit bitstream.
@ Node byte: bits0-5 child offset, bit6 = node1 is data, bit7 = node0 is data;
@ children live at ((node AND NOT 1) + offset*2 + 2) and +1. Bitstream MSB first.
swi_huff:
    stmfd sp!, {r4-r11, lr}
    ldr   r5, [r0], #4         @ header
    and   r7, r5, #0x0F        @ data size in bits per unit
    mov   r8, r5, lsr #8       @ total output bytes
    ldrb  r12, [r0]            @ tree-size byte (r0 now points at it: src+4)
    add   r3, r0, #1           @ root node = tree base + 1
    add   r4, r12, #1
    add   r4, r0, r4, lsl #1   @ bitstream = tree base + (treesize + 1) * 2
    mov   r2, r3               @ current node = root
    mov   r6, #0               @ bits left in the current word (force a load)
    mov   r9, #0               @ output bytes written
    mov   r10, #0              @ output accumulator
    mov   r11, #0              @ bits filled in accumulator
    mov   lr, #1
    mov   lr, lr, lsl r7
    sub   lr, lr, #1           @ data-unit mask
.Lhuf_loop:
    cmp   r9, r8
    bhs   .Lhuf_done
    cmp   r6, #0
    bne   .Lhuf_havebit
    ldr   r5, [r4], #4         @ next 32-bit chunk of the bitstream
    mov   r6, #32
.Lhuf_havebit:
    ldrb  r12, [r2]            @ node byte
    and   r0, r12, #0x3F       @ child offset
    bic   r2, r2, #1           @ (node AND NOT 1)
    add   r2, r2, r0, lsl #1
    add   r2, r2, #2           @ r2 = child0 address
    movs  r5, r5, lsl #1       @ consume the next bit (MSB first) into carry
    sub   r6, r6, #1
    bcc   .Lhuf_bit0
    add   r2, r2, #1           @ bit 1 -> child1
    tst   r12, #0x40           @ node1 end flag
    b     .Lhuf_after
.Lhuf_bit0:
    tst   r12, #0x80           @ node0 end flag
.Lhuf_after:
    beq   .Lhuf_loop           @ internal node: descend (r2 already the child)
    ldrb  r0, [r2]             @ data node: read the value
    and   r0, r0, lr           @ keep only data-size bits
    orr   r10, r10, r0, lsl r11
    add   r11, r11, r7
    cmp   r11, #32
    blo   .Lhuf_reset
    str   r10, [r1], #4
    mov   r10, #0
    mov   r11, #0
    add   r9, r9, #4
.Lhuf_reset:
    mov   r2, r3               @ back to the root for the next symbol
    b     .Lhuf_loop
.Lhuf_done:
    ldmfd sp!, {r4-r11, lr}
    b     swi_return

@ ---------------------------------------------------------------------------
@ BIOS rotation/scaling functions (GBATEK "BIOS Rotation/Scaling Functions").
@ Both build the 2x2 affine matrix from a scale (sx, sy, 8.8) and rotation:
@   PA =  sx*cos   PB = -sx*sin   PC =  sy*sin   PD =  sy*cos   (all 8.8)
@ Only the angle's upper 8 bits select an entry in a 256-step sine table (Q14,
@ derived from first principles); cos(x) = sin(x + 90 degrees). The real BIOS's
@ own table lives only in its ROM, so these are independent, not bit-identical.
@ ---------------------------------------------------------------------------

@ SWI 0x0F ObjAffineSet: write the four OBJ affine parameters per source entry.
@   r0 = source (s16 sx, s16 sy, u16 angle), r1 = destination,
@   r2 = count, r3 = byte stride between the four parameters (2 or 8 for OAM).
swi_obj_affine_set:
    stmfd sp!, {r4-r11, lr}
    ldr   r11, .Loas_taboff
.Loas_pc:
    add   r11, pc, r11         @ r11 = sine table (pc reads as .Loas_pc + 8)
.Loas_loop:
    cmp   r2, #0
    beq   .Loas_done
    ldrsh r4, [r0], #2         @ sx (8.8)
    ldrsh r5, [r0], #2         @ sy (8.8)
    ldrh  r6, [r0], #2         @ angle
    mov   r6, r6, lsr #8       @ upper 8 bits -> table index
    add   r12, r6, #64
    and   r12, r12, #0xFF
    mov   r12, r12, lsl #1         @ byte offset (armv4t ldrsh has no scaled index)
    ldrsh r12, [r11, r12]          @ cos = sin(index + 64)
    and   r6, r6, #0xFF
    mov   r6, r6, lsl #1
    ldrsh r6, [r11, r6]            @ sin
    mul   r7, r4, r12
    mov   r7, r7, asr #14      @ PA =  sx*cos
    mul   r8, r4, r6
    mov   r8, r8, asr #14
    rsb   r8, r8, #0           @ PB = -sx*sin
    mul   r9, r5, r6
    mov   r9, r9, asr #14      @ PC =  sy*sin
    mul   r10, r5, r12
    mov   r10, r10, asr #14    @ PD =  sy*cos
    strh  r7, [r1]
    strh  r8, [r1, r3]
    add   r12, r3, r3
    strh  r9, [r1, r12]        @ + 2*stride
    add   r12, r12, r3
    strh  r10, [r1, r12]       @ + 3*stride
    add   r1, r1, r3, lsl #2   @ next entry starts 4 parameters on
    sub   r2, r2, #1
    b     .Loas_loop
.Loas_done:
    ldmfd sp!, {r4-r11, lr}
    b     swi_return
.Loas_taboff:
    .word .Laffine_sintab - (.Loas_pc + 8)

@ SWI 0x0E BgAffineSet: build the BG affine matrix and the start coordinates.
@   r0 = source: s32 cx, s32 cy (24.8 centre), s16 scrx, s16 scry (display
@        centre), s16 sx, s16 sy (8.8), u16 angle.
@   r1 = destination: s16 PA, PB, PC, PD, then s32 startx, starty.
@   r2 = count.
swi_bg_affine_set:
    stmfd sp!, {r4-r11, lr}
    ldr   r11, .Lbas_taboff
.Lbas_pc:
    add   r11, pc, r11
.Lbas_loop:
    cmp   r2, #0
    beq   .Lbas_done
    ldr   r7, [r0], #4         @ cx (24.8)
    ldr   r8, [r0], #4         @ cy (24.8)
    stmfd sp!, {r7, r8}        @ stash centre across the matrix computation
    ldrsh r3, [r0], #2         @ scrx (kept in r3)
    ldrsh lr, [r0], #2         @ scry (kept in lr)
    ldrsh r4, [r0], #2         @ sx
    ldrsh r5, [r0], #2         @ sy
    ldrh  r6, [r0], #2         @ angle
    mov   r6, r6, lsr #8
    add   r12, r6, #64
    and   r12, r12, #0xFF
    mov   r12, r12, lsl #1
    ldrsh r12, [r11, r12]          @ cos
    and   r6, r6, #0xFF
    mov   r6, r6, lsl #1
    ldrsh r6, [r11, r6]            @ sin
    mul   r7, r4, r12
    mov   r7, r7, asr #14      @ PA
    mul   r8, r4, r6
    mov   r8, r8, asr #14
    rsb   r8, r8, #0           @ PB
    mul   r9, r5, r6
    mov   r9, r9, asr #14      @ PC
    mul   r10, r5, r12
    mov   r10, r10, asr #14    @ PD
    strh  r7, [r1]
    strh  r8, [r1, #2]
    strh  r9, [r1, #4]
    strh  r10, [r1, #6]
    @ startx = cx - PA*scrx - PB*scry ; starty = cy - PC*scrx - PD*scry
    mul   r4, r7, r3           @ PA*scrx
    mul   r5, r8, lr           @ PB*scry
    ldmfd sp!, {r6, r12}       @ r6 = cx, r12 = cy
    sub   r6, r6, r4
    sub   r6, r6, r5
    str   r6, [r1, #8]         @ startx
    mul   r4, r9, r3           @ PC*scrx
    mul   r5, r10, lr          @ PD*scry
    sub   r12, r12, r4
    sub   r12, r12, r5
    str   r12, [r1, #12]       @ starty
    add   r1, r1, #16
    sub   r2, r2, #1
    b     .Lbas_loop
.Lbas_done:
    ldmfd sp!, {r4-r11, lr}
    b     swi_return
.Lbas_taboff:
    .word .Laffine_sintab - (.Lbas_pc + 8)

@ 256-step sine table, Q14 (round(sin(2*PI*i/256) * 16384)). cos is read 64
@ entries (90 degrees) ahead. Pure mathematical constants.
.balign 2
.Laffine_sintab:
    .hword 0, 402, 804, 1205, 1606, 2006, 2404, 2801
    .hword 3196, 3590, 3981, 4370, 4756, 5139, 5520, 5897
    .hword 6270, 6639, 7005, 7366, 7723, 8076, 8423, 8765
    .hword 9102, 9434, 9760, 10080, 10394, 10702, 11003, 11297
    .hword 11585, 11866, 12140, 12406, 12665, 12916, 13160, 13395
    .hword 13623, 13842, 14053, 14256, 14449, 14635, 14811, 14978
    .hword 15137, 15286, 15426, 15557, 15679, 15791, 15893, 15986
    .hword 16069, 16143, 16207, 16261, 16305, 16340, 16364, 16379
    .hword 16384, 16379, 16364, 16340, 16305, 16261, 16207, 16143
    .hword 16069, 15986, 15893, 15791, 15679, 15557, 15426, 15286
    .hword 15137, 14978, 14811, 14635, 14449, 14256, 14053, 13842
    .hword 13623, 13395, 13160, 12916, 12665, 12406, 12140, 11866
    .hword 11585, 11297, 11003, 10702, 10394, 10080, 9760, 9434
    .hword 9102, 8765, 8423, 8076, 7723, 7366, 7005, 6639
    .hword 6270, 5897, 5520, 5139, 4756, 4370, 3981, 3590
    .hword 3196, 2801, 2404, 2006, 1606, 1205, 804, 402
    .hword 0, -402, -804, -1205, -1606, -2006, -2404, -2801
    .hword -3196, -3590, -3981, -4370, -4756, -5139, -5520, -5897
    .hword -6270, -6639, -7005, -7366, -7723, -8076, -8423, -8765
    .hword -9102, -9434, -9760, -10080, -10394, -10702, -11003, -11297
    .hword -11585, -11866, -12140, -12406, -12665, -12916, -13160, -13395
    .hword -13623, -13842, -14053, -14256, -14449, -14635, -14811, -14978
    .hword -15137, -15286, -15426, -15557, -15679, -15791, -15893, -15986
    .hword -16069, -16143, -16207, -16261, -16305, -16340, -16364, -16379
    .hword -16384, -16379, -16364, -16340, -16305, -16261, -16207, -16143
    .hword -16069, -15986, -15893, -15791, -15679, -15557, -15426, -15286
    .hword -15137, -14978, -14811, -14635, -14449, -14256, -14053, -13842
    .hword -13623, -13395, -13160, -12916, -12665, -12406, -12140, -11866
    .hword -11585, -11297, -11003, -10702, -10394, -10080, -9760, -9434
    .hword -9102, -8765, -8423, -8076, -7723, -7366, -7005, -6639
    .hword -6270, -5897, -5520, -5139, -4756, -4370, -3981, -3590
    .hword -3196, -2801, -2404, -2006, -1606, -1205, -804, -402

@ SWI 0x00 SoftReset. Clears the 0x200-byte BIOS RAM area, re-initialises the
@ privileged stacks, zeroes r0-r12 and the exception banks, enters System mode,
@ and jumps to the return address. The 8-bit flag at 0x03007FFA selects it:
@ 0 -> ROM (0x08000000), non-zero -> RAM (0x02000000), both in ARM state. This
@ does not return to the caller.
swi_soft_reset:
    ldr   r1, =0x03007FFA
    ldrb  r0, [r1]             @ read the return flag before clearing RAM
    cmp   r0, #0
    ldreq r10, =0x08000000
    ldrne r10, =0x02000000     @ r10 = return target (survives the clear)
    ldr   r1, =0x03007E00
    mov   r2, #0
    mov   r3, #0x200
.Lsr_clear:
    strb  r2, [r1], #1         @ clear 0x03007E00..0x03007FFF
    subs  r3, r3, #1
    bne   .Lsr_clear
    msr   cpsr_c, #0xD2        @ IRQ mode: stack, LR_irq = 0, SPSR_irq = 0
    ldr   sp, =0x03007FA0
    mov   lr, #0
    msr   spsr_fsxc, lr
    msr   cpsr_c, #0xD3        @ Supervisor mode: stack, LR_svc = 0, SPSR_svc = 0
    ldr   sp, =0x03007FE0
    mov   lr, #0
    msr   spsr_fsxc, lr
    msr   cpsr_c, #0xDF        @ System mode
    ldr   sp, =0x03007F00
    mov   lr, r10             @ return address into LR, then zero r0-r12
    mov   r0, #0
    mov   r1, #0
    mov   r2, #0
    mov   r3, #0
    mov   r4, #0
    mov   r5, #0
    mov   r6, #0
    mov   r7, #0
    mov   r8, #0
    mov   r9, #0
    mov   r10, #0
    mov   r11, #0
    mov   r12, #0
    bx    lr
    .pool

@ Clear words in [r4, r5) to zero (r6 must hold 0), inline (a BL to a local
@ label would emit a relocation). Used for both the RAM areas and I/O blocks.
.macro CLEAR_RANGE
.Lcr\@:
    cmp   r4, r5
    strlo r6, [r4], #4
    blo   .Lcr\@
.endm

@ SWI 0x01 RegisterRamReset. Clears the memory areas and resets the I/O register
@ blocks selected by the flags in r0, and always forces the screen blank
@ (DISPCNT = 0x0080). No return value.
@   r0 bit0 256K WRAM, bit1 32K WRAM (minus last 0x200), bit2 palette,
@      bit3 VRAM, bit4 OAM, bit5 SIO regs, bit6 sound regs, bit7 other regs.
swi_reg_ram_reset:
    stmfd sp!, {r4, r5, r6}
    mov   r6, #0
    tst   r0, #0x01            @ 256K on-board WRAM
    beq   .Lrrr_1
    ldr   r4, =0x02000000
    ldr   r5, =0x02040000
    CLEAR_RANGE
.Lrrr_1:
    tst   r0, #0x02            @ 32K on-chip WRAM, excluding the last 0x200 bytes
    beq   .Lrrr_2
    ldr   r4, =0x03000000
    ldr   r5, =0x03007E00
    CLEAR_RANGE
.Lrrr_2:
    tst   r0, #0x04            @ palette
    beq   .Lrrr_3
    ldr   r4, =0x05000000
    ldr   r5, =0x05000400
    CLEAR_RANGE
.Lrrr_3:
    tst   r0, #0x08            @ VRAM
    beq   .Lrrr_4
    ldr   r4, =0x06000000
    ldr   r5, =0x06018000
    CLEAR_RANGE
.Lrrr_4:
    tst   r0, #0x10            @ OAM
    beq   .Lrrr_5
    ldr   r4, =0x07000000
    ldr   r5, =0x07000400
    CLEAR_RANGE
.Lrrr_5:
    tst   r0, #0x20            @ SIO registers
    beq   .Lrrr_6
    ldr   r4, =0x04000120
    ldr   r5, =0x04000130
    CLEAR_RANGE
.Lrrr_6:
    tst   r0, #0x40            @ sound registers
    beq   .Lrrr_7
    ldr   r4, =0x04000060
    ldr   r5, =0x040000A8
    CLEAR_RANGE
.Lrrr_7:
    tst   r0, #0x80            @ other registers (display, DMA, timers, IRQ)
    beq   .Lrrr_blank
    ldr   r4, =0x04000000
    ldr   r5, =0x04000058
    CLEAR_RANGE
    ldr   r4, =0x040000B0
    ldr   r5, =0x04000110
    CLEAR_RANGE
    ldr   r4, =0x04000200
    ldr   r5, =0x0400020C
    CLEAR_RANGE
.Lrrr_blank:
    ldr   r4, =0x04000000      @ always force blank: DISPCNT = 0x0080
    mov   r5, #0x80
    strh  r5, [r4]
    ldmfd sp!, {r4, r5, r6}
    b     swi_return
    .pool
