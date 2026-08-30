//! A TCP debug server speaking newline-delimited JSON.
//!
//! The emulator runs single-threaded on the main thread (it owns [`System`]); a
//! listener thread accepts a client and forwards each request line to the main
//! loop over a channel, so every request is serviced against a consistent state.
//!
//! Requests are `{"id":N,"method":"ns.method","params":{...}}`; responses are
//! `{"id":N,"ok":true,"result":...}` or `{"id":N,"ok":false,"error":"..."}`.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

use gba::{Access, Key, System};
use serde_json::{json, Value};

/// Run the debug server on `port`, owning `system`. Blocks forever (the caller's
/// thread becomes the emulator/request loop).
pub fn serve(mut system: System, port: u16) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    eprintln!("[debug] listening on 127.0.0.1:{port}");

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
        let _ = rtx.send(handle(&mut system, &line));
    }
    Ok(())
}

fn handle(system: &mut System, line: &str) -> String {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "error": format!("bad json: {e}")}).to_string(),
    };
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    match dispatch(system, method, &params) {
        Ok(result) => json!({"id": id, "ok": true, "result": result}).to_string(),
        Err(e) => json!({"id": id, "ok": false, "error": e}).to_string(),
    }
}

// --- helpers ---

fn u32p(params: &Value, key: &str) -> u32 {
    params.get(key).and_then(Value::as_u64).unwrap_or(0) as u32
}
fn u64p(params: &Value, key: &str, default: u64) -> u64 {
    params.get(key).and_then(Value::as_u64).unwrap_or(default)
}

fn read8(system: &mut System, addr: u32) -> u8 {
    system
        .gba
        .bus
        .read8(addr, Access::cpu_data(), &mut system.scheduler)
        .value
}
fn read16(system: &mut System, addr: u32) -> u16 {
    system
        .gba
        .bus
        .read16(addr, Access::cpu_data(), &mut system.scheduler)
        .value
}
fn read32(system: &mut System, addr: u32) -> u32 {
    system
        .gba
        .bus
        .read32(addr, Access::cpu_data(), &mut system.scheduler)
        .value
}

/// The memory region name for an address, for readouts.
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

/// Base64-encode bytes (standard alphabet), so binary payloads (the framebuffer)
/// ride in JSON without a dependency.
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

