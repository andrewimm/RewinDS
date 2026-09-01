//! Debug probes (feature `cyctrace`) for chasing DS boot divergences. Run with:
//!   CYC_ROM=/path/game.nds CYC_BIOS9=/path/bios9.bin CYC_BIOS7=/path/bios7.bin \
//!   cargo test -p nds --features cyctrace --test cyc_capture <name> -- --ignored --nocapture
//! Names: `capture` (PC trace → CYC_OUT), `regs_at` (register trap at CYC_TRIG),
//! `watch` (log accesses to CYC_WATCH), `disasm` (disassemble CYC_ADDR/CYC_LEN),
//! `boot_probe` (per-frame DISPCNT + non-black pixel count).
#![cfg(feature = "cyctrace")]

use nds::memory::Core;
use nds::system::{cyctrace, System};

fn expand(p: &str) -> String {
    p.strip_prefix("~/")
        .map(|r| format!("{}/{}", std::env::var("HOME").unwrap(), r))
        .unwrap_or_else(|| p.to_string())
}

fn booted() -> System {
    let rom = std::fs::read(expand(&std::env::var("CYC_ROM").expect("CYC_ROM"))).unwrap();
    let b9 = std::fs::read(expand(&std::env::var("CYC_BIOS9").expect("CYC_BIOS9"))).unwrap();
    let b7 = std::fs::read(expand(&std::env::var("CYC_BIOS7").expect("CYC_BIOS7"))).unwrap();
    let mut s = System::new();
    s.load_bios(&b9, &b7);
    s.direct_boot(&rom).expect("direct boot");
    s
}

fn frames() -> u64 {
    std::env::var("CYC_FRAMES").ok().and_then(|s| s.parse().ok()).unwrap_or(30)
}

fn env_hex(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .unwrap_or(default)
}

/// Capture a core's PC trace to CYC_OUT (CYC_CORE=0 ARM9 [default], 1 ARM7), for
/// resync-diffing against a reference.
#[test]
#[ignore]
fn capture() {
    let mut sys = booted();
    let out = expand(&std::env::var("CYC_OUT").expect("CYC_OUT"));
    let core = std::env::var("CYC_CORE").ok().and_then(|s| s.parse().ok()).unwrap_or(0usize);
    let target: usize = std::env::var("CYC_N").ok().and_then(|s| s.parse().ok()).unwrap_or(11_000_000);
    cyctrace::enable();
    let mut now = sys.now();
    while cyctrace::len(core) < target {
        now += 1_000_000;
        sys.run_until(now);
    }
    cyctrace::dump(core, &out);
    eprintln!("captured {} PCs (core {core}) -> {}", cyctrace::len(core), out);
}

/// Trap an ARM9 address (CYC_TRIG) and dump the register file at each hit.
#[test]
#[ignore]
fn regs_at() {
    let mut sys = booted();
    let trig = env_hex("CYC_TRIG", 0x020d_9fa8);
    cyctrace::trigger(trig);
    for _ in 0..frames() {
        sys.run_frame();
    }
    let snaps = cyctrace::take_regs();
    eprintln!("=== {trig:#010x} reached {} times ===", snaps.len());
    for (n, r) in snaps.iter().take(4).enumerate() {
        eprintln!("hit {n}:");
        for row in 0..4 {
            let line: String =
                (0..4).map(|c| format!("r{:<2}={:08x}  ", row * 4 + c, r[row * 4 + c])).collect();
            eprintln!("  {line}");
        }
    }
}

