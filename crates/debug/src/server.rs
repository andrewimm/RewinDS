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

use emulator::Emulator;
use gba::{Access, System};
use serde_json::{json, Value};

use crate::Debugger;

/// Run the debug server on `port`, owning `emu`. Blocks forever (the caller's thread
/// becomes the emulator/request loop).
pub fn serve(mut emu: Emulator, port: u16) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    eprintln!("[debug] listening on 127.0.0.1:{port} ({:?})", emu.console());

    let (tx, rx) = mpsc::channel::<(String, mpsc::Sender<String>)>();
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

    while let Ok((line, rtx)) = rx.recv() {
        let _ = rtx.send(handle(&mut emu, &line));
    }
    Ok(())
}

fn handle(emu: &mut Emulator, line: &str) -> String {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "error": format!("bad json: {e}")}).to_string(),
    };
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    match dispatch(emu, method, &params) {
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

fn read16(system: &mut System, addr: u32) -> u16 {
    system.gba.bus.read16(addr, Access::cpu_data(), &mut system.scheduler).value
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

fn dispatch(emu: &mut Emulator, method: &str, params: &Value) -> Result<Value, String> {
    Ok(match method {
        // --- run control (console-agnostic) ---
        "run.frames" => {
            for _ in 0..u64p(params, "n", 1) {
                emu.run_frame();
            }
            state_summary(emu)
        }
        "run.toFrame" => {
            let target = u64p(params, "frame", 0);
            let mut guard = 0;
            while emu.frame() < target && guard < 100_000 {
                emu.run_frame();
                guard += 1;
            }
            state_summary(emu)
        }

        // --- run control (GBA-only: single-step machinery) ---
        "run.steps" => {
            let system = gba_only(emu)?;
            for _ in 0..u64p(params, "n", 1) {
                system.step();
            }
            state_summary(emu)
        }
        "run.untilPc" => {
            let (pc, max) = (u32p(params, "pc"), u64p(params, "maxSteps", 20_000_000));
            let system = gba_only(emu)?;
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
        }
        "run.untilWrite" => {
            let (addr, max) = (u32p(params, "addr"), u64p(params, "maxSteps", 20_000_000));
            let system = gba_only(emu)?;
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
            let (addr, value) = (u32p(params, "addr"), u32p(params, "value"));
            let system = gba_only(emu)?;
            system.gba.bus.write32(addr, value, Access::cpu_data(), &mut system.scheduler);
            json!({"ok": true})
        }
        "memory.writeU8" => {
            let (addr, value) = (u32p(params, "addr"), u32p(params, "value") as u8);
            let system = gba_only(emu)?;
            system.gba.bus.write8(addr, value, Access::cpu_data(), &mut system.scheduler);
            json!({"ok": true})
        }
        "memory.watch" => {
            let enable = params.get("enable").and_then(Value::as_bool).unwrap_or(true);
            let system = gba_only(emu)?;
            match params.get("kind").and_then(Value::as_str).unwrap_or("writes") {
                "reads" => system.gba.bus.read_watch = enable.then(Default::default),
                _ => system.gba.bus.write_watch = enable.then(Default::default),
            }
            json!({"ok": true})
        }
        "memory.watchReport" => {
            let top = u64p(params, "top", 24) as usize;
            let kind = params.get("kind").and_then(Value::as_str).unwrap_or("writes").to_string();
            let system = gba_only(emu)?;
            let watch = if kind == "reads" { &system.gba.bus.read_watch } else { &system.gba.bus.write_watch };
            watch.as_ref().map(|w| addr_entries(w, top)).unwrap_or(Value::Null)
        }

        // --- execution profiling / disassembly (GBA-only) ---
        "execution.hottestPc" => {
            let (steps, top) = (u64p(params, "steps", 20_000), u64p(params, "top", 12) as usize);
            let system = gba_only(emu)?;
            let mut hist: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
            for _ in 0..steps {
                system.step();
                *hist.entry(system.cpu.register(15)).or_insert(0) += 1;
            }
            addr_entries(&hist, top)
        }
        "execution.disassemble" => {
            let (addr, count) = (u32p(params, "addr"), u64p(params, "count", 16) as u32);
            let thumb_param = params.get("thumb").and_then(Value::as_bool);
            let system = gba_only(emu)?;
            let thumb = thumb_param.unwrap_or_else(|| system.cpu.cpsr().thumb());
            let step = if thumb { 2 } else { 4 };
            let lines: Vec<Value> = (0..count)
                .map(|i| {
                    let a = addr + i * step;
                    let text = if thumb {
                        arm::format_thumb(&arm::decode_thumb(read16(system, a)))
                    } else {
                        arm::format_arm(&arm::decode_arm(read32(system, a)))
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
            // GBA carries the richer register/background summary; the DS's per-BG state
            // is available through video.scanline / video.explainPixel instead.
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
