# RewinDS Emulator

A GBA + NDS emulator focused on ease of debugging, including time travel debugging, a rich debug api, and the ability to query not just what's on a screen but why. Internally, it also has a few unique features, including a JIT powered by a device-aware IR that combines memory bus and execution, and compute shaders that power the graphics pipeline.

# Design Aid

The NDS and GBA have complex architecture. The `docs/` folder contains supporting documents that can help understand the hardware we are replicating.

The user will provide clear design details for each phase of the project, which may include specific struct or module shapes. Ask for clarification or approval before designing larger chunks of the project.

# Core Crates

`arm` - decoding and interpreting arm and thumb instructions
 - No GBA knowledge
 - No DS knowledge
 - No scheduler knowledge

`emu-core` - memory map, bus, scheduler, and debugger api
 - No GBA/DS knowledge

`gba` - Owns GBA hardware semantics

`nds` - Owns DS hardware semantics

`video2d` - Shared rendering mechanics between the GBA and NDS 2d renderers

`jit` - Consumes ARM semantics + machine-aware IR metadata to produce JIT execution

`emulator` - Console-agnostic facade (`enum Emulator { Gba, Nds }`) presenting one uniform, FFI-ready surface: load, run a frame, feed input, read screens, pull audio, exchange save bytes. Sits below `debug` and the frontends; owns the host-facing glue (RGBA present buffers, the audio resample+ring, ROM detection). Designed so a thin `emulator-ffi` crate (`extern "C"` + cbindgen → XCFramework) can later carry it into native macOS/iOS Swift apps — the C boundary is treated as the host boundary (bytes/pointers cross it; the host owns files, persistence, and windowing).

`debug` - Structured interface over emulator state. `emu-core::debug` owns low-level hooks and events, `debug` owns the richer semantic API that an agent or debugger can use. Currently GBA-specific; reaches the concrete machine through the `emulator` facade's escape hatch until an NDS machine exists to generalize it.

`rewinds` - Desktop reference host (minifb window + cpal audio) that drives the `emulator` facade — the same surface the future Swift apps will drive.