/// Watch a byte address (CYC_WATCH) and log every read/write touching it.
#[test]
#[ignore]
fn watch() {
    let mut sys = booted();
    let addr = env_hex("CYC_WATCH", 0x027f_fcd8);
    cyctrace::watch(addr);
    for _ in 0..frames() {
        sys.run_frame();
    }
    let log = cyctrace::take_watch_log();
    eprintln!("=== {addr:#010x}: {} accesses ===", log.len());
    // With CYC_OUT set, dump the full raw timeline ("core W/R addr value clock", one
    // per line) for offline cadence analysis. Otherwise print the filtered view.
    if let Ok(path) = std::env::var("CYC_OUT") {
        use std::fmt::Write as _;
        let mut s = String::new();
        for (core, write, value, clock) in log.iter() {
            let kind = if *write { 'W' } else { 'R' };
            let _ = writeln!(s, "{core} {kind} {addr:08X} {value:08X} {clock}");
        }
        std::fs::write(expand(&path), s).unwrap();
        eprintln!("dumped {} accesses -> {}", log.len(), expand(&path));
        return;
    }
    // Only writes, or reads whose value differs from the previous (to skip spin polls).
    let mut prev = u32::MAX;
    for (core, write, value, clock) in log.iter() {
        if *write || *value != prev {
            let who = if *core == 0 { "ARM9" } else { "ARM7" };
            let kind = if *write { "WROTE" } else { "read " };
            eprintln!("  {who} {kind} {value:#010x} @ clock {clock}");
        }
        prev = *value;
    }
}

/// Disassemble a RAM range (the ARM9 binary is compressed in ROM → read from RAM).
#[test]
#[ignore]
fn disasm() {
    let mut sys = booted();
    for _ in 0..frames() {
        sys.run_frame();
    }
    let addr = env_hex("CYC_ADDR", 0x0202_5e00);
    let len = std::env::var("CYC_LEN").ok().and_then(|s| s.parse().ok()).unwrap_or(0x80u32);
    let thumb = std::env::var("CYC_THUMB").map(|s| s != "0").unwrap_or(false);
    let core = if std::env::var("CYC_CORE").ok().as_deref() == Some("1") { Core::Arm7 } else { Core::Arm9 };
    let mut a = 0;
    while a < len {
        let at = addr + a;
        if thumb {
            let w = sys.read(core, at, 2) as u16;
            eprintln!("  {at:#010x}: {w:04x}      {}", arm::format_thumb(&arm::decode_thumb(w)));
            a += 2;
        } else {
            let w = sys.read(core, at, 4);
            eprintln!("  {at:#010x}: {w:08x}  {}", arm::format_arm(&arm::decode_arm(w)));
            a += 4;
        }
    }
}

/// After `CYC_FRAMES` frames, report each core's interrupt state and the PC set it is
/// spinning in — to distinguish a handshake deadlock (spinning with IRQs off / IF idle)
/// from an interrupt wait (IRQs enabled, waiting on a VBlank/IPC IF bit that never sets).
#[test]
#[ignore]
fn boot_state() {
    use std::collections::BTreeSet;
    let mut sys = booted();
    for _ in 0..frames() {
        sys.run_frame();
    }
    for core in [Core::Arm9, Core::Arm7] {
        let (pc, irq_on) = if core == Core::Arm9 {
            (sys.arm9.register(15), sys.arm9.irq_enabled())
        } else {
            (sys.arm7.register(15), sys.arm7.irq_enabled())
        };
        let irqs = sys.interrupts(core);
        let who = if core == Core::Arm9 { "ARM9" } else { "ARM7" };
        eprintln!(
            "{who}: PC={pc:#010x} irq_enabled={irq_on} IME={} IE={:#010x} IF={:#010x} pending={} line={}",
            irqs.ime(),
            irqs.ie(),
            irqs.iflags(),
            irqs.pending(),
            irqs.line_asserted(),
        );
    }
    // Sample the spin range: record PCs over ~half a frame and list the distinct set.
    cyctrace::enable();
    let base = [cyctrace::len(0), cyctrace::len(1)];
    let now = sys.now();
    sys.run_until(now + _half_frame());
    for core in 0..2 {
        let pcs = cyctrace::slice(core, base[core]);
        let set: BTreeSet<u32> = pcs.iter().copied().collect();
        let who = if core == 0 { "ARM9" } else { "ARM7" };
        eprintln!("{who} spin: {} instrs, {} distinct PCs: {:08X?}",
            pcs.len(), set.len(), set.iter().take(24).collect::<Vec<_>>());
    }
}

fn _half_frame() -> u64 {
    // ~half a scanline-driven frame in master ticks; enough to capture a spin loop.
    nds::ppu::CYCLES_PER_LINE * 130
}

