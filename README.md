# RewinDS

A Game Boy Advance + Nintendo DS emulator written in Rust, built around one question:
not just *what's* on the screen, but *why*. It's an ordinary emulator you can play games
on — and a debugging instrument you can single-step, rewind, and interrogate deeply.

It runs as a desktop app and as a native iOS app, both driving the same emulator core.

## What's different

Most of the design budget goes into **debuggability**:

- A structured **debug API** (`debug` crate) over live machine state, exposed as a
  headless **JSON-RPC server** you can script: jump to a frame, dump every background
  layer, ask *why* a given pixel is the color it is, watch memory, trace the CPU. This
  is deliberately intended to be driven by LLMs and Agents. When you're building your
  next GBA game, you'll want to give your Agent complete debugging power.
- A console-agnostic **facade** (`emulator`) that presents one uniform surface — load,
  run a frame, feed input, read screens, pull audio, exchange saves — so hosts (desktop,
  iOS, tooling) never care which console they're holding.
- Longer-term internals: a JIT powered by a device-aware IR that fuses the memory bus
  with execution, and a compute-shader graphics pipeline. (The shipping renderer today
  is a software rasterizer.)

Both **GBA and DS boot and render commercial games**

## Workspace

```
crates/
  arm           ARM/Thumb decode + interpreter (no console knowledge)
  emu-core      memory map, bus, scheduler, debugger hooks
  gba           GBA hardware
  nds           Nintendo DS hardware
  video2d       shared 2D renderer for GBA + DS
  gpu3d         DS 3D engine (fixed-point geometry + scanline rasterizer)
  emulator      console-agnostic facade (enum Emulator { Gba, Nds })
  emulator-ffi  extern-C boundary over the facade (static lib for native hosts)
  debug         structured debug API + JSON-RPC server
  rewinds       desktop reference host (minifb window + cpal audio)
```

## Running it (desktop)

BIOS images are **not** included (they're copyrighted); supply your own.

```sh
# GBA — requires a BIOS
cargo run --release -p rewinds -- game.gba --bios gba_bios.bin

# DS — direct-boots with the ARM9 + ARM7 BIOS (firmware optional; --firmware boots the
# real firmware menu instead of direct boot)
cargo run --release -p rewinds -- game.nds --bios biosnds9.bin --bios7 biosnds7.bin
```

Keyboard: arrow keys, **X**/**Z** = A/B, **A**/**S** = L/R, **Enter**/**Backspace** =
Start/Select, hold **Space** to fast-forward, hold **L** to shut the DS lid, **Esc** quits.

## iOS app

A SwiftUI app (landscape GBA / portrait DS, Metal screens, multitouch controls) that
links the Rust core through `emulator-ffi`. Build and run instructions live in
[`rewinds-ios/README.md`](rewinds-ios/README.md).

## Debugging

Start the emulator headless with a debug port, then drive it over JSON-RPC:

```sh
cargo run --release -p rewinds -- game.nds --bios biosnds9.bin --bios7 biosnds7.bin --debug-port 9000
```

A Node client lives in `tools/rewinds-debug/` (`client.mjs`) — run to a frame, pull a
framebuffer, render a single BG layer in isolation, explain a pixel, and so on.

## Status

Experimental and moving fast. Interfaces (including the FFI and debug protocol) are not
yet stable.
