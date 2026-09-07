# GBA BIOS replacement

A from-scratch, freely redistributable replacement for the GBA BIOS, so RewinDS
needs no copyrighted Nintendo image to boot. It runs on the real CPU path — the
emulator vectors reset to `0x0`, SWI to `0x8`, and IRQ to `0x18` exactly as
hardware does — so this 16 KiB image is interchangeable with a real BIOS at
those entry points, and a real image can still be supplied when byte-exact
fidelity is wanted.

## Provenance

All behavior is derived from public hardware documentation (GBATEK, mirrored in
`docs/nds_gba_arch.html`). The commercial BIOS is **never** disassembled; where
we compare against a real image (see the golden tests), it is exercised only as
a black box.

## Building

```sh
./build.sh
```

Requires only Apple clang (the same assembler the `arm` crate's fixtures use)
and `rustc` — no cross-linker or `objcopy`:

1. `clang --target=armv4t-none-eabi -c bios.s` assembles a single object.
2. `extract_text.rs` pulls the `.text` section out and pads it to 16 KiB,
   producing `gba_bios.bin`.

This works without a linker because `bios.s` is written to be
position-independent: control flow is PC-relative only (branches, `adr`, and a
branch-table dispatch), and the only absolute values are literal-pool
*constants* (`ldr rN, =<number>`). There are no `.word <label>` entries or
`ldr rN, =<label>` loads, either of which would emit an absolute relocation the
extractor would have to resolve. `extract_text.rs` refuses to emit an image if
it finds a `.text` relocation section, so a mistake here fails loudly.

`gba_bios.bin` is checked in; rebuild and re-commit it after editing `bios.s`.
The `gba` crate embeds it via `gba::default_bios()`.

## What's implemented

- **Boot**: set up the privileged-mode stacks at their documented tops and hand
  off to the cartridge entry point in System / ARM state.
- **SWI dispatcher**: the full comment-field decode (ARM and Thumb) and
  jump-table framework. Implemented: `SoftReset` (0x00), `RegisterRamReset`
  (0x01), `Halt` (0x02), `IntrWait` (0x04),
  `VBlankIntrWait` (0x05), `Div` (0x06), `DivArm` (0x07), `Sqrt` (0x08),
  `ArcTan` (0x09), `ArcTan2` (0x0A), `CpuSet` (0x0B), `CpuFastSet` (0x0C),
  `BgAffineSet` (0x0E), `ObjAffineSet` (0x0F), `BitUnPack` (0x10), `LZ77UnComp`
  Wram/Vram (0x11/0x12), `HuffUnComp` (0x13), `RLUnComp` Wram/Vram (0x14/0x15),
  `Diff8bitUnFilter` Wram/Vram (0x16/0x17), and `Diff16bitUnFilter` (0x18).
  Every other SWI currently returns as a no-op.
- **IRQ**: the documented BIOS interrupt entry that saves context, forwards to
  the user handler at `[0x03007FFC]`, and returns via `subs pc, lr, #4`.

`IntrWait`/`VBlankIntrWait` force interrupts on (IME=1 and the CPSR I-bit
cleared) and halt-poll the BIOS Interrupt Check Flags at `0x03007FF8`, which the
user IRQ handler is responsible for posting to; the awaited bits are cleared
before returning. The caller's CPSR (I-bit included) is restored by the normal
SWI return.

`CpuSet` copies/fills in 16- or 32-bit units; `CpuFastSet` does 32-byte blocks
(word count rounded up to a multiple of 8). Both silently reject a source that
reaches into the BIOS area, as the GBA does. `CpuFastSet` moves words rather than
literal 8-word blocks — the result is byte-identical.

`Sqrt` is an exact integer square root (bit-identical to a real BIOS). `ArcTan`
and `ArcTan2` are **independent CORDIC approximations** derived from first
principles — the docs specify only the interface and note the real BIOS's own
inaccuracy, and its polynomial constants live only in the disassembly (off
limits). They meet the documented range/format and track the true angle to
within a few units, but are *not* bit-identical to the real BIOS. CORDIC's angle
table holds the pure constants `round(atan(2^-i) * 0x10000 / 2PI)`.

The decompressors are exact (bit-identical to a real BIOS). The "Wram"/"Vram"
pairs share one core; a byte-emit macro either does 8-bit stores (Wram) or
buffers halfword stores (Vram). LZ77 back-references read from the destination
as they are written; in Vram mode the last byte is still buffered, so `disp=0`
back-references are unsupported, exactly as the hardware documents.

`BgAffineSet` and `ObjAffineSet` build the affine matrix
(PA=sx·cos, PB=−sx·sin, PC=sy·sin, PD=sy·cos) from a 256-step Q14 sine table
(pure `sin(2π·i/256)` constants; cos is read 64 entries ahead). Only the angle's
upper 8 bits select an entry, as the BIOS documents. `BgAffineSet` also derives
the start coordinates. Like `ArcTan`, these are independent approximations — the
real BIOS's own sine table is disassembly-only — so they are not bit-identical
to it (its address is loaded reloc-free via a same-section label-difference
literal, since a 512-byte table is out of `adr` range).

`SoftReset` reads the return flag at `0x03007FFA` (0 -> ROM `0x08000000`,
non-zero -> RAM `0x02000000`), clears the `0x200`-byte BIOS RAM area, re-inits
the privileged stacks, zeroes r0-r12 and the exception banks, enters System
mode, and jumps to the target (it never returns). `RegisterRamReset` clears the
memory areas its flags select (on-board/on-chip WRAM, palette, VRAM, OAM),
resets the SIO/sound/other I/O register blocks, and always forces the screen
blank (`DISPCNT = 0x0080`). The on-chip WRAM clear preserves the last `0x200`
bytes, as documented.

## Not yet implemented

The sound-driver SWIs (`SoundBias`, `SoundDriver*`, `MidiKey2Freq`, `MultiBoot`),
`GetBiosChecksum`, and the boot logo intro. See the crate memory / project notes
for the roadmap.

## Tests

`crates/gba/tests/bios_golden.rs` checks the documented contract (boot hand-off,
Div/DivArm/Halt, IRQ forwarding) and, when a real image is provided, compares
observable results against it:

```sh
REWINDS_BIOS=/path/to/real_gba_bios.bin \
REWINDS_ROM=/path/to/game.gba \
  cargo test -p gba --test bios_golden
```