/// Run `CYC_FRAMES` frames and dump the final Engine-A framebuffer to `CYC_OUT` as a
/// binary PPM-ish raw (width, height, then RGB bytes) for offline inspection. Also
/// prints the DISPCNT so we can pick a frame that has OBJ enabled.
#[test]
#[ignore]
fn dump_frame() {
    let mut sys = booted();
    // Optionally pulse a keypad mask (CYC_KEYS, KEYINPUT bit order) to advance past
    // input-gated screens: pressed for four frames, released for four.
    let keys = env_hex("CYC_KEYS", 0);
    for f in 0..frames() {
        sys.set_keypad(if keys != 0 && f % 8 < 4 { keys } else { 0 });
        sys.run_frame();
    }
    let dispcnt_a = sys.read(Core::Arm9, 0x0400_0000, 4);
    let dispcnt_b = sys.read(Core::Arm9, 0x0400_1000, 4);
    // Engine config summary (Engine B DISPCNT, its BGxCNT, and the 9 VRAMCNT bytes).
    let bgcnt_b: Vec<u16> = (0..4).map(|i| sys.read(Core::Arm9, 0x0400_1008 + i * 2, 2) as u16).collect();
    let vramcnt: Vec<u8> = (0..9).map(|b| sys.vram_control(b)).collect();
    eprintln!("EngineB DISPCNT={dispcnt_b:#010x} BGCNT={bgcnt_b:04X?}  VRAMCNT={vramcnt:02X?}");
    for eng in 0..2 {
        let r = sys.engine_registers(eng);
        let dc = if eng == 0 { dispcnt_a } else { dispcnt_b };
        eprintln!("  Engine{} mode={} dispcnt_low={:#06x} 3d_bg0={} ext_pal={}",
            if eng == 0 { "A" } else { "B" }, dc & 7, r.dispcnt, (dc >> 3) & 1, (dc >> 30) & 3);
        for bg in 0..4 {
            let en = r.dispcnt & (1 << (8 + bg)) != 0;
            eprintln!("    BG{bg}: enabled={en} BGCNT={:#06x} (prio={} charbase={} scrbase={} bit7={} bit2={} size={}) hofs={} vofs={}",
                r.bgcnt[bg], r.bgcnt[bg] & 3, (r.bgcnt[bg] >> 2) & 0xF, (r.bgcnt[bg] >> 8) & 0x1F,
                (r.bgcnt[bg] >> 7) & 1, (r.bgcnt[bg] >> 2) & 1, (r.bgcnt[bg] >> 14) & 3,
                r.bg_hofs[bg], r.bg_vofs[bg]);
        }
        for k in 0..2 {
            eprintln!("    BG{} affine: PA={} PB={} PC={} PD={} X={} Y={}", k + 2,
                r.bg_pa[k], r.bg_pb[k], r.bg_pc[k], r.bg_pd[k], r.bg_ref_x[k], r.bg_ref_y[k]);
        }
        let cov = sys.debug_layer_coverage(eng);
        eprintln!("    layer coverage [BG0,BG1,BG2,BG3,OBJ] = {cov:?}");
    }
    // Both physical screens stacked vertically (top over bottom): 256 x 384.
    let mut rgb = Vec::with_capacity(256 * 384 * 3);
    for screen in 0..2 {
        for &p in sys.screen(screen) {
            let r = (p & 0x1F) as u8;
            let g = ((p >> 5) & 0x1F) as u8;
            let b = ((p >> 10) & 0x1F) as u8;
            rgb.push((r << 3) | (r >> 2));
            rgb.push((g << 3) | (g >> 2));
            rgb.push((b << 3) | (b >> 2));
        }
    }
    let out = expand(&std::env::var("CYC_OUT").expect("CYC_OUT"));
    let mut ppm = b"P6\n256 384\n255\n".to_vec();
    ppm.extend_from_slice(&rgb);
    std::fs::write(&out, ppm).unwrap();
    let (polys, verts) = sys.gpu3d_peak_geometry();
    let (rl_polys, rl_verts) = sys.gpu3d_render_list();
    eprintln!("frame {} DISPCNT_A={dispcnt_a:#010x} DISPCNT_B={dispcnt_b:#010x} 3D-peak polys={polys} verts={verts} render-list polys={rl_polys} verts={rl_verts} -> {out}", frames());
}

