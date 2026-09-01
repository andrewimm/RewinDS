//! Temporary cycle-trace capture harness (feature `cyctrace`), for validating
//! ARM9 cycle accuracy against a reference cycle trace. Remove once validated.
//!
//! Run with (paths supplied by the developer; nothing is bundled):
//!   CYC_ROM=/path/to/game.nds \
//!   CYC_BIOS9=/path/to/bios9.bin CYC_BIOS7=/path/to/bios7.bin \
//!   CYC_OUT=/path/our_arm9_cycles.txt CYC_N=3000000 \
//!   cargo test -p nds --features cyctrace --test cyc_capture -- --ignored --nocapture
#![cfg(feature = "cyctrace")]

use nds::system::{cyctrace, System};

fn expand(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        format!("{}/{}", std::env::var("HOME").unwrap(), rest)
    } else {
        path.to_string()
    }
}

/// Boot the ROM and report progress each frame: DISPCNT (display setup) and the
/// count of non-black pixels. Validates whether the ROM gets past the IPC handshake.
#[test]
#[ignore]
fn boot_probe() {
    use nds::memory::Core;

    let rom = std::fs::read(expand(&std::env::var("CYC_ROM").expect("set CYC_ROM"))).unwrap();
    let b9 = std::fs::read(expand(&std::env::var("CYC_BIOS9").expect("set CYC_BIOS9"))).unwrap();
    let b7 = std::fs::read(expand(&std::env::var("CYC_BIOS7").expect("set CYC_BIOS7"))).unwrap();
    let frames: u64 = std::env::var("CYC_FRAMES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);

    let mut system = System::new();
    system.load_bios(&b9, &b7);
    system.direct_boot(&rom).expect("direct boot");

    for f in 0..frames {
        system.run_frame();
        let dispcnt = system.read(Core::Arm9, 0x0400_0000, 4);
        let nonblack = system.framebuffer().iter().filter(|&&p| p != 0).count();
        if f % 10 == 0 || nonblack > 0 || dispcnt != 0 {
            eprintln!("frame {f:3}: DISPCNT={dispcnt:#010x}  nonblack={nonblack}");
        }
    }
}

#[test]
#[ignore]
fn capture_arm9_cycles() {
    let rom_path = expand(&std::env::var("CYC_ROM").expect("set CYC_ROM"));
    let bios9 = expand(&std::env::var("CYC_BIOS9").expect("set CYC_BIOS9"));
    let bios7 = expand(&std::env::var("CYC_BIOS7").expect("set CYC_BIOS7"));
    let out = expand(&std::env::var("CYC_OUT").expect("set CYC_OUT"));
    let target: usize = std::env::var("CYC_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3_000_000);

    let rom = std::fs::read(&rom_path).expect("read rom");
    let b9 = std::fs::read(&bios9).expect("read bios9");
    let b7 = std::fs::read(&bios7).expect("read bios7");

    let mut system = System::new();
    system.load_bios(&b9, &b7);
    system.direct_boot(&rom).expect("direct boot");

    cyctrace::enable();
    // Advance the timeline in chunks until we have captured enough ARM9 instructions.
    let mut now = system.now();
    while cyctrace::len() < target {
        now += 1_000_000;
        system.run_until(now);
    }
    cyctrace::dump(&out);
    eprintln!("captured {} ARM9 instructions -> {}", cyctrace::len(), out);
}
