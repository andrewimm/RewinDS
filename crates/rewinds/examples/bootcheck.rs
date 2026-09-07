//! Headless boot smoke-check: verifies firmware boot renders (not a white screen)
//! and that a cart launched from firmware advances its ARM9 past the early stall.
//! Throwaway diagnostic — run with the real firmware/BIOS/ROM paths as args.

use emulator::{Console, Emulator, Load};

fn nonwhite_fraction(rgba: &[u8]) -> f32 {
    let mut nonwhite = 0usize;
    let px = rgba.len() / 4;
    for c in rgba.chunks_exact(4) {
        // "White" = all three channels near max.
        if !(c[0] > 240 && c[1] > 240 && c[2] > 240) {
            nonwhite += 1;
        }
    }
    nonwhite as f32 / px as f32
}

fn main() {
    let mut a = std::env::args().skip(1);
    let rom_path = a.next().expect("rom path");
    let bios9 = a.next().expect("bios9 path");
    let bios7 = a.next().expect("bios7 path");
    let fw = a.next().expect("firmware path");

    let rom = std::fs::read(&rom_path).unwrap();
    let b9 = std::fs::read(&bios9).unwrap();
    let b7 = std::fs::read(&bios7).unwrap();
    let firmware = std::fs::read(&fw).unwrap();

    // DIRECT=1 skips firmware and direct-boots the cart (the desmume-compared path).
    let direct = std::env::var_os("DIRECT").is_some();
    let mut emu = Emulator::load(Load {
        rom: Some(&rom),
        console: Some(Console::Nds),
        bios: Some(&b9),
        bios7: Some(&b7),
        firmware: if direct { None } else { Some(&firmware) },
    })
    .expect("load");
    if direct && std::env::var_os("MEASURE").is_some() {
        use nds::Core::{Arm7, Arm9};
        // Reach just before the first IPC send.
        for _ in 0..10 {
            emu.run_frame();
        }
        let nds = emu.as_nds_mut().unwrap();
        // Measure the cost of one poll round-trip: master-ticks between consecutive
        // ARM9 sends of 0x1ab (at 0x020d6738), plus how many instructions each core
        // runs in that span and how each core's clock advances (idle = clock jumps
        // without instructions).
        // Advance to the phase-2 break-out send (first 0x020d6738 send whose value
        // r1 != 0x1ab), then trace the ARM9's non-idle path through the 7 sends and
        // into the spin, so the decision to stop shows.
        // Advance to our LAST phase-2 ARM9 send (0x40400088); the next round-trip's
        // reply is where the ARM9 decides to spin instead of sending an 8th message.
        let mut got = false;
        for _ in 0..8000u64 {
            let (_, hit) = nds.debug_run(Arm9, 20_000_000, Some(0x020d_6738), None, None);
            if !hit {
                break;
            }
            if nds.arm9.register(1) == 0x0F26_C04D {
                got = true;
                break;
            }
            nds.debug_run(Arm9, 1, None, None, None);
        }
        println!(
            "anchor (0F26C04D send) reached: {got}, pc={:08X} r0={:08X} r1={:08X}",
            nds.arm9.register(15),
            nds.arm9.register(0),
            nds.arm9.register(1)
        );
        println!("anchor clock={}", nds.now());
        if std::env::var_os("A7DIS").is_some() {
            println!("--- ARM7 BIOS 00002DB8..00002DE0 (ARM) ---");
            for i in 0..11u32 {
                let addr = 0x0000_2DB8 + i * 4;
                let raw = nds.read(Arm7, addr, 4);
                println!("  {addr:08X}: {raw:08X}  {}", arm::format_arm(&arm::decode_arm(raw)));
            }
            return;
        }
        // ARM7_TRACE: trace the ARM7 with per-instruction (PC clock) to compare cycle
        // timing against desmume. Otherwise emit the ARM9 register trace.
        if let Ok(p) = std::env::var("ARM7_TRACE") {
            let n = nds.debug_trace(Arm7, 0, 40000, &p).unwrap();
            println!("wrote {n} ARM7 (PC clock) lines to {p}");
        } else {
            let path = std::env::var("OURS_TRACE").unwrap_or_else(|_| "/tmp/ours_trace.txt".into());
            let n = nds.debug_trace_regs(Arm9, 0, 800_000, &path).unwrap();
            println!("wrote {n} ARM9 trace lines to {path}");
        }
        return;
    }

    if direct && std::env::var_os("TRACE").is_some() {
        use nds::Core::Arm9;
        // Run near the poll break (~frame 204), then single-step to catch the exact
        // break-out send (0x87875007 at 0x020d6738) and trace the main thread's
        // phase-2 work from there until it blocks.
        for _ in 0..180 {
            emu.run_frame();
        }
        let nds = emu.as_nds_mut().unwrap();
        // Advance to the break-out send efficiently: run to each IPCFIFO send site
        // (0x020d6738) at native speed and check the value being sent (r1).
        let mut found = false;
        for _ in 0..200_000u64 {
            let (_, hit) = nds.debug_run(Arm9, 5_000_000, Some(0x020d_6738), None, None);
            if !hit {
                break;
            }
            if nds.arm9.register(1) == 0x0F26_C04D {
                found = true;
                break;
            }
            nds.debug_run(Arm9, 1, None, None, None); // step past this send
        }
        println!("break-out send reached: {found}");
        // Now trace the main thread's non-idle work.
        let mut logged = 0;
        let mut last_pc = 0u32;
        for _ in 0..8_000_000u64 {
            let pc = nds.arm9.register(15);
            let idle = (0x020d_3f40..0x020d_3f68).contains(&pc);
            if !idle && pc != last_pc {
                let thumb = nds.arm9.cpsr().thumb();
                let regs: Vec<u32> = (0..6).map(|i| nds.arm9.register(i)).collect();
                let (raw, dis) = if thumb {
                    let r = nds.read(Arm9, pc, 2);
                    (r, arm::format_thumb(&arm::decode_thumb(r as u16)))
                } else {
                    let r = nds.read(Arm9, pc, 4);
                    (r, arm::format_arm(&arm::decode_arm(r)))
                };
                println!(
                    "  {pc:08X}: {raw:08X}  {dis:<22} r0={:08X} r1={:08X} r2={:08X} r3={:08X} r4={:08X} r5={:08X}",
                    regs[0], regs[1], regs[2], regs[3], regs[4], regs[5]
                );
                logged += 1;
                if logged > 1200 {
                    break;
                }
            }
            last_pc = pc;
            nds.debug_run(Arm9, 1, None, None, None);
        }
        println!("(logged {logged} ARM9 non-idle instructions at phase-2 stall)");
        return;
    }

    if direct && std::env::var_os("CACHECHK").is_some() {
        // Sample the ARM9 CP15 cache-enable state across the crt0 decompression window
        // (ours' #9->#10 spans ~frame 23..135) to confirm caches are off during boot.
        for f in 1..=150u32 {
            emu.run_frame();
            if [30u32, 60, 90, 120, 150].contains(&f) {
                let cp = emu.as_nds().unwrap().cp15();
                println!(
                    "frame {f}: dcache={} icache={} dtcm={} itcm={}",
                    cp.dcache_enabled(),
                    cp.icache_enabled(),
                    cp.dtcm_enabled(),
                    cp.itcm_enabled()
                );
            }
        }
        return;
    }

    if direct && std::env::var_os("PCTRACE").is_some() {
        use nds::{Core::Arm9, RegWatch};
        // Anchor on the ARM9 sending a specific IPC value (all ARM9 PXI sends funnel
        // through 0x020d6738 with the value in r1), then trace the ARM9 for TRACEN
        // instructions to TRACEOUT — for diffing against the oracle's PC trace to
        // locate where boot control flow / computed values diverge post-fix.
        let target = u32::from_str_radix(
            std::env::var("PCTRACE").ok().filter(|s| !s.is_empty()).as_deref().unwrap_or("8008C004"),
            16,
        )
        .unwrap();
        let n: u64 = std::env::var("TRACEN").ok().and_then(|s| s.parse().ok()).unwrap_or(20000);
        let out = std::env::var("TRACEOUT").unwrap_or_else(|_| "/tmp/our_pctrace.txt".into());
        let nds = emu.as_nds_mut().unwrap();
        let (_, hit) = nds.debug_run(
            Arm9,
            2_000_000_000,
            None,
            None,
            Some(RegWatch { reg: 1, value: target, pc_lo: 0x020d_6730, pc_hi: 0x020d_6739, skip: 0 }),
        );
        println!("anchor 0x{target:08X} reached: {hit}, pc={:08X}", nds.arm9.register(15));
        if hit {
            if let Ok(s) = std::env::var("STOPAT") {
                let stop = u32::from_str_radix(&s, 16).unwrap();
                let trace_at: Option<u32> = std::env::var("STOPTRACE").ok().and_then(|s| s.parse().ok());
                for k in 0..8u32 {
                    let (_, h) = nds.debug_run(Arm9, 2_000_000_000, Some(stop), None, None);
                    if !h {
                        println!("call #{k}: stop_pc not reached");
                        break;
                    }
                    let r: Vec<String> = (0..16).map(|i| format!("r{i}={:08X}", nds.arm9.register(i))).collect();
                    println!("call #{k} @ {stop:08X}: {}", r.join(" "));
                    if trace_at == Some(k) {
                        let n: u64 = std::env::var("TRACEN").ok().and_then(|s| s.parse().ok()).unwrap_or(600);
                        nds.debug_trace_regs(Arm9, 0, n, "/tmp/e8cc.txt").unwrap();
                        println!("traced {n} regs from call #{k} to /tmp/e8cc.txt");
                        return;
                    }
                    nds.debug_run(Arm9, 1, None, None, None);
                }
                return;
            }
            let wrote = if std::env::var_os("REGS").is_some() {
                nds.debug_trace_regs(Arm9, 0, n, &out).unwrap()
            } else {
                nds.debug_trace(Arm9, 0, n, &out).unwrap()
            };
            println!("wrote {wrote} ARM9 lines to {out}");
        }
        return;
    }

    if direct && std::env::var_os("DIS7").is_some() {
        use nds::Core::Arm7;
        for _ in 0..320 {
            emu.run_frame();
        }
        let nds = emu.as_nds_mut().unwrap();
        let base = u32::from_str_radix(&std::env::var("DIS7").unwrap(), 16).unwrap();
        for i in 0..52u32 {
            let addr = base + i * 4;
            let raw = nds.read(Arm7, addr, 4);
            println!("  {addr:08X}: {raw:08X}  {}", arm::format_arm(&arm::decode_arm(raw)));
        }
        return;
    }

    if direct && std::env::var_os("ARM7TRACE").is_some() {
        use nds::Core::Arm7;
        let frames: u32 = std::env::var("A7FRAMES").ok().and_then(|s| s.parse().ok()).unwrap_or(320);
        let n: u64 = std::env::var("ARM7TRACE").ok().and_then(|s| s.parse().ok()).unwrap_or(40000);
        for _ in 0..frames {
            emu.run_frame();
        }
        let nds = emu.as_nds_mut().unwrap();
        let wrote = nds.debug_trace(Arm7, 0, n, "/tmp/our_a7.txt").unwrap();
        println!("wrote {wrote} ARM7 PC lines to /tmp/our_a7.txt");
        return;
    }

    if direct && std::env::var_os("WWATCH").is_some() {
        use nds::Core::{Arm7, Arm9};
        let core = if std::env::var("WCORE").as_deref() == Ok("7") { Arm7 } else { Arm9 };
        let addr = u32::from_str_radix(&std::env::var("WWATCH").unwrap(), 16).unwrap();
        let want = std::env::var("WVAL").ok().and_then(|s| u32::from_str_radix(&s, 16).ok());
        let iters: u32 = std::env::var("WWATCHN").ok().and_then(|s| s.parse().ok()).unwrap_or(12);
        let nds = emu.as_nds_mut().unwrap();
        for k in 0..iters {
            let (_, hit) = nds.debug_run(core, 2_000_000_000, None, Some(addr), None);
            if !hit {
                println!("write #{k}: not hit");
                break;
            }
            let reg = |nds: &nds::System, i: usize| if core == Arm7 { nds.arm7.register(i) } else { nds.arm9.register(i) };
            let pc = reg(nds, 15);
            let val = nds.read(core, addr, 4);
            if want.is_none() || want == Some(val) {
                println!(
                    "write #{k}: pc={pc:08X} -> [{addr:08X}]={val:08X}  r5={:08X} r6={:08X} r7={:08X}",
                    reg(nds, 5), reg(nds, 6), reg(nds, 7),
                );
            }
            nds.debug_run(core, 1, None, None, None); // step past
        }
        return;
    }

    if direct && std::env::var_os("MEMRD").is_some() {
        use nds::Core::Arm9;
        let base = u32::from_str_radix(&std::env::var("MEMRD").unwrap(), 16).unwrap();
        let frames: u32 = std::env::var("MEMFRAMES").ok().and_then(|s| s.parse().ok()).unwrap_or(400);
        for _ in 0..frames {
            emu.run_frame();
        }
        let nds = emu.as_nds_mut().unwrap();
        for i in 0..8u32 {
            let a = base + i * 4;
            println!("  [{a:08X}] = {:08X}", nds.read(Arm9, a, 4));
        }
        return;
    }

    if direct && std::env::var_os("DIST").is_some() {
        use nds::Core::Arm9;
        for _ in 0..40 {
            emu.run_frame();
        }
        let nds = emu.as_nds_mut().unwrap();
        let base = u32::from_str_radix(&std::env::var("DIST").unwrap(), 16).unwrap();
        for i in 0..40u32 {
            let addr = base + i * 2;
            let raw = nds.read(Arm9, addr, 2) as u16;
            println!("  {addr:08X}: {raw:04X}  {}", arm::format_thumb(&arm::decode_thumb(raw)));
        }
        return;
    }

    if direct && std::env::var_os("DIS").is_some() {
        use nds::Core::Arm9;
        for _ in 0..40 {
            emu.run_frame();
        }
        let nds = emu.as_nds_mut().unwrap();
        let base = u32::from_str_radix(&std::env::var("DIS").unwrap(), 16).unwrap();
        for i in 0..48u32 {
            let addr = base + i * 4;
            let raw = nds.read(Arm9, addr, 4);
            println!("  {addr:08X}: {raw:08X}  {}", arm::format_arm(&arm::decode_arm(raw)));
        }
        return;
    }

    if direct && std::env::var_os("SNAP").is_some() {
        use nds::Core::{Arm7, Arm9};
        let frames: u32 = std::env::var("SNAP").ok().and_then(|s| s.parse().ok()).unwrap_or(2000);
        for _ in 0..frames {
            emu.run_frame();
        }
        let dispcnt = emu.as_nds_mut().unwrap().io_read(nds::Core::Arm9, 0x0400_0000, 4);
        println!("after {frames} frames: DISPCNT_A={dispcnt:08X}");
        let nds = emu.as_nds_mut().unwrap();
        let rd = |nds: &mut nds::System, core, addr| nds.io_read(core, addr, 4);
        for (name, core) in [("ARM9", Arm9), ("ARM7", Arm7)] {
            let (pc, cpsr) = if core == Arm9 {
                (nds.arm9.register(15), nds.arm9.cpsr())
            } else {
                (nds.arm7.register(15), nds.arm7.cpsr())
            };
            let ie = rd(nds, core, 0x0400_0210);
            let iff = rd(nds, core, 0x0400_0214);
            let ime = rd(nds, core, 0x0400_0208);
            let sync = rd(nds, core, 0x0400_0180);
            let fifocnt = rd(nds, core, 0x0400_0184);
            println!(
                "{name}: pc={pc:08X} I={} T={} IME={} IE={ie:08X} IF={iff:08X} pend={:08X} SYNC={sync:04X} FIFOCNT={fifocnt:04X}",
                cpsr.irq_disabled() as u8, cpsr.thumb() as u8, ime & 1, ie & iff,
            );
        }
        // Sample each core's PC over a few hundred steps to see if it's spinning.
        for (name, core) in [("ARM9", Arm9), ("ARM7", Arm7)] {
            let mut seen = std::collections::BTreeSet::new();
            for _ in 0..400 {
                let pc = if core == Arm9 { nds.arm9.register(15) } else { nds.arm7.register(15) };
                seen.insert(pc);
                nds.debug_run(core, 1, None, None, None);
            }
            let lo = seen.iter().next().copied().unwrap_or(0);
            let hi = seen.iter().next_back().copied().unwrap_or(0);
            println!("{name} PC span over 400 steps: {} distinct, {lo:08X}..{hi:08X}", seen.len());
        }
        return;
    }

    if direct {
        // Drain the IPC send log every frame (so the ring never wraps) and collect
        // the non-poll sends with a running count of collapsed 0x1ab/0x6b poll
        // iterations — directly comparable to desmume's transition log.
        // Long boot-progress monitor: is HeartGold still advancing (new non-poll IPC
        // transitions appearing) or plateaued at a later stall?
        let mut loopn = 0u64;
        let mut trans = 0u64;
        let mut last: u32 = 0;
        for i in 0..15000u32 {
            emu.run_frame();
            let nds = emu.as_nds_mut().unwrap();
            for (core, kind, value, _pc) in nds.ipc_drain() {
                if kind != 0 {
                    continue;
                }
                if (core == 0 && value == 0x1ab) || (core == 1 && value == 0x6b) {
                    loopn += 1;
                    continue;
                }
                trans += 1;
                last = value;
                if std::env::var_os("IPCSEQ").is_some() && trans <= 60 {
                    let c = if core == 0 { 9 } else { 7 };
                    println!("IPC {c} {value:08X}  (#{trans})");
                }
            }
            if i % 1000 == 999 {
                let f = emu.frame();
                let n = emu.as_nds().unwrap();
                let a9 = n.arm9.register(15);
                let dispcnt = emu.as_nds_mut().unwrap().io_read(nds::Core::Arm9, 0x0400_0000, 4);
                let top = nonwhite_fraction(emu.screen(0).expect("s").rgba);
                println!(
                    "[direct] frame={f} arm9={a9:08X} DISPCNT_A={dispcnt:08X} trans={trans} top-nonwhite={top:.3}"
                );
            }
        }
        return;
        #[allow(unreachable_code)]
        let frame = emu.frame();
        let nds = emu.as_nds_mut().unwrap();
        // Full deadlock snapshot: both cores + IPC + interrupt state.
        let rd = |nds: &mut nds::System, core, addr| nds.io_read(core, addr, 4);
        use nds::Core::{Arm7, Arm9};
        println!("\n=== deadlock snapshot @ frame {} ===", frame);
        for (name, core) in [("ARM9", Arm9), ("ARM7", Arm7)] {
            let (pc, cpsr) = if core == Arm9 {
                (nds.arm9.register(15), nds.arm9.cpsr())
            } else {
                (nds.arm7.register(15), nds.arm7.cpsr())
            };
            let ie = rd(nds, core, 0x0400_0210);
            let iff = rd(nds, core, 0x0400_0214);
            let ime = rd(nds, core, 0x0400_0208);
            let sync = rd(nds, core, 0x0400_0180);
            let fifocnt = rd(nds, core, 0x0400_0184);
            let dispstat = rd(nds, core, 0x0400_0004);
            println!(
                "{name}: pc={pc:#010x} I={} T={} IME={} IE={ie:#010x} IF={iff:#010x} active={:#x}\n      IPCSYNC={sync:#06x} FIFOCNT={fifocnt:#06x} DISPSTAT={dispstat:#06x}",
                cpsr.irq_disabled() as u8,
                cpsr.thumb() as u8,
                ime & 1,
                ie & iff,
            );
        }
        // The ARM9 idle-loop's work fn (0x020d3f4c bl -1308 -> 0x020d3a38) and a
        // candidate wait flag flagged in earlier tracing.
        println!("wait-flag[0x021e1924] = {:#010x}", nds.read(Arm9, 0x021e_1924, 4));
        println!("\n--- recent IPC log (core,kind,value,pc) ---");
        let log = nds.ipc_drain();
        for (core, kind, value, pc) in log.iter().rev().take(8).rev() {
            println!("  core={core} kind={kind} value={value:#010x} pc={pc:#010x}");
        }
        // Disassemble both handshake sites (read via bus so ARM7 memory works too).
        for (label, core, base) in [
            ("ARM9 poll-loop @020d67ac", Arm9, 0x020d6760u32),
        ] {
            println!("--- {label} ---");
            for i in 0..40 {
                let addr = base + i * 4;
                let raw = nds.read(core, addr, 4);
                let ins = arm::decode_arm(raw);
                let mark = if addr == 0x020d67ac || addr == 0x020d6738 { " <==" } else { "" };
                println!("  {addr:#010x}: {raw:08x}  {}{mark}", arm::format_arm(&ins));
            }
        }
        return;
    }

    // Firmware boot: run some frames, then sample the top screen.
    for _ in 0..90 {
        emu.run_frame();
    }
    let s = emu.screen(0).expect("screen");
    println!(
        "[firmware] frame={} top-screen non-white fraction={:.3}",
        emu.frame(),
        nonwhite_fraction(s.rgba)
    );

    // Launch the inserted cart from the firmware menu, then watch the ARM9 PC.
    {
        let nds = emu.as_nds_mut().expect("nds");
        nds.launch_cart_from_firmware().expect("launch");
    }
    for chunk in 0..40 {
        for _ in 0..8 {
            emu.run_frame();
        }
        let pc = emu.as_nds().unwrap().arm9.register(15);
        let s = emu.screen(0).expect("screen");
        println!(
            "[cart] frame={} arm9_pc={:#010x} top non-white={:.3} (chunk {})",
            emu.frame(),
            pc,
            nonwhite_fraction(s.rgba),
            chunk
        );
    }
}