/// Run `CYC_FRAMES` frames and report the sound mixer's output: sample count, peak
/// amplitude, and how many samples are non-zero — to confirm audio is produced.
#[test]
#[ignore]
fn audio_probe() {
    let mut sys = booted();
    let keys = env_hex("CYC_KEYS", 0);
    let (mut total, mut peak, mut nonzero) = (0usize, 0i32, 0usize);
    for f in 0..frames() {
        sys.set_keypad(if keys != 0 && f % 8 < 4 { keys } else { 0 });
        sys.run_frame();
        let s = sys.take_audio();
        total += s.len();
        for &x in &s {
            peak = peak.max((x as i32).abs());
            if x != 0 {
                nonzero += 1;
            }
        }
        let (on, active, mask) = sys.sound_status();
        if f % 30 == 0 || (f == frames() - 1) {
            eprintln!("frame {f:3}: samples={total} peak={peak} nonzero={nonzero}  master_on={on} active={active} mask={mask:#06x}");
        }
    }
}

/// Report DISPCNT + non-black pixel count each frame (has the game reached display?).
#[test]
#[ignore]
fn backup_dump() {
    // BACKUP_NONFF_PROBE
    let mut sys = booted();
    for f in 0..frames() { let k=env_hex("CYC_KEYS",0); sys.set_keypad(if k!=0 && f%8<4 {k} else {0}); sys.run_frame(); }
    let b = sys.cart_backup();
    let nonff = b.iter().filter(|&&x| x != 0xFF).count();
    eprintln!("backup: {} bytes, {} non-0xFF", b.len(), nonff);
    // show the first non-FF regions
    let mut i = 0; let mut shown = 0;
    while i < b.len() && shown < 6 {
        if b[i] != 0xFF {
            let end = (i+16).min(b.len());
            eprintln!("  @{:#08x}: {:02X?}", i, &b[i..end]);
            shown += 1; i += 16;
        } else { i += 1; }
    }
}

/// Rasterize the 3D engine's render list to a 256x192 PPM (`CYC_OUT`) after
/// `CYC_FRAMES` — a visual check that the software rasterizer draws a game's geometry.
#[test]
#[ignore]
fn rasterize_3d() {
    let mut sys = booted();
    let keys = env_hex("CYC_KEYS", 0);
    for f in 0..frames() {
        sys.set_keypad(if keys != 0 && f % 8 < 4 { keys } else { 0 });
        sys.run_frame();
    }
    let (polys, verts) = sys.gpu3d_render_list();
    let (vp, clips) = sys.gpu3d_render_geometry();
    eprintln!("viewport={vp:#010x} (x1={} y1={} x2={} y2={})", vp & 0xFF, (vp >> 8) & 0xFF, (vp >> 16) & 0xFF, (vp >> 24) & 0xFF);
    for (i, c) in clips.iter().enumerate().take(12) {
        let w = c[3].max(1);
        // ndc y and the screen y my projection produces (full-screen viewport).
        let ndc_y = c[1] as f64 / w as f64;
        let sy = (192.0 * (w - c[1]) as f64) / (2.0 * w as f64);
        eprintln!("  v{i}: clip=[{},{},{},{}]  ndc_y={ndc_y:.3}  screen_y={sy:.0}", c[0], c[1], c[2], c[3]);
    }
    let (disp3dcnt, polys_summary) = sys.gpu3d_poly_summary();
    eprintln!(
        "DISP3DCNT={disp3dcnt:#06x} (tex_enable={} alpha_test={} alpha_blend={} rear_bitmap={})",
        disp3dcnt & 1, (disp3dcnt >> 2) & 1, (disp3dcnt >> 3) & 1, (disp3dcnt >> 14) & 1
    );
    let cc = sys.gpu3d_clear_color();
    eprintln!("CLEAR_COLOR={cc:#010x} (rgb=[{},{},{}] alpha={})", cc & 0x1F, (cc >> 5) & 0x1F, (cc >> 10) & 0x1F, (cc >> 16) & 0x1F);
    let dispcnt = sys.read(Core::Arm9, 0x0400_0000, 4);
    let bldcnt = sys.read(Core::Arm9, 0x0400_0050, 2);
    let bldalpha = sys.read(Core::Arm9, 0x0400_0052, 2);
    let bldy = sys.read(Core::Arm9, 0x0400_0054, 2);
    eprintln!("DISPCNT_A={dispcnt:#010x} BLDCNT={bldcnt:#06x} (mode={}) BLDALPHA={bldalpha:#06x} BLDY={bldy:#06x}", (bldcnt >> 6) & 3);
    for (i, (fmt, alpha, mode)) in polys_summary.iter().enumerate().take(12) {
        eprintln!("  poly{i}: tex_format={fmt} poly_alpha={alpha} blend_mode={mode}");
    }
    let rgb = sys.gpu3d_rasterize_rgb();
    let out = expand(&std::env::var("CYC_OUT").expect("CYC_OUT"));
    let mut ppm = b"P6\n256 192\n255\n".to_vec();
    ppm.extend_from_slice(&rgb);
    std::fs::write(&out, ppm).unwrap();
    eprintln!("3D render list: {polys} polys, {verts} verts -> {out}");
}

