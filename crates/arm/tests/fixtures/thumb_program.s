@ A small Thumb function exercising push/pop, immediates, a load-address,
@ a load, and a long branch-with-link (two halfwords).
@ Assembled with: clang --target=thumbv4t-none-eabi -c thumb_program.s
.text
.thumb
.thumb_func
tmain:
    push    {r4, lr}
    movs    r0, #10
    lsls    r1, r0, #2
    add     r4, sp, #8
    ldr     r2, [r4]
    bl      target
    pop     {r4, pc}
.thumb_func
target:
    adds    r0, r0, #1
    bx      lr
