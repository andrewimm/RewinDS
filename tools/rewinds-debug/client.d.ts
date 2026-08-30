// Type declarations for the RewinDS debug client.

export interface Registers {
  r: number[];
  pc: number;
  cpsr: { thumb: boolean; irqDisabled: boolean; fiqDisabled: boolean; mode: string };
}
export interface AddrCount { addr: number; count: number; region: string }
export interface Disasm { thumb: boolean; lines: { addr: number; text: string }[] }
export interface Background { index: number; enabled: boolean; priority: number; kind: string }
export interface VideoState { mode: number; dispcnt: number; frame: number; backgrounds: Background[] }
export interface PixelExplanation {
  x: number; y: number; finalColor: number; videoMode: number; topLayer: string;
  candidates: { layer: string; color: number; visible: boolean; addresses: number[] }[];
}
export interface RunState { frame: number; pc: number; region: string; dispcnt: number }
export interface WriteHit { hit: boolean; writerPc: number | null; steps: number; value: number }

export class Debugger {
  connect(port?: number, host?: string): Promise<this>;
  call(method: string, params?: object): Promise<any>;
  close(): void;

  runFrames(n?: number): Promise<RunState>;
  toFrame(frame: number): Promise<RunState>;
  steps(n?: number): Promise<RunState>;
  untilPc(pc: number, maxSteps?: number): Promise<{ hit: boolean; steps: number; pc: number }>;
  untilWrite(addr: number, maxSteps?: number): Promise<WriteHit>;

  registers(): Promise<Registers>;
  setReg(index: number, value: number): Promise<any>;

  readU32(addr: number): Promise<number>;
  readU16(addr: number): Promise<number>;
  read(addr: number, len: number): Promise<Buffer>;
  writeU32(addr: number, value: number): Promise<any>;
  writeU8(addr: number, value: number): Promise<any>;
  watch(kind?: 'reads' | 'writes', enable?: boolean): Promise<any>;
  watchReport(kind?: 'reads' | 'writes', top?: number): Promise<AddrCount[]>;

  hottestPc(steps?: number, top?: number): Promise<AddrCount[]>;
  disassemble(addr: number, count?: number, thumb?: boolean): Promise<Disasm>;

  videoState(): Promise<VideoState>;
  explainPixel(x: number, y: number): Promise<PixelExplanation>;
  spriteAt(x: number, y: number): Promise<any>;
  framebuffer(): Promise<{ width: number; height: number; rgba: string }>;
  screenshot(path: string): Promise<string>;

  pendingEvents(): Promise<{ now: number; events: { at: number; kind: string }[] }>;
  interrupts(): Promise<{ ie: number; if: number; ime: boolean; pending: boolean }>;
  press(key: string): Promise<any>;
  release(key: string): Promise<any>;
}
