#!/usr/bin/env bash
# Build the from-scratch GBA BIOS replacement into gba_bios.bin.
#
# Uses only Apple clang (the same assembler the arm crate's fixtures use) plus
# rustc for a tiny .text extractor — no cross-linker or objcopy required. See
# bios.s for the position-independence constraints that make this possible.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"

# rustup installs rustc under ~/.cargo/bin, which non-interactive shells may not
# have on PATH.
command -v rustc >/dev/null 2>&1 || export PATH="$HOME/.cargo/bin:$PATH"

clang --target=armv4t-none-eabi -c "$here/bios.s" -o "$here/bios.o"
rustc -O "$here/extract_text.rs" -o "$here/extract_text"
"$here/extract_text" "$here/bios.o" "$here/gba_bios.bin"

echo "wrote $here/gba_bios.bin ($(wc -c < "$here/gba_bios.bin") bytes)"
