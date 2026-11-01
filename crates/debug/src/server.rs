//! A TCP debug server speaking newline-delimited JSON, over the console-agnostic
//! [`Emulator`] facade.
//!
//! The emulator runs single-threaded on the main thread (it owns the [`Emulator`]); a
//! listener thread accepts a client and forwards each request line to the main loop
//! over a channel, so every request is serviced against a consistent state.
//!
//! Requests are `{"id":N,"method":"ns.method","params":{...}}`; responses are
//! `{"id":N,"ok":true,"result":...}` or `{"id":N,"ok":false,"error":"..."}`.
//!
//! Inspection methods (`run`, `cpu`, `memory`, `video`, `scheduler`, `interrupts`,
//! `input`) work on both the GBA and the DS. On the DS a `params.engine` (0 = A/main,
//! 1 = B/sub) selects the 2D engine and `params.core` (0 = ARM9, 1 = ARM7) the CPU/bus;
//! the GBA ignores them. A handful of advanced GBA-only methods (memory/execution
//! watches, single-step breakpoints, disassembly, audio) return an error on the DS.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

use emulator::rewind::Timeline;
use emulator::Emulator;
use gba::{Access, System};
use serde_json::{json, Value};

use crate::Debugger;

/// One pending request line and the channel its response goes back on.
type Request = (String, mpsc::Sender<String>);

/// Bind the port and spawn the listener thread, returning the channel the main loop
/// drains. Shared by [`serve`] (blocking/headless) and [`attach`] (polled alongside a
/// window loop) — the socket protocol and request handling are identical; only how the
/// emulator thread consumes requests differs.
fn listen(port: u16) -> std::io::Result<mpsc::Receiver<Request>> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let (tx, rx) = mpsc::channel::<Request>();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            eprintln!("[debug] client connected");
            let reader = BufReader::new(match stream.try_clone() {
                Ok(s) => s,
                Err(_) => continue,
            });
            let mut writer = stream;
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                let (rtx, rrx) = mpsc::channel();
                if tx.send((line, rtx)).is_err() {
                    return;
                }
                let resp = rrx
                    .recv()
                    .unwrap_or_else(|_| r#"{"ok":false,"error":"server stopped"}"#.to_string());
                if writeln!(writer, "{resp}").is_err() || writer.flush().is_err() {
                    break;
                }
            }
            eprintln!("[debug] client disconnected");
        }
    });
    Ok(rx)
}

/// Run the debug server on `port`, owning `emu`. Blocks forever (the caller's thread
/// becomes the emulator/request loop) — the headless mode used for automated testing,
/// where the emulator advances only on `run.*` commands.
pub fn serve(mut emu: Emulator, port: u16) -> std::io::Result<()> {
    let rx = listen(port)?;
    eprintln!("[debug] listening on 127.0.0.1:{port} ({:?})", emu.console());
    let mut timeline: Option<Timeline> = None;
    while let Ok((line, rtx)) = rx.recv() {
        let _ = rtx.send(handle(&mut emu, &mut timeline, &line));
    }
    Ok(())
}

/// A debug server attached to a free-running host (e.g. the desktop window loop): the
/// emulator runs in real time on the host thread, and [`Attached::poll`] services any
/// debug requests between frames. Lets a user play interactively while a debug client
/// inspects live — e.g. attaching to a hang the moment it happens.
pub struct Attached {
    rx: mpsc::Receiver<Request>,
    /// The rewind recorder, present only while rewind is enabled (via `rewind.enable`), so a
    /// live session carries no overhead until the user turns it on.
    timeline: Option<Timeline>,
}

/// Bind `port` for an attached session (see [`Attached`]). Does not block.
pub fn attach(port: u16) -> std::io::Result<Attached> {
    let rx = listen(port)?;
    eprintln!("[debug] attached debug server on 127.0.0.1:{port} (play as normal; connect to inspect)");
    Ok(Attached { rx, timeline: None })
}

impl Attached {
    /// Handle every debug request received since the last call, without blocking. Call once
    /// per frame from the host loop (single-threaded with the emulator).
    pub fn poll(&mut self, emu: &mut Emulator) {
        while let Ok((line, rtx)) = self.rx.try_recv() {
            let _ = rtx.send(handle(emu, &mut self.timeline, &line));
        }
    }

    /// Record the frame the host just ran into the rewind timeline (no-op unless rewind is
    /// enabled). Call once after each *live* `run_frame` — not while scrubbing, where no new
    /// frame is produced.
    pub fn record_frame(&mut self, emu: &Emulator) {
        if let Some(tl) = self.timeline.as_mut() {
            tl.record(emu);
        }
    }

    /// Turn on rewind recording with the given keyframe interval and ring capacity, so the
    /// host can offer an in-window scrubber. No-op if already enabled.
    pub fn enable_rewind(&mut self, keyframe_interval: u32, capacity: usize) {
        if self.timeline.is_none() {
            self.timeline = Some(Timeline::new(keyframe_interval, capacity));
        }
    }

