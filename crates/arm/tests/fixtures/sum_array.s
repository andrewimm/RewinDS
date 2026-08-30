@ sum_array(r0 = ptr, r1 = count) -> r0 = sum
@ Assembled with: clang --target=armv4t-none-eabi -c sum_array.s
.text
.arm
sum_array:
    mov     r2, #0
    cmp     r1, #0
    beq     .Ldone
.Lloop:
    ldr     r3, [r0], #4
    add     r2, r2, r3
    subs    r1, r1, #1
    bne     .Lloop
.Ldone:
    mov     r0, r2
    bx      lr
