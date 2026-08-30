@ A leaf function exercising the prologue/epilogue, multiply, and
@ halfword/byte transfers.
@ Assembled with: clang --target=armv4t-none-eabi -c leaf.s
.text
.arm
leaf:
    push    {r4, r5, lr}
    mul     r4, r0, r1
    mla     r5, r0, r1, r2
    ldrh    r0, [r3, #4]
    strb    r4, [r3, #1]
    mrs     r0, cpsr
    pop     {r4, r5, pc}