/// Drive the menu into gameplay (tap A repeatedly after `CYC_PRESS_FROM`), then report
/// the render config, whether the ARM9 is spinning, and dump the final frame — to
/// diagnose the "enter game → glitch + lockup" symptom.
#[test]
#[ignore]
fn play_probe() {
    use std::collections::BTreeSet;
    let mut sys = booted();
    // Decimal frame index (not hex — it's a frame count).
    let press_from: u64 = std::env::var("CYC_PRESS_FROM").ok().and_then(|s| s.parse().ok()).unwrap_or(520);
    let key = env_hex("CYC_KEYS", 0x001); // A
    // CYC_TOUCH="x,y" taps the lower-screen touch panel instead of a button.
    let touch: Option<(i32, i32)> = std::env::var("CYC_TOUCH").ok().map(|s| {
        let mut it = s.split(',').map(|v| v.trim().parse::<i32>().unwrap());
        (it.next().unwrap(), it.next().unwrap())
    });
    for f in 0..frames() {
        // Tap for 6 of every 40 frames once past the menu-reach point.
        let down = f >= press_from && (f - press_from) % 40 < 6;
        if let Some(pt) = touch {
            sys.set_touch(if down { Some(pt) } else { None });
        } else {
            sys.set_keypad(if down { key } else { 0 });
        }
        sys.run_frame();
    }
    let dispcnt_a = sys.read(Core::Arm9, 0x0400_0000, 4);
    let dispcnt_b = sys.read(Core::Arm9, 0x0400_1000, 4);
    let bgcnt_a: Vec<u16> = (0..4).map(|i| sys.read(Core::Arm9, 0x0400_0008 + i * 2, 2) as u16).collect();
    let vramcnt: Vec<u8> = (0..9).map(|b| sys.vram_control(b)).collect();
    eprintln!("DISPCNT_A={dispcnt_a:#010x} mode={} dispmode={} BGCNT_A={bgcnt_a:04X?}",
        dispcnt_a & 7, (dispcnt_a >> 16) & 3);
    eprintln!("DISPCNT_B={dispcnt_b:#010x}  VRAMCNT={vramcnt:02X?}");
    for core in [Core::Arm9, Core::Arm7] {
        let (pc, irq_on) = if core == Core::Arm9 {
            (sys.arm9.register(15), sys.arm9.irq_enabled())
        } else {
            (sys.arm7.register(15), sys.arm7.irq_enabled())
        };
        let irqs = sys.interrupts(core);
        let who = if core == Core::Arm9 { "ARM9" } else { "ARM7" };
        eprintln!("{who}: PC={pc:#010x} irq_on={irq_on} IME={} IE={:#010x} IF={:#010x} pending={}",
            irqs.ime(), irqs.ie(), irqs.iflags(), irqs.pending());
    }
    cyctrace::enable();
    let base = [cyctrace::len(0), cyctrace::len(1)];
    let now = sys.now();
    sys.run_until(now + _half_frame());
    for core in 0..2 {
        let pcs = cyctrace::slice(core, base[core]);
        let set: BTreeSet<u32> = pcs.iter().copied().collect();
        let who = if core == 0 { "ARM9" } else { "ARM7" };
        eprintln!("{who} spin: {} instrs, {} distinct PCs: {:08X?}",
            pcs.len(), set.len(), set.iter().take(24).collect::<Vec<_>>());
    }
    if let Ok(out) = std::env::var("CYC_OUT") {
        let out = expand(&out);
        let mut rgb = Vec::with_capacity(256 * 384 * 3);
        for screen in 0..2 {
            for &p in sys.screen(screen) {
                let (r, g, b) = ((p & 0x1F) as u8, ((p >> 5) & 0x1F) as u8, ((p >> 10) & 0x1F) as u8);
                rgb.push((r << 3) | (r >> 2));
                rgb.push((g << 3) | (g >> 2));
                rgb.push((b << 3) | (b >> 2));
            }
        }
        let mut ppm = b"P6\n256 384\n255\n".to_vec();
        ppm.extend_from_slice(&rgb);
        std::fs::write(&out, ppm).unwrap();
        eprintln!("-> {out}");
    }
}

