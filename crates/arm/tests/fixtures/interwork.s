@ Round-trip interworking: ARM -> Thumb -> ARM.
@ r0 starts at 2; the Thumb routine shifts it left by 2 (->8); ARM adds 1 (->9).
@ Assembled with: clang --target=armv4t-none-eabi -c interwork.s
.text
.arm
.global _start
_start:
    mov     r0, #2
    adr     lr, back            @ ARM return address (bit0 = 0)
    adr     r1, tfunc
    orr     r1, r1, #1          @ set the Thumb bit
    bx      r1                  @ -> Thumb
back:
    add     r0, r0, #1          @ r0 = (thumb result) + 1
0:  b       0b                  @ spin

.thumb
.thumb_func
tfunc:
    lsls    r0, r0, #2          @ r0 <<= 2   (2 -> 8)
    bx      lr                  @ -> ARM
