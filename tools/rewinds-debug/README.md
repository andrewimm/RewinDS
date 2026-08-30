# rewinds-debug

A small Node client for the RewinDS emulator debug socket. Connect to a running
emulator over TCP and drive/inspect the system programmatically — run frames, set
breakpoints, read/write memory, histogram PCs, disassemble, and dump the screen to
a PNG. No dependencies (PNG encoding uses Node's built-in `zlib`).

## Usage

Start the emulator in headless debug mode:

```sh
cargo run -p rewinds -- gba_bios.bin game.gba --debug-port 9000
```

Then, from Node:

```js
import { Debugger } from 'rewinds-debug'; // or './client.mjs'

const dbg = await new Debugger().connect(9000);

await dbg.toFrame(300);
console.log(await dbg.registers());          // { r, pc, cpsr }
console.log(await dbg.videoState());         // mode, dispcnt, backgrounds
console.log(await dbg.hottestPc(30000, 8));  // where time is spent

// Find who writes a memory location, then disassemble the writer.
const w = await dbg.untilWrite(0x0300310c);
if (w.hit) console.log((await dbg.disassemble(w.writerPc - 8, 10, true)).lines);

await dbg.screenshot('screen.png');          // dump the framebuffer to PNG
dbg.close();
```

See `client.d.ts` for the full typed API. One-off exploration scripts live in the
git-ignored `scratch/` directory.