    /// The rewind timeline, if enabled — for the host to drive a scrubber (`seek`/`step_back`)
    /// and read the recorded range.
    pub fn rewind(&mut self) -> Option<&mut Timeline> {
        self.timeline.as_mut()
    }
}

fn handle(emu: &mut Emulator, timeline: &mut Option<Timeline>, line: &str) -> String {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "error": format!("bad json: {e}")}).to_string(),
    };
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    match dispatch(emu, timeline, method, &params) {
        Ok(result) => json!({"id": id, "ok": true, "result": result}).to_string(),
        Err(e) => json!({"id": id, "ok": false, "error": e}).to_string(),
    }
}

// --- param helpers ---

fn u32p(params: &Value, key: &str) -> u32 {
    params.get(key).and_then(Value::as_u64).unwrap_or(0) as u32
}
fn u64p(params: &Value, key: &str, default: u64) -> u64 {
    params.get(key).and_then(Value::as_u64).unwrap_or(default)
}
fn u32p_or(params: &Value, key: &str, default: u32) -> u32 {
    params.get(key).and_then(Value::as_u64).map(|v| v as u32).unwrap_or(default)
}
/// The DS 2D-engine selector (0 = A/main, 1 = B/sub); 0 on the GBA.
fn enginep(params: &Value) -> usize {
    params.get("engine").and_then(Value::as_u64).unwrap_or(0) as usize
}
/// The DS core selector (0 = ARM9, 1 = ARM7); 0 on the GBA.
fn corep(params: &Value) -> usize {
    params.get("core").and_then(Value::as_u64).unwrap_or(0) as usize
}

/// Borrow the GBA system for a GBA-only method, or produce a uniform error on the DS.
fn gba_only(emu: &mut Emulator) -> Result<&mut System, String> {
    emu.as_gba_mut().ok_or_else(|| "method is GBA-only (not available for the DS)".to_string())
}

fn read32(system: &mut System, addr: u32) -> u32 {
    system.gba.bus.read32(addr, Access::cpu_data(), &mut system.scheduler).value
}

/// The memory region name for a GBA address, for readouts.
fn region(addr: u32) -> &'static str {
    match addr >> 24 {
        0x00 => "bios",
        0x02 => "ewram",
        0x03 => "iwram",
        0x04 => "io",
        0x05 => "palette",
        0x06 => "vram",
        0x07 => "oam",
        0x08..=0x0D => "rom",
        0x0E | 0x0F => "sram",
        _ => "open",
    }
}

/// Base64-encode bytes (standard alphabet), so binary payloads ride in JSON.
fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

fn addr_entries(watch: &std::collections::HashMap<u32, u64>, top: usize) -> Value {
    let mut v: Vec<_> = watch.iter().map(|(&a, &c)| (a, c)).collect();
    v.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
    v.truncate(top);
    json!(v
        .iter()
        .map(|&(addr, count)| json!({"addr": addr, "count": count, "region": region(addr)}))
        .collect::<Vec<_>>())
}

/// A compact "where are we" summary returned by run commands (console-agnostic).
fn state_summary(emu: &mut Emulator) -> Value {
    let core = 0;
    let mut dbg = Debugger::new(emu);
    let pc = dbg.cpu().pc(core);
    let mode = dbg.video().current_mode(0);
    let frame = dbg.emulator().frame();
    json!({ "console": format!("{:?}", dbg.emulator().console()), "frame": frame, "pc": pc, "videoMode": mode })
}

