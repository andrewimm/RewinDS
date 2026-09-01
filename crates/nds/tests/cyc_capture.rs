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

/// Capture the ARM9 PC trace to CYC_OUT (for resync-diffing against a reference).
#[test]
#[ignore]
fn capture() {
    let mut sys = booted();
    let out = expand(&std::env::var("CYC_OUT").expect("CYC_OUT"));
    let target: usize = std::env::var("CYC_N").ok().and_then(|s| s.parse().ok()).unwrap_or(11_000_000);
    cyctrace::enable();
    let mut now = sys.now();
    while cyctrace::len() < target {
        now += 1_000_000;
        sys.run_until(now);
    }
    cyctrace::dump(&out);
    eprintln!("captured {} ARM9 PCs -> {}", cyctrace::len(), out);
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
    for (core, write, value, clock) in log.iter().take(40) {
        let who = if *core == 0 { "ARM9" } else { "ARM7" };
        let kind = if *write { "WROTE" } else { "read " };
        eprintln!("  {who} {kind} {value:#010x} @ clock {clock}");
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
    let mut a = 0;
    while a < len {
        let at = addr + a;
        if thumb {
            let w = sys.read(Core::Arm9, at, 2) as u16;
            eprintln!("  {at:#010x}: {w:04x}      {}", arm::format_thumb(&arm::decode_thumb(w)));
            a += 2;
        } else {
            let w = sys.read(Core::Arm9, at, 4);
            eprintln!("  {at:#010x}: {w:08x}  {}", arm::format_arm(&arm::decode_arm(w)));
            a += 4;
        }
    }
}

/// Report DISPCNT + non-black pixel count each frame (has the game reached display?).
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
