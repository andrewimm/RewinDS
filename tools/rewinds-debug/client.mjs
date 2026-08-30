// RewinDS debug client: connect to the emulator's debug socket (TCP + JSON-lines)
// and drive/inspect the system programmatically.
//
//   import { Debugger } from './client.mjs';
//   const dbg = new Debugger();
//   await dbg.connect(9000);
//   await dbg.toFrame(320);
//   console.log(await dbg.registers());
//   console.log(await dbg.hottestPc(30000, 8));
//   await dbg.screenshot('screen.png');   // dumps the framebuffer to a PNG
//   dbg.close();
//
// No dependencies — PNG encoding uses Node's built-in zlib.

import net from 'node:net';
import zlib from 'node:zlib';
import { writeFileSync } from 'node:fs';

export class Debugger {
  constructor() {
    this.sock = null;
    this.nextId = 0;
    this.pending = new Map();
    this.buf = '';
  }

  connect(port = 9000, host = '127.0.0.1') {
    return new Promise((resolve, reject) => {
      this.sock = net.createConnection({ port, host }, () => resolve(this));
      this.sock.setEncoding('utf8');
      this.sock.on('error', reject);
      this.sock.on('data', (chunk) => {
        this.buf += chunk;
        let i;
        while ((i = this.buf.indexOf('\n')) >= 0) {
          const line = this.buf.slice(0, i);
          this.buf = this.buf.slice(i + 1);
          if (!line.trim()) continue;
          const msg = JSON.parse(line);
          const p = this.pending.get(msg.id);
          if (!p) continue;
          this.pending.delete(msg.id);
          msg.ok ? p.resolve(msg.result) : p.reject(new Error(msg.error));
        }
      });
    });
  }

  call(method, params = {}) {
    const id = ++this.nextId;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.sock.write(JSON.stringify({ id, method, params }) + '\n');
    });
  }

  close() {
    this.sock?.end();
  }

  // --- run control ---
  runFrames(n = 1) { return this.call('run.frames', { n }); }
  toFrame(frame) { return this.call('run.toFrame', { frame }); }
  steps(n = 1) { return this.call('run.steps', { n }); }
  untilPc(pc, maxSteps) { return this.call('run.untilPc', { pc, maxSteps }); }
  untilWrite(addr, maxSteps) { return this.call('run.untilWrite', { addr, maxSteps }); }

  // --- cpu ---
  registers() { return this.call('cpu.registers'); }
  setReg(index, value) { return this.call('cpu.setReg', { index, value }); }

  // --- memory ---
  async readU32(addr) { return (await this.call('memory.readU32', { addr })).value; }
  async readU16(addr) { return (await this.call('memory.readU16', { addr })).value; }
  async read(addr, len) {
    const r = await this.call('memory.read', { addr, len });
    return Buffer.from(r.base64, 'base64');
  }
  writeU32(addr, value) { return this.call('memory.writeU32', { addr, value }); }
  writeU8(addr, value) { return this.call('memory.writeU8', { addr, value }); }
  watch(kind = 'writes', enable = true) { return this.call('memory.watch', { kind, enable }); }
  watchReport(kind = 'writes', top = 24) { return this.call('memory.watchReport', { kind, top }); }

  // --- execution ---
  hottestPc(steps = 20000, top = 12) { return this.call('execution.hottestPc', { steps, top }); }
  disassemble(addr, count = 16, thumb) { return this.call('execution.disassemble', { addr, count, thumb }); }

  // --- video ---
  videoState() { return this.call('video.state'); }
  explainPixel(x, y) { return this.call('video.explainPixel', { x, y }); }
  spriteAt(x, y) { return this.call('video.spriteAt', { x, y }); }
  framebuffer() { return this.call('video.framebuffer'); }
  async screenshot(path) {
    const { width, height, rgba } = await this.framebuffer();
    writeFileSync(path, encodePng(width, height, Buffer.from(rgba, 'base64')));
    return path;
  }

  // --- scheduler / interrupts / input ---
  pendingEvents() { return this.call('scheduler.pendingEvents'); }
  interrupts() { return this.call('interrupts.state'); }
  press(key) { return this.call('input.set', { key, pressed: true }); }
  release(key) { return this.call('input.set', { key, pressed: false }); }
}

// --- minimal PNG encoder (RGBA8, no interlace) using only Node builtins ---

const CRC_TABLE = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();

function crc32(buf) {
  let c = 0xffffffff;
  for (let i = 0; i < buf.length; i++) c = CRC_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const typeBuf = Buffer.from(type, 'ascii');
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(Buffer.concat([typeBuf, data])), 0);
  return Buffer.concat([len, typeBuf, data, crc]);
}

export function encodePng(width, height, rgba) {
  const stride = width * 4;
  const raw = Buffer.alloc((stride + 1) * height);
  for (let y = 0; y < height; y++) {
    raw[y * (stride + 1)] = 0; // filter type 0 (none)
    rgba.copy(raw, y * (stride + 1) + 1, y * stride, y * stride + stride);
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // color type: RGBA
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', zlib.deflateSync(raw)),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}
