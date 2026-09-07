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
    b swi_stub              @ 0x0D GetBiosChecksum
    b swi_stub              @ 0x0E BgAffineSet
    b swi_stub              @ 0x0F ObjAffineSet
    b swi_stub              @ 0x10 BitUnPack
    b swi_stub              @ 0x11 LZ77UnCompWram
    b swi_stub              @ 0x12 LZ77UnCompVram
    b swi_stub              @ 0x13 HuffUnComp
    b swi_stub              @ 0x14 RLUnCompWram
    b swi_stub              @ 0x15 RLUnCompVram
    b swi_stub              @ 0x16 Diff8bitUnFilterWram
    b swi_stub              @ 0x17 Diff8bitUnFilterVram
    b swi_stub              @ 0x18 Diff16bitUnFilter
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

@ Reset SWIs are not yet implemented; return as no-ops for now. (SoftReset does
@ not return on hardware — that behavior comes with the real implementation.)
swi_soft_reset:
    b swi_return
swi_reg_ram_reset:
    b swi_return
    .pool