/// Dump the raw AUXSPI backup transactions (segmented by chip-select) with the ARM7
/// PC that issued each, to see the exact command/address/data byte stream.
#[test]
#[ignore]
fn aux_trace() {
    nds::system::cyctrace::enable();
    let mut sys = booted();
    let keys = env_hex("CYC_KEYS", 0);
    for f in 0..frames() {
        sys.set_keypad(if keys != 0 && f % 8 < 4 { keys } else { 0 });
        sys.run_frame();
    }
    let log = nds::system::cyctrace::take_aux_log();
    eprintln!("{} clocked bytes", log.len());
    // Segment into transactions: a transaction's bytes run until in_transfer==0.
    let mut i = 0;
    let mut txn = 0;
    let only = std::env::var("CYC_ONLY").ok(); // "02" to show only that command
    while i < log.len() {
        let start = i;
        while i < log.len() && log[i].3 != 0 {
            i += 1;
        }
        if i < log.len() {
            i += 1;
        } // include the terminating byte
        let bytes = &log[start..i];
        let cmd = bytes[0].1;
        let pc = bytes[0].0;
        if only.as_deref().map(|o| u8::from_str_radix(o, 16).ok()) == Some(Some(cmd)) || only.is_none()
        {
            let ins: Vec<u8> = bytes.iter().map(|b| b.1).collect();
            let outs: Vec<u8> = bytes.iter().map(|b| b.2).collect();
            eprintln!(
                "txn {txn:3} pc={pc:08X} cmd={cmd:02X} n={} in={:02X?} out={:02X?}",
                bytes.len(),
                &ins[..ins.len().min(20)],
                &outs[..outs.len().min(20)]
            );
        }
        txn += 1;
    }
    eprintln!("{txn} transactions");
}

/// Search Main RAM for a byte substring (CYC_STR ASCII, or CYC_HEX hex bytes) after
/// `CYC_FRAMES` frames — to locate error text / data in memory.
#[test]
#[ignore]
fn find_bytes() {
    let mut sys = booted();
    let keys = env_hex("CYC_KEYS", 0);
    for f in 0..frames() {
        sys.set_keypad(if keys != 0 && f % 8 < 4 { keys } else { 0 });
        sys.run_frame();
    }
    let needle: Vec<u8> = if let Ok(s) = std::env::var("CYC_STR") {
        s.into_bytes()
    } else {
        let h = std::env::var("CYC_HEX").expect("CYC_STR or CYC_HEX");
        (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap()).collect()
    };
    let ram = &sys.memory().main;
    let mut hits = 0;
    for i in 0..ram.len().saturating_sub(needle.len()) {
        if ram[i..i + needle.len()] == needle[..] {
            eprintln!("hit @ {:#010x}", 0x0200_0000u32 + i as u32);
            hits += 1;
            if hits >= 16 {
                break;
            }
        }
    }
    eprintln!("{hits} hit(s) for {} bytes", needle.len());
}

#[test]
#[ignore]
fn boot_probe() {
    let mut sys = booted();
    for f in 0..frames() {
        sys.run_frame();
        let dispcnt = sys.read(Core::Arm9, 0x0400_0000, 4);
        let nonblack = sys.framebuffer().iter().filter(|&&p| p != 0).count();
        if f % 20 == 0 || nonblack > 0 || dispcnt != 0 {
            eprintln!("frame {f:3}: DISPCNT={dispcnt:#010x}  nonblack={nonblack}");
        }
    }
}
