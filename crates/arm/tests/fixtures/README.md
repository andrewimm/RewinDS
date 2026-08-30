# Disassembler test fixtures

These `.s` files are small programs assembled by a real ARM toolchain to produce
ground-truth instruction encodings. The resulting bytes are embedded in
`tests/disasm_fixtures.rs` and checked against our disassembler output, so the
tests validate the decoder + disassembler against real-world encodings rather
than hand-computed ones.

## Regenerating the bytes

Assemble with Apple clang (any assembler targeting `armv4t`/`thumbv4t` works):

```sh
clang --target=armv4t-none-eabi   -c sum_array.s     -o sum_array.o
clang --target=armv4t-none-eabi   -c leaf.s          -o leaf.o
clang --target=thumbv4t-none-eabi -c thumb_program.s -o thumb_program.o
```

Then dump the `.text` section as little-endian words (ARM) or halfwords (Thumb).
Any objdump-style tool works; on a machine without one, this reads the ELF
directly:

```sh
python3 - <<'PY' sum_array.o 4     # width 4 for ARM, 2 for Thumb
import sys, struct
w = int(sys.argv[2])
d = open(sys.argv[1], 'rb').read()
shoff, = struct.unpack_from('<I', d, 0x20)
esz, n, stridx = struct.unpack_from('<HHH', d, 0x2E)
sh = lambda i: struct.unpack_from('<IIIIII', d, shoff + i*esz)
stroff = sh(stridx)[4]
name = lambda o: d[stroff+o:d.index(b'\0', stroff+o)].decode()
for i in range(n):
    nm, _, _, _, off, size = sh(i)
    if name(nm) == '.text':
        for o in range(0, size, w):
            print(f"0x{struct.unpack_from('<H' if w==2 else '<I', d, off+o)[0]:0{w*2}X}")
PY
```