fn dispatch(system: &mut System, method: &str, params: &Value) -> Result<Value, String> {
    Ok(match method {
        // --- run control ---
        "run.frames" => {
            for _ in 0..u64p(params, "n", 1) {
                system.run_frame();
            }
            state_summary(system)
        }
        "run.toFrame" => {
            let target = u64p(params, "frame", 0);
            let mut guard = 0;
            while system.gba.bus.io.video.frame() < target && guard < 100_000 {
                system.run_frame();
                guard += 1;
            }
            state_summary(system)
        }
        "run.steps" => {
            for _ in 0..u64p(params, "n", 1) {
                system.step();
            }
            state_summary(system)
        }
        "run.untilPc" => {
            let pc = u32p(params, "pc");
            let max = u64p(params, "maxSteps", 20_000_000);
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
            let addr = u32p(params, "addr");
            let max = u64p(params, "maxSteps", 20_000_000);
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

        // --- cpu ---
        "cpu.registers" => {
            let cpsr = system.cpu.cpsr();
            let r: Vec<u32> = (0..16).map(|i| system.cpu.register(i)).collect();
            json!({
                "r": r,
                "pc": system.cpu.register(15),
                "cpsr": {
                    "thumb": cpsr.thumb(),
                    "irqDisabled": cpsr.irq_disabled(),
                    "fiqDisabled": cpsr.fiq_disabled(),
                    "mode": format!("{:?}", system.cpu.mode()),
                }
            })
        }
        "cpu.setReg" => {
            let index = u32p(params, "index") as usize;
            system.cpu.set_register(index, u32p(params, "value"));
            json!({"ok": true})
        }

        // --- memory ---
        "memory.read" => {
            let addr = u32p(params, "addr");
            let len = u64p(params, "len", 16) as u32;
            let bytes: Vec<u8> = (0..len).map(|i| read8(system, addr + i)).collect();
            json!({"addr": addr, "len": len, "base64": base64(&bytes)})
        }
        "memory.readU32" => json!({"value": read32(system, u32p(params, "addr"))}),
        "memory.readU16" => json!({"value": read16(system, u32p(params, "addr"))}),
        "memory.writeU32" => {
            let (addr, value) = (u32p(params, "addr"), u32p(params, "value"));
            system.gba.bus.write32(addr, value, Access::cpu_data(), &mut system.scheduler);
            json!({"ok": true})
        }
        "memory.writeU8" => {
            let (addr, value) = (u32p(params, "addr"), u32p(params, "value") as u8);
            system.gba.bus.write8(addr, value, Access::cpu_data(), &mut system.scheduler);
            json!({"ok": true})
        }
        "memory.watch" => {
            let enable = params.get("enable").and_then(Value::as_bool).unwrap_or(true);
            match params.get("kind").and_then(Value::as_str).unwrap_or("writes") {
                "reads" => {
                    system.gba.bus.read_watch = enable.then(Default::default);
                }
                _ => {
                    system.gba.bus.write_watch = enable.then(Default::default);
                }
            }
            json!({"ok": true})
        }
        "memory.watchReport" => {
            let top = u64p(params, "top", 24) as usize;
            let watch = match params.get("kind").and_then(Value::as_str).unwrap_or("writes") {
                "reads" => &system.gba.bus.read_watch,
                _ => &system.gba.bus.write_watch,
            };
            watch.as_ref().map(|w| addr_entries(w, top)).unwrap_or(Value::Null)
        }

        // --- execution profiling / disassembly ---
        "execution.hottestPc" => {
            let steps = u64p(params, "steps", 20_000);
            let top = u64p(params, "top", 12) as usize;
            let mut hist: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
            for _ in 0..steps {
                system.step();
                *hist.entry(system.cpu.register(15)).or_insert(0) += 1;
            }
            addr_entries(&hist, top)
        }
        "execution.disassemble" => {
            let addr = u32p(params, "addr");
            let count = u64p(params, "count", 16) as u32;
            let thumb = params
                .get("thumb")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| system.cpu.cpsr().thumb());
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

        // --- video ---
        "video.state" => {
            let v = &system.gba.bus.io.video;
            let bgs: Vec<Value> = v
                .backgrounds()
                .iter()
                .enumerate()
                .map(|(i, b)| json!({"index": i, "enabled": b.enabled, "priority": b.priority, "kind": format!("{:?}", b.kind)}))
                .collect();
            json!({"mode": v.current_mode(), "dispcnt": v.read_dispcnt(), "frame": v.frame(), "backgrounds": bgs})
        }
        "video.framebuffer" => {
            let mut rgba = Vec::with_capacity(240 * 160 * 4);
            for color in system.framebuffer() {
                rgba.extend_from_slice(&color.to_rgba8());
            }
            json!({"width": 240, "height": 160, "rgba": base64(&rgba)})
        }
        "video.explainPixel" => {
            let (x, y) = (u32p(params, "x") as u16, u32p(params, "y") as u16);
            match system.explain_pixel(x, y) {
                Ok(ex) => {
                    let candidates: Vec<Value> = ex
                        .candidates
                        .iter()
                        .map(|c| json!({
                            "layer": format!("{:?}", c.candidate.layer),
                            "color": c.candidate.color.0,
                            "visible": c.visible_after_window,
                            "addresses": c.provenance.source_addresses(),
                        }))
                        .collect();
                    json!({
                        "x": ex.x, "y": ex.y,
                        "finalColor": ex.final_color.0,
                        "videoMode": ex.video_mode,
                        "topLayer": format!("{:?}", ex.resolved.top.layer),
                        "candidates": candidates,
                    })
                }
                Err(e) => return Err(format!("{e:?}")),
            }
        }
        "video.spriteAt" => {
            let (x, y) = (u32p(params, "x") as u16, u32p(params, "y") as u16);
            match system.gba.bus.with_video_view(|v, mem| v.sprite_at(x, y, mem)) {
                Some(p) => json!({
                    "oamIndex": p.oam_index,
                    "oamAddress": p.oam_address,
                    "tileNumber": p.tile_number,
                    "tileAddress": p.tile_address,
                    "priority": p.priority,
                }),
                None => Value::Null,
            }
        }

        // --- scheduler / interrupts / input ---
        "scheduler.pendingEvents" => {
            let events: Vec<Value> = system
                .scheduler
                .pending_events()
                .iter()
                .map(|e| json!({"at": e.at, "kind": format!("{:?}", e.kind)}))
                .collect();
            json!({"now": system.scheduler.now(), "events": events})
        }
        "interrupts.state" => {
            let irq = &system.gba.bus.io.irq;
            json!({"ie": irq.ie(), "if": irq.iflags(), "ime": irq.ime(), "pending": irq.pending()})
        }
        "input.set" => {
            let name = params.get("key").and_then(Value::as_str).unwrap_or("");
            let pressed = params.get("pressed").and_then(Value::as_bool).unwrap_or(true);
            match Key::from_name(name) {
                Some(key) => {
                    system.set_key(key, pressed);
                    json!({"ok": true})
                }
                None => return Err(format!("unknown key: {name}")),
            }
        }

        other => return Err(format!("unknown method: {other}")),
    })
}

/// A compact "where are we" summary returned by run commands.
fn state_summary(system: &mut System) -> Value {
    let pc = system.cpu.register(15);
    json!({
        "frame": system.gba.bus.io.video.frame(),
        "pc": pc,
        "region": region(pc),
        "dispcnt": system.gba.bus.io.video.read_dispcnt(),
    })
}