fn dispatch(
    emu: &mut Emulator,
    timeline: &mut Option<Timeline>,
    method: &str,
    params: &Value,
) -> Result<Value, String> {
    Ok(match method {
        // --- run control (console-agnostic) ---
        "run.frames" => {
            for _ in 0..u64p(params, "n", 1) {
                emu.run_frame();
                if let Some(tl) = timeline.as_mut() {
                    tl.record(emu);
                }
            }
            state_summary(emu)
        }
        "run.toFrame" => {
            let target = u64p(params, "frame", 0);
            let mut guard = 0;
            while emu.frame() < target && guard < 100_000 {
                emu.run_frame();
                if let Some(tl) = timeline.as_mut() {
                    tl.record(emu);
                }
                guard += 1;
            }
            state_summary(emu)
        }
        // --- rewind / time-travel debugging ---
        // Turn on recording: from here each frame logs its input, and a full snapshot is
        // kept every `keyframeInterval` frames (default 30), up to `capacity` keyframes
        // (default 60 → ~30 s of history). Both bound the memory cost; rewind is off (and
        // free) until this is called.
        "rewind.enable" => {
            let interval = u32p_or(params, "keyframeInterval", 30);
            let capacity = u64p(params, "capacity", 60) as usize;
            let mut tl = Timeline::new(interval, capacity);
            // Capture the current frame as the first keyframe so the past starts here.
            tl.record(emu);
            *timeline = Some(tl);
            json!({"enabled": true, "keyframeInterval": interval, "capacity": capacity})
        }
        "rewind.disable" => {
            *timeline = None;
            json!({"enabled": false})
        }
        // The recorded range and where the cursor sits (what the emulator currently shows).
        "rewind.timeline" => match timeline.as_ref() {
            Some(tl) => json!({
                "enabled": true,
                "oldest": tl.oldest_frame(),
                "newest": tl.newest_frame(),
                "cursor": tl.cursor(),
                "keyframes": tl.keyframe_frames(),
            }),
            None => json!({"enabled": false}),
        },
        // Reconstruct a past frame: restore the nearest keyframe and replay inputs to it.
        // Leaves the emulator at that frame for inspection.
        "rewind.seek" => {
            let tl = timeline.as_mut().ok_or("rewind is not enabled")?;
            let target = u64p(params, "frame", tl.cursor());
            let reached = tl.seek(emu, target);
            let mut out = state_summary(emu);
            out["cursor"] = json!(reached);
            out
        }
        // Step one frame into the past.
        "rewind.stepBack" => {
            let tl = timeline.as_mut().ok_or("rewind is not enabled")?;
            let reached = tl.step_back(emu);
            let mut out = state_summary(emu);
            out["cursor"] = json!(reached);
            out
        }
        // Timing benchmark: run N frames as fast as possible and report wall-clock
        // throughput — frames/sec and per-frame microseconds. Real-time is 59.83 fps.
        // Export the cartridge backup (save) bytes plus the write-epoch and dirty flag —
        // to verify a game's save was written completely (e.g. both HGSS mirror copies).
        "cart.saveData" => {
            let data = emu.save_data();
            json!({
                "len": data.len(),
                "writeEpoch": emu.save_write_epoch(),
                "dirty": emu.save_dirty(),
                "base64": base64(data),
            })
        }
        "bench.frames" => {
            let n = u64p(params, "n", 300).max(1);
            let start = std::time::Instant::now();
            for _ in 0..n {
                emu.run_frame();
            }
            let secs = start.elapsed().as_secs_f64();
            json!({
                "frames": n,
                "ms": (secs * 1000.0).round() / 1000.0,
                "fps": ((n as f64 / secs) * 100.0).round() / 100.0,
                "perFrameUs": ((secs / n as f64) * 1_000_000.0).round(),
                "realtimeRatio": ((n as f64 / secs / 59.83) * 100.0).round() / 100.0,
            })
        }

        // --- run control (single-step; `core` selects the DS CPU) ---
        "run.steps" => {
            let (n, core) = (u64p(params, "n", 1), corep(params));
            if let Some(system) = emu.as_gba_mut() {
                for _ in 0..n {
                    system.step();
                }
            } else if let Some(nds) = emu.as_nds_mut() {
                nds.debug_run(crate::nds_core(core), n, None, None, None);
            }
            state_summary(emu)
        }
        "run.untilPc" => {
            let (pc, max, core) = (u32p(params, "pc"), u64p(params, "maxSteps", 20_000_000), corep(params));
            if let Some(system) = emu.as_gba_mut() {
                let mut steps = 0;
                let mut hit = false;
                while steps < max {
                    if system.cpu.register(15) == pc {
                        hit = true;
                        break;
                    }
                    system.step();
                    steps += 1;
                }
                json!({"hit": hit, "steps": steps, "pc": system.cpu.register(15)})
            } else if let Some(nds) = emu.as_nds_mut() {
                let dc = crate::nds_core(core);
                let (steps, hit) = nds.debug_run(dc, max, Some(pc), None, None);
                json!({"hit": hit, "steps": steps, "pc": nds.core_pc(dc)})
            } else {
                Value::Null
            }
        }
        "run.untilWrite" => {
            let (addr, max, core) = (u32p(params, "addr"), u64p(params, "maxSteps", 20_000_000), corep(params));
            if let Some(system) = emu.as_gba_mut() {
                system.gba.bus.break_write_addr = Some(addr);
                system.gba.bus.write_hit = false;
                let mut steps = 0;
                let mut writer_pc = None;
                while steps < max {
                    let before = system.cpu.register(15);
                    system.step();
                    if system.gba.bus.write_hit {
                        writer_pc = Some(before);
                        break;
                    }
                    steps += 1;
                }
                system.gba.bus.break_write_addr = None;
                system.gba.bus.write_hit = false;
                json!({"hit": writer_pc.is_some(), "writerPc": writer_pc, "steps": steps, "value": read32(system, addr)})
            } else if let Some(nds) = emu.as_nds_mut() {
                let dc = crate::nds_core(core);
                let (steps, hit) = nds.debug_run(dc, max, None, Some(addr), None);
                // Attribute the write to the core that actually issued it (shared RAM is
                // written by the core NOT being stepped as often as by it), and report that
                // core's executing-instruction PC — not the stepped core's.
                let writer = nds.dbg_write_hit_core();
                json!({
                    "hit": hit,
                    "writerPc": writer.map(|_| nds.dbg_write_hit_pc()),
                    "writerCore": writer.map(|c| c.index()),
                    "value": if hit { nds.dbg_write_hit_val() } else { nds.read(dc, addr, 4) },
                    "steps": steps,
                })
            } else {
                Value::Null
            }
        }
        // Register-transition watchpoint (DS only): break when register `reg` becomes
        // `value`, via an instruction whose PC is in [pcLo, pcHi) (default: anywhere),
        // after `skip` earlier matches. Pins where a specific value originates.
        "run.untilReg" => {
            let core = corep(params);
            let nds = emu.as_nds_mut().ok_or_else(|| "run.untilReg is DS-only".to_string())?;
            let dc = crate::nds_core(core);
            let watch = nds::RegWatch {
                reg: u32p(params, "reg") as usize,
                value: u32p(params, "value"),
                pc_lo: u32p_or(params, "pcLo", 0),
                pc_hi: u32p_or(params, "pcHi", 0xFFFF_FFFF),
                skip: u32p(params, "skip"),
            };
            let max = u64p(params, "maxSteps", 40_000_000);
            let reg = watch.reg;
            let (steps, hit) = nds.debug_run(dc, max, None, None, Some(watch));
            json!({"hit": hit, "steps": steps, "pc": nds.core_pc(dc), "reg": reg, "value": nds.core_reg(dc, reg)})
        }

        // Emit a diffable execution trace to a file (DS only): fast-forward `skip`
        // instructions, then log `count` lines of "PC R0..R15 CPSR" to `path`.
        "run.trace" => {
            let core = corep(params);
            let nds = emu.as_nds_mut().ok_or_else(|| "run.trace is DS-only".to_string())?;
            let path = params.get("path").and_then(Value::as_str).ok_or("run.trace needs a path")?;
            let skip = u64p(params, "skip", 0);
            let count = u64p(params, "count", 1000);
            match nds.debug_trace(crate::nds_core(core), skip, count, path) {
                Ok(n) => json!({"wrote": n, "path": path}),
                Err(e) => return Err(format!("trace write failed: {e}")),
            }
        }

        // Emit a full-register trace (DS only) for value-level diffing: `PC R0..R15 CPSR`
        // per line. Finds *data* divergences (a register with the wrong value under a
        // matching PC), which a PC-only trace can't see.
        "run.traceRegs" => {
            let core = corep(params);
            let nds = emu.as_nds_mut().ok_or_else(|| "run.traceRegs is DS-only".to_string())?;
            let path = params.get("path").and_then(Value::as_str).ok_or("run.traceRegs needs a path")?;
            let skip = u64p(params, "skip", 0);
            let count = u64p(params, "count", 1000);
            match nds.debug_trace_regs(crate::nds_core(core), skip, count, path) {
                Ok(n) => json!({"wrote": n, "path": path}),
                Err(e) => return Err(format!("trace write failed: {e}")),
            }
        }
        // Annotated execution trace (DS only): one line per instruction, disassembled from
        // the bytes it actually executes (overlay- and per-core-view correct), with every
        // memory write it performs. Unlike traceRegs this shows the instruction text and its
        // stores — the tool for following a path through swappable VRAM-overlay code.
        "run.traceDisasm" => {
            let core = corep(params);
            let nds = emu.as_nds_mut().ok_or_else(|| "run.traceDisasm is DS-only".to_string())?;
            let path = params.get("path").and_then(Value::as_str).ok_or("run.traceDisasm needs a path")?;
            let skip = u64p(params, "skip", 0);
            let count = u64p(params, "count", 1000);
            match nds.debug_trace_disasm(crate::nds_core(core), skip, count, path) {
                Ok(n) => json!({"wrote": n, "path": path}),
                Err(e) => return Err(format!("trace write failed: {e}")),
            }
        }

        // --- cpu (console-agnostic; `core` selects the DS CPU) ---
        "cpu.registers" => {
            let core = corep(params);
            let mut dbg = Debugger::new(emu);
            let (r, cpsr) = dbg.cpu().registers(core);
            let mode = format!("{:?}", dbg.cpu().mode(core));
            json!({ "core": core, "r": r.to_vec(), "pc": r[15], "cpsr": cpsr, "mode": mode })
        }
        "cpu.setReg" => {
            let (index, value) = (u32p(params, "index") as usize, u32p(params, "value"));
            gba_only(emu)?.cpu.set_register(index, value);
            json!({"ok": true})
        }

        // --- memory (reads console-agnostic; writes/watches GBA-only) ---
        "memory.read" => {
            let (addr, len, core) = (u32p(params, "addr"), u64p(params, "len", 16) as u32, corep(params));
            let mut dbg = Debugger::new(emu);
            let bytes: Vec<u8> = (0..len).map(|i| dbg.memory().read8(core, addr + i)).collect();
            json!({"addr": addr, "len": len, "base64": base64(&bytes)})
        }
        "memory.readU32" => json!({"value": Debugger::new(emu).memory().read32(corep(params), u32p(params, "addr"))}),
        "memory.readU16" => json!({"value": Debugger::new(emu).memory().read16(corep(params), u32p(params, "addr"))}),
        "memory.writeU32" => {
            let (addr, value, core) = (u32p(params, "addr"), u32p(params, "value"), corep(params));
            Debugger::new(emu).memory().write32(core, addr, value);
            json!({"ok": true})
        }
        "memory.writeU16" => {
            let (addr, value, core) = (u32p(params, "addr"), u32p(params, "value") as u16, corep(params));
            Debugger::new(emu).memory().write16(core, addr, value);
            json!({"ok": true})
        }
        "memory.writeU8" => {
            let (addr, value, core) = (u32p(params, "addr"), u32p(params, "value") as u8, corep(params));
            Debugger::new(emu).memory().write8(core, addr, value);
            json!({"ok": true})
        }
        "memory.watch" => {
            let enable = params.get("enable").and_then(Value::as_bool).unwrap_or(true);
            let kind = params.get("kind").and_then(Value::as_str).unwrap_or("writes").to_string();
            if let Some(system) = emu.as_gba_mut() {
                match kind.as_str() {
                    "reads" => system.gba.bus.read_watch = enable.then(Default::default),
                    _ => system.gba.bus.write_watch = enable.then(Default::default),
                }
            } else if let Some(nds) = emu.as_nds_mut() {
                // The DS watch tracks writes only.
                nds.set_write_watch(enable);
            }
            json!({"ok": true})
        }
        "memory.watchReport" => {
            let top = u64p(params, "top", 24) as usize;
            let kind = params.get("kind").and_then(Value::as_str).unwrap_or("writes").to_string();
            if let Some(system) = emu.as_gba() {
                let watch = if kind == "reads" { &system.gba.bus.read_watch } else { &system.gba.bus.write_watch };
                watch.as_ref().map(|w| addr_entries(w, top)).unwrap_or(Value::Null)
            } else if let Some(nds) = emu.as_nds() {
                json!(nds
                    .write_watch_report(top)
                    .iter()
                    .map(|&(addr, count)| json!({"addr": addr, "count": count}))
                    .collect::<Vec<_>>())
            } else {
                Value::Null
            }
        }

        // --- execution profiling / disassembly (console-agnostic; `core` selects the DS core) ---
        "execution.hottestPc" => {
            let (steps, top, core) =
                (u64p(params, "steps", 20_000), u64p(params, "top", 12) as usize, corep(params));
            let mut hist: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
            if let Some(system) = emu.as_gba_mut() {
                for _ in 0..steps {
                    system.step();
                    *hist.entry(system.cpu.register(15)).or_insert(0) += 1;
                }
            } else if let Some(nds) = emu.as_nds_mut() {
                let dc = crate::nds_core(core);
                for _ in 0..steps {
                    nds.debug_run(dc, 1, None, None, None);
                    *hist.entry(nds.core_pc(dc)).or_insert(0) += 1;
                }
            }
            addr_entries(&hist, top)
        }
        "execution.disassemble" => {
            let (addr, count, core) =
                (u32p(params, "addr"), u64p(params, "count", 16) as u32, corep(params));
            let thumb_param = params.get("thumb").and_then(Value::as_bool);
            let mut dbg = Debugger::new(emu);
            // Thumb state: honor an explicit override, else read the core's CPSR T bit (5).
            let thumb = thumb_param.unwrap_or_else(|| dbg.cpu().registers(core).1 & (1 << 5) != 0);
            let step = if thumb { 2 } else { 4 };
            let lines: Vec<Value> = (0..count)
                .map(|i| {
                    let a = addr + i * step;
                    let text = if thumb {
                        arm::format_thumb(&arm::decode_thumb(dbg.memory().read16(core, a)))
                    } else {
                        arm::format_arm(&arm::decode_arm(dbg.memory().read32(core, a)))
                    };
                    json!({"addr": a, "text": text})
                })
                .collect();
            json!({"thumb": thumb, "lines": lines})
        }
        "execution.callStack" => {
            let top = u32p_or(params, "top", 0x0300_7f00);
            let system = gba_only(emu)?;
            let sp = system.cpu.register(13) & !3;
            let mut frames = Vec::new();
            let mut a = sp;
            while a < top && frames.len() < 48 {
                let v = read32(system, a);
                if (0x0800_0000..0x0a00_0000).contains(&v) && v & 1 == 1 {
                    frames.push(json!({"sp": a, "addr": v & !1}));
                }
                a += 4;
            }
            json!({"pc": system.cpu.register(15), "sp": sp, "frames": frames})
        }

        // --- video (console-agnostic; `engine` selects the DS 2D engine) ---
        "video.state" => {
            let engine = enginep(params);
            let mode = Debugger::new(emu).video().current_mode(engine);
            let mut out = json!({ "engine": engine, "mode": mode });
            // The DS reports each engine's DISPCNT + per-background configuration via its
            // register snapshot; the GBA carries an equivalent summary from its own io.
            if let Some(regs) = Debugger::new(emu).video().engine_registers(engine) {
                let dispcnt = Debugger::new(emu).video().nds_dispcnt(engine).unwrap_or(0);
                let display_mode = (dispcnt >> 16) & 3;
                let display_mode_name = ["off", "graphics", "vram", "mainmem"][display_mode as usize];
                let bgs: Vec<Value> = (0..4)
                    .map(|i| json!({
                        "index": i,
                        "enabled": dispcnt & (1 << (8 + i)) != 0,
                        "priority": regs.bgcnt[i] & 3,
                        "bgcnt": regs.bgcnt[i],
                    }))
                    .collect();
                let (mbright, powcnt) = Debugger::new(emu).video().nds_display_power(engine).unwrap_or((0, 0));
                let mbright_mode = (mbright >> 14) & 3; // 0/3 = none, 1 = to white, 2 = to black
                let mbright_factor = (mbright & 0x1F).min(16);
                let engine_powered = if engine == 0 { powcnt & (1 << 1) != 0 } else { powcnt & (1 << 9) != 0 };
                out = json!({"engine": engine, "mode": mode, "dispcnt": dispcnt,
                             "displayMode": display_mode, "displayModeName": display_mode_name,
                             "bg0_is_3d": engine == 0 && dispcnt & (1 << 3) != 0,
                             "masterBright": mbright, "masterBrightMode": mbright_mode,
                             "masterBrightFactor": mbright_factor,
                             "lcdsOn": powcnt & 1 != 0, "enginePowered": engine_powered,
                             "backgrounds": bgs});
            }
            if let Some(system) = emu.as_gba() {
                let v = &system.gba.bus.io.video;
                let bgs: Vec<Value> = v
                    .backgrounds()
                    .iter()
                    .enumerate()
                    .map(|(i, b)| json!({"index": i, "enabled": b.enabled, "priority": b.priority, "kind": format!("{:?}", b.kind)}))
                    .collect();
                let r = &v.registers;
                out = json!({"engine": engine, "mode": v.current_mode(), "dispcnt": v.read_dispcnt(), "frame": v.frame(),
                             "backgrounds": bgs, "win_h": r.win_h, "win_v": r.win_v, "winin": r.winin,
                             "winout": r.winout, "bldcnt": r.bldcnt, "mosaic": r.mosaic});
            }
            out
        }
        // The full RGBA8 present buffer for one physical screen (`screen`: 0 = top,
        // 1 = the DS bottom screen). Works for both consoles; the client stacks the
        // two DS screens itself. `base64` is width*height*4 bytes, row-major.
        "video.framebuffer" => {
            emu.present();
            let index = params.get("screen").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let s = emu
                .screen(index)
                .ok_or_else(|| format!("screen {index} out of range"))?;
            json!({
                "screen": index,
                "width": s.width,
                "height": s.height,
                "format": "rgba8",
                "base64": base64(s.rgba),
            })
        }
        // Per-background "dump all BGs" — pixel coverage per layer, and each layer
        // rendered in isolation as an RGBA8 image (DS only; `layer`: BG 0-3, OBJ = 4).
        "video.layerCoverage" => {
            let engine = enginep(params);
            let cov = Debugger::new(emu).video().layer_coverage(engine);
            json!({"engine": engine, "bg0": cov[0], "bg1": cov[1], "bg2": cov[2], "bg3": cov[3], "obj": cov[4]})
        }
        "video.renderLayer" => {
            let engine = enginep(params);
            let layer = params.get("layer").and_then(Value::as_u64).unwrap_or(0) as usize;
            let bgr = Debugger::new(emu).video().render_layer(engine, layer);
            // The DS engine framebuffer is 256 wide (video2d::WIDTH is the *GBA* width);
            // derive the height from the buffer so the row stride is correct.
            let width = video2d::MAX_WIDTH; // DS active width (nonzero constant)
            let height = bgr.len() / width;
            // BGR555 -> RGBA8 so the client can PNG it directly, like video.framebuffer.
            let mut rgba = Vec::with_capacity(bgr.len() * 4);
            for &v in &bgr {
                let c5to8 = |c: u16| ((c & 0x1F) as u32 * 255 / 31) as u8;
                rgba.extend_from_slice(&[c5to8(v), c5to8(v >> 5), c5to8(v >> 10), 255]);
            }
            json!({"engine": engine, "layer": layer, "width": width, "height": height,
                   "format": "rgba8", "base64": base64(&rgba)})
        }
        "video.explainPixel" => {
            let (engine, x, y) = (enginep(params), u32p(params, "x") as u16, u32p(params, "y") as u16);
            let ex = Debugger::new(emu)
                .video()
                .explain_pixel(engine, x, y)
                .map_err(|e| format!("{e:?}"))?;
            let candidates: Vec<Value> = ex
                .candidates
                .iter()
                .map(|c| json!({
                    "layer": format!("{:?}", c.candidate.layer),
                    "color": c.candidate.color.0,
                    "priority": c.candidate.priority,
                    "semiTransparentObj": c.candidate.flags.semi_transparent_obj,
                    "objAlpha": c.candidate.flags.obj_alpha,
                    "visible": c.visible_after_window,
                    "rejection": c.rejection_reason.as_ref().map(|r| format!("{r:?}")),
                    "addresses": c.provenance.source_addresses(),
                }))
                .collect();
            json!({
                "engine": engine, "x": ex.x, "y": ex.y,
                "finalColor": ex.final_color.0,
                "videoMode": ex.video_mode,
                "topLayer": format!("{:?}", ex.resolved.top.layer),
                "topPriority": ex.resolved.top.priority,
                // The second-front-most candidate: the alpha-blend operand behind the top.
                "secondLayer": format!("{:?}", ex.resolved.second.layer),
                "secondPriority": ex.resolved.second.priority,
                // Window decision at this pixel — `effectsEnabled` gates all color effects.
                "window": {
                    "region": format!("{:?}", ex.window.region),
                    "effectsEnabled": ex.window.effects_enabled,
                    "layersEnabled": ex.window.layers_enabled,
                },
                // The color-effects (blend/brighten/darken) stage — the "why" behind a
                // translucency result. `applied` names the operands and coefficients.
                "effect": {
                    "mode": format!("{:?}", ex.effect.mode),
                    "applied": format!("{:?}", ex.effect.applied),
                    "result": ex.effect.result.0,
                },
                "candidates": candidates,
            })
        }
        "video.scanline" => {
            let (engine, y) = (enginep(params), u32p(params, "y") as u16);
            let sc = Debugger::new(emu).video().scanline(engine, y);
            let sprites: Vec<Value> = sc
                .sprites
                .iter()
                .map(|s| json!({"oamIndex": s.oam_index, "x": s.x, "y": s.y, "tile": s.tile_number, "priority": s.priority}))
                .collect();
            json!({"engine": engine, "y": y, "sprites": sprites})
        }
        // DS 3D render-list shading summary: DISP3DCNT, the frame's toon-vs-highlight select,
        // and a per-polygon-mode count (mode 2 = toon/highlight — the cel-shaded polygons).
        "video.polySummary" => {
            let nds = emu.as_nds().ok_or_else(|| "video.polySummary is DS-only".to_string())?;
            let (disp3dcnt, polys) = nds.gpu3d_poly_summary();
            let mut by_mode = [0u32; 4];
            for &(_, _, mode, _) in &polys {
                by_mode[(mode & 3) as usize] += 1;
            }
            json!({
                "disp3dcnt": disp3dcnt,
                "shading": if disp3dcnt & 2 != 0 { "highlight" } else { "toon" },
                "polyCount": polys.len(),
                "byMode": {"modulate": by_mode[0], "decal": by_mode[1], "toonHighlight": by_mode[2], "shadow": by_mode[3]},
            })
        }
        "video.setBenchSkipRender" => {
            let nds = emu.as_nds_mut().ok_or_else(|| "DS-only".to_string())?;
            let skip = params.get("skip").and_then(Value::as_bool).unwrap_or(false);
            nds.gpu3d_set_bench_skip_render(skip);
            json!({"skip": skip})
        }
        "video.edgeMarking" => {
            let nds = emu.as_nds().ok_or_else(|| "video.edgeMarking is DS-only".to_string())?;
            let (enabled, table, changed) = nds.gpu3d_edge_marking();
            json!({
                "enabled": enabled,
                "edgeTable": table.iter().map(|&c| c as u32).collect::<Vec<_>>(),
                "changedPixels": changed,
            })
        }
        // The 3D framebuffer with edge-marked pixels forced to magenta (BGR555 → RGBA8),
        // so outlines are visible pre-composite. 256×192, like video.renderLayer.
        "video.edgeOverlay" => {
            let nds = emu.as_nds().ok_or_else(|| "video.edgeOverlay is DS-only".to_string())?;
            let bgr = nds.gpu3d_edge_overlay();
            let width = 256usize;
            let height = bgr.len() / width;
            let mut rgba = Vec::with_capacity(bgr.len() * 4);
            for &v in &bgr {
                let c5to8 = |c: u16| ((c & 0x1F) as u32 * 255 / 31) as u8;
                let a = if v == 0 { 0 } else { 255 };
                rgba.extend_from_slice(&[c5to8(v), c5to8(v >> 5), c5to8(v >> 10), a]);
            }
            json!({"width": width, "height": height, "format": "rgba8", "base64": base64(&rgba)})
        }

        // --- scheduler / interrupts / input (console-agnostic) ---
        "scheduler.pendingEvents" => {
            let mut dbg = Debugger::new(emu);
            let sched = dbg.scheduler();
            let events: Vec<Value> = sched
                .pending_events()
                .into_iter()
                .map(|(at, kind)| json!({"at": at, "kind": kind}))
                .collect();
            json!({"now": sched.now(), "count": sched.pending_count(), "events": events})
        }
        "interrupts.state" => {
            let core = corep(params);
            let mut dbg = Debugger::new(emu);
            let i = dbg.interrupts();
            json!({"core": core, "ie": i.enabled(core), "if": i.flags(core), "ime": i.master_enable(core),
                   "pending": i.pending(core), "lineAsserted": i.line_asserted(core)})
        }
        "input.set" => {
            let name = params.get("key").and_then(Value::as_str).unwrap_or("").to_string();
            let pressed = params.get("pressed").and_then(Value::as_bool).unwrap_or(true);
            if Debugger::new(emu).input().set_name(&name, pressed) {
                json!({"ok": true})
            } else {
                return Err(format!("unknown key: {name}"));
            }
        }
        // DS touchscreen: `{x, y}` (lower-screen pixels) presses the pen; omit or
        // `pressed:false` lifts it.
        "input.touch" => {
            let pressed = params.get("pressed").and_then(Value::as_bool).unwrap_or(true);
            let pos = pressed.then(|| {
                let x = params.get("x").and_then(Value::as_i64).unwrap_or(0) as i32;
                let y = params.get("y").and_then(Value::as_i64).unwrap_or(0) as i32;
                (x, y)
            });
            if Debugger::new(emu).input().set_touch(pos) {
                json!({"ok": true})
            } else {
                return Err("touch is DS-only".to_string());
            }
        }
        // DS clamshell: `{closed: true}` shuts the lid, `{closed: false}` (or omitted)
        // opens it; a change raises the hinge IRQ (games sleep closed, wake on open).
        "input.lid" => {
            let closed = params.get("closed").and_then(Value::as_bool).unwrap_or(false);
            if Debugger::new(emu).input().set_lid(closed) {
                json!({"ok": true})
            } else {
                return Err("lid is DS-only".to_string());
            }
        }
        // Recent inter-core IPC traffic (DS): the ARM9<->ARM7 conversation, to
        // see which core is waiting on what. kind 0 = FIFO send, 1 = SYNC write.
        "ipc.recent" => {
            let nds = emu
                .as_nds()
                .ok_or_else(|| "ipc.recent is DS-only".to_string())?;
            let events: Vec<Value> = nds
                .ipc_recent()
                .into_iter()
                .map(|(core, kind, value, pc)| {
                    json!({
                        "core": if core == 0 { "arm9" } else { "arm7" },
                        "kind": if kind == 0 { "send" } else { "sync" },
                        "value": value,
                        "pc": pc,
                    })
                })
                .collect();
            json!({ "events": events })
        }
        // Per-core master-cycle clocks + scheduler now, for measuring the ARM9/ARM7
        // relative execution rate (a timing-imbalance check).
        "sys.clocks" => {
            let nds = emu
                .as_nds()
                .ok_or_else(|| "sys.clocks is DS-only".to_string())?;
            json!({
                "arm9": nds.clock(nds::Core::Arm9),
                "arm7": nds.clock(nds::Core::Arm7),
                "now": nds.now(),
            })
        }
        // Take-and-clear the IPC log, for streaming the full ordered history by
        // polling repeatedly (the ring in `ipc.recent` only keeps recent events).
        "ipc.drain" => {
            let nds = emu
                .as_nds_mut()
                .ok_or_else(|| "ipc.drain is DS-only".to_string())?;
            let events: Vec<Value> = nds
                .ipc_drain()
                .into_iter()
                .map(|(core, kind, value, pc)| {
                    json!({
                        "core": if core == 0 { "arm9" } else { "arm7" },
                        "kind": if kind == 0 { "send" } else { "sync" },
                        "value": value,
                        "pc": pc,
                    })
                })
                .collect();
            json!({ "events": events })
        }
        // Launch the inserted cartridge after a firmware boot has reached the menu
        // (run enough frames first). Deterministic, menu-free counterpart to
        // selecting the cart in the firmware menu.
        "system.launchCart" => {
            let nds = emu
                .as_nds_mut()
                .ok_or_else(|| "system.launchCart is DS-only".to_string())?;
            nds.launch_cart_from_firmware()
                .map_err(|e| format!("{e:?}"))?;
            json!({"ok": true})
        }

        // --- GBA-only extras ---
        "audio.take" => {
            let system = gba_only(emu)?;
            let (clipped, raw_peak) = system.audio_clip_stats();
            let samples = system.take_audio();
            let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
            let nonzero = samples.iter().filter(|&&s| s != 0).count();
            json!({"count": samples.len(), "peak": peak, "nonzero": nonzero, "clipped": clipped, "rawPeak": raw_peak})
        }

        other => return Err(format!("unknown method: {other}")),
    })
}
