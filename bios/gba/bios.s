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
    b swi_stub              @ 0x04 IntrWait
    b swi_stub              @ 0x05 VBlankIntrWait
    b swi_div               @ 0x06 Div
    b swi_div_arm           @ 0x07 DivArm
    b swi_stub              @ 0x08 Sqrt
    b swi_stub              @ 0x09 ArcTan
    b swi_stub              @ 0x0A ArcTan2
    b swi_stub              @ 0x0B CpuSet
    b swi_stub              @ 0x0C CpuFastSet
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

@ Reset SWIs are not yet implemented; return as no-ops for now. (SoftReset does
@ not return on hardware — that behavior comes with the real implementation.)
swi_soft_reset:
    b swi_return
swi_reg_ram_reset:
    b swi_return
    .pool
