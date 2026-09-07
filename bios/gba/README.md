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
  jump-table framework. Implemented: `Halt` (0x02), `IntrWait` (0x04),
  `VBlankIntrWait` (0x05), `Div` (0x06), and `DivArm` (0x07). Every other SWI
  currently returns as a no-op.
- **IRQ**: the documented BIOS interrupt entry that saves context, forwards to
  the user handler at `[0x03007FFC]`, and returns via `subs pc, lr, #4`.

`IntrWait`/`VBlankIntrWait` force interrupts on (IME=1 and the CPSR I-bit
cleared) and halt-poll the BIOS Interrupt Check Flags at `0x03007FF8`, which the
user IRQ handler is responsible for posting to; the awaited bits are cleared
before returning. The caller's CPSR (I-bit included) is restored by the normal
SWI return.

## Not yet implemented

`SoftReset`/`RegisterRamReset`, the remaining arithmetic (`Sqrt`, `ArcTan*`),
memory (`CpuSet`, `CpuFastSet`), decompression, and affine SWIs, plus the boot
logo intro. See the crate memory / project notes for the roadmap.

## Tests

`crates/gba/tests/bios_golden.rs` checks the documented contract (boot hand-off,
Div/DivArm/Halt, IRQ forwarding) and, when a real image is provided, compares
observable results against it:

```sh
REWINDS_BIOS=/path/to/real_gba_bios.bin \
REWINDS_ROM=/path/to/game.gba \
  cargo test -p gba --test bios_golden
```
