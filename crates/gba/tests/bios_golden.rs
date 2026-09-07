//! Golden tests for the built-in GBA BIOS replacement (`bios/gba/bios.s`).
//!
//! Two layers:
//!   1. Always-run checks that our BIOS obeys the *documented* contract (GBATEK):
//!      boot hand-off state, the Div/DivArm/Halt SWIs, and IRQ forwarding.
//!   2. Opt-in equivalence checks that run the *same* scenarios on a real BIOS
//!      and compare observable results. These need images the repo can't ship:
//!        REWINDS_BIOS=/path/to/gba_bios.bin   (a real BIOS)
//!        REWINDS_ROM=/path/to/game.gba        (a real ROM, for boot hand-off)
//!      Expected behavior is derived from the docs, never by disassembling the
//!      commercial BIOS — the real image is exercised only as a black box.

use arm::cpu::Mode;
use gba::{IrqSource, System, TimerId};

/// A generic user IRQ handler (assembled from clang, verified by disassembly):
/// acknowledge all pending `IF` and set every BIOS interrupt flag at
/// `0x03007FF8`, then `bx lr`. That is all IntrWait needs to observe a wake-up.
const USER_HANDLER: &[u32] = &[
    0xE3E0_0000, // mvn  r0, #0            ; 0xFFFFFFFF
    0xE3A0_1301, // mov  r1, #0x04000000
    0xE281_1C02, // add  r1, r1, #0x200
    0xE1C1_00B2, // strh r0, [r1, #2]      ; IF = 0xFFFF (write-1-clear)
    0xE3A0_2403, // mov  r2, #0x03000000
    0xE282_2C7F, // add  r2, r2, #0x7F00
    0xE282_20F8, // add  r2, r2, #0xF8     ; r2 = 0x03007FF8
    0xE1C2_00B0, // strh r0, [r2]          ; BIOS flags = 0xFFFF
    0xE12F_FF1E, // bx   lr
];

const BIOS_FLAGS: u32 = 0x0300_7FF8; // BIOS Interrupt Check Flags (16-bit)

/// Timer control bits.
const TIMER_START: u16 = 1 << 7;
const TIMER_IRQ: u16 = 1 << 6;

fn bios_flags(sys: &System) -> u16 {
    let off = (BIOS_FLAGS - 0x0300_0000) as usize;
    u16::from_le_bytes(sys.gba.bus.memory.iwram[off..off + 2].try_into().unwrap())
}

fn set_bios_flags(sys: &mut System, value: u16) {
    let off = (BIOS_FLAGS - 0x0300_0000) as usize;
    sys.gba.bus.memory.iwram[off..off + 2].copy_from_slice(&value.to_le_bytes());
}

// GBATEK "BIOS RAM Usage": the System/User stack initialises to the top of its
// reserved area (SP_svc=0x03007FE0 and SP_irq=0x03007FA0 are exercised
// behaviorally by the SWI and IRQ tests below).
const SP_USR: u32 = 0x0300_7F00;
const ROM_ENTRY: u32 = 0x0800_0000;
const USER_IRQ_PTR: u32 = 0x0300_7FFC; // pointer to the user IRQ handler

/// A cartridge whose entry point is an endless branch — enough for our BIOS to
/// hand control to (it does not run the logo intro or header validation).
fn spin_cart() -> Vec<u8> {
    let mut rom = vec![0u8; 0xC0];
    rom[0..4].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes()); // b . at 0x08000000
    rom
}

/// Write ARM words into IWRAM at `addr` (must be in 0x03000000..0x03008000).
fn put_iwram(sys: &mut System, addr: u32, words: &[u32]) {
    let base = (addr - 0x0300_0000) as usize;
    for (i, w) in words.iter().enumerate() {
        sys.gba.bus.memory.iwram[base + i * 4..base + i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
}

/// Boot `bios` with a spin cartridge and step until control reaches ROM (our
/// BIOS hands off in a handful of instructions; a real BIOS runs its intro
/// first, hence the large budget). Returns false if ROM was never reached.
fn boot_to_rom(sys: &mut System) -> bool {
    for _ in 0..5_000_000u64 {
        if in_rom(sys.cpu.register(15)) {
            return true;
        }
        sys.step();
    }
    false
}

fn in_rom(pc: u32) -> bool {
    (ROM_ENTRY..0x0E00_0000).contains(&pc)
}

fn booted(bios: &[u8]) -> System {
    let mut sys = System::new();
    sys.gba.bus.load_bios(bios);
    sys.gba.bus.load_rom(spin_cart());
    sys.cpu.set_pc(0);
    assert!(boot_to_rom(&mut sys), "BIOS never handed off to ROM");
    sys
}

/// Invoke `swi_number` after booting `bios`, with r0/r1 preloaded, and run until
/// the SWI returns to its caller. Returns (r0, r1, r3).
fn run_swi(bios: &[u8], swi_number: u8, r0: u32, r1: u32) -> (u32, u32, u32) {
    let mut sys = booted(bios);
    // A SWI followed by a spin, placed in IWRAM.
    let swi = 0xEF00_0000 | ((swi_number as u32) << 16);
    put_iwram(&mut sys, 0x0300_0000, &[swi, 0xEAFF_FFFE]);
    sys.cpu.set_register(0, r0);
    sys.cpu.set_register(1, r1);
    sys.cpu.set_pc(0x0300_0000);

    // Run until control returns to the instruction after the SWI.
    for _ in 0..5000 {
        if sys.cpu.register(15) == 0x0300_0004 {
            break;
        }
        sys.step();
    }
    assert_eq!(
        sys.cpu.register(15),
        0x0300_0004,
        "SWI {swi_number:#x} did not return to its caller"
    );
    (sys.cpu.register(0), sys.cpu.register(1), sys.cpu.register(3))
}

// --- Always-run: documented behavior of our BIOS ---------------------------

#[test]
fn boot_hands_off_in_system_mode_with_documented_stacks() {
    let sys = booted(gba::default_bios());
    // GBATEK: entry points leave the CPU in System mode; the user stack is at
    // the top of its reserved area; control is at the ROM entry point.
    assert_eq!(sys.cpu.mode(), Some(Mode::System));
    assert_eq!(sys.cpu.register(13), SP_USR, "SP_usr");
    assert_eq!(sys.cpu.register(15), ROM_ENTRY, "handoff PC");
}

#[test]
fn div_matches_documented_semantics() {
    // Signed, truncated toward zero; remainder takes the sign of the numerator;
    // r3 is the absolute value of the quotient (GBATEK "Div", incl. its own
    // -1234/10 -> -123, -4, +123 example).
    let cases: &[(i32, i32)] = &[
        (-1234, 10),
        (1234, 10),
        (100, 7),
        (-100, 7),
        (100, -7),
        (-100, -7),
        (7, 1),
        (0, 5),
        (5, 100),
        (i32::MAX, 3),
    ];
    for &(num, den) in cases {
        let (q, r, absq) = run_swi(gba::default_bios(), 0x06, num as u32, den as u32);
        assert_eq!(q as i32, num / den, "quotient for {num}/{den}");
        assert_eq!(r as i32, num % den, "remainder for {num}/{den}");
        assert_eq!(absq, (num / den).unsigned_abs(), "abs(quotient) for {num}/{den}");
    }
}

#[test]
fn div_by_zero_returns_without_hanging() {
    // Real hardware loops forever; we deliberately return zeroes so the machine
    // stays well-defined and testable.
    let (q, r, absq) = run_swi(gba::default_bios(), 0x06, 42, 0);
    assert_eq!((q, r, absq), (0, 0, 0));
}

#[test]
fn div_arm_swaps_operands() {
    // DivArm (0x07) takes r0=denominator, r1=numerator, else identical to Div.
    let (q, r, absq) = run_swi(gba::default_bios(), 0x07, 10, -1234i32 as u32);
    assert_eq!(q as i32, -123);
    assert_eq!(r as i32, -4);
    assert_eq!(absq, 123);
}

#[test]
fn halt_swi_enters_low_power() {
    // Halt (0x02) writes HALTCNT and the CPU parks inside the SWI until an IRQ.
    let mut sys = booted(gba::default_bios());
    put_iwram(&mut sys, 0x0300_0000, &[0xEF02_0000, 0xEAFF_FFFE]); // swi #0x02 ; b .
    sys.cpu.set_pc(0x0300_0000);
    let mut halted = false;
    for _ in 0..200 {
        if sys.gba.is_low_power() {
            halted = true;
            break;
        }
        sys.step();
    }
    assert!(halted, "Halt SWI did not put the machine into low-power mode");
}

#[test]
fn irq_handler_forwards_to_user_handler_and_returns() {
    // The documented BIOS IRQ path: save context, call [0x03007FFC], restore,
    // and return to the interrupted code with CPSR restored from SPSR_irq.
    let mut sys = booted(gba::default_bios());
    // A user handler in IWRAM that writes a marker to EWRAM, then returns via
    // `bx lr`. It must use memory, not a register: the BIOS saves and restores
    // r0-r3 around the call, so the interrupted code sees its registers intact.
    put_iwram(
        &mut sys,
        0x0300_0000,
        &[
            0xE3A0_0042, // mov r0, #0x42
            0xE3A0_1402, // mov r1, #0x02000000  (EWRAM base)
            0xE581_0000, // str r0, [r1]
            0xE12F_FF1E, // bx lr
        ],
    );
    put_iwram(&mut sys, USER_IRQ_PTR, &[0x0300_0000]); // install the handler pointer

    // The CPU is spinning at the ROM entry; force an IRQ entry (bypassing the
    // enable checks — we are testing the BIOS handler, not the acceptance path).
    let interrupted = sys.cpu.register(15);
    sys.cpu.take_irq();
    assert_eq!(sys.cpu.register(15), 0x18, "IRQ vector");

    for _ in 0..200 {
        if in_rom(sys.cpu.register(15)) {
            break;
        }
        sys.step();
    }
    let marker = u32::from_le_bytes(sys.gba.bus.memory.ewram[0..4].try_into().unwrap());
    assert_eq!(marker, 0x42, "user IRQ handler ran");
    assert_eq!(sys.cpu.register(15), interrupted, "resumed the interrupted code");
    assert_eq!(sys.cpu.mode(), Some(Mode::System), "CPSR restored on return");
}

/// Place `[swi #num ; b .]` at 0x03000000, preload r0/r1, and point PC at it.
fn invoke_swi(sys: &mut System, num: u8, r0: u32, r1: u32) {
    let swi = 0xEF00_0000 | ((num as u32) << 16);
    put_iwram(sys, 0x0300_0000, &[swi, 0xEAFF_FFFE]);
    sys.cpu.set_register(0, r0);
    sys.cpu.set_register(1, r1);
    sys.cpu.set_pc(0x0300_0000);
}

/// Step until control returns to the instruction after the SWI stub.
fn run_until_swi_returns(sys: &mut System, budget: u32) -> bool {
    for _ in 0..budget {
        if sys.cpu.register(15) == 0x0300_0004 {
            return true;
        }
        sys.step();
    }
    false
}

#[test]
fn intr_wait_returns_immediately_when_flag_already_set() {
    // r0=0: if a wanted flag is already set in the BIOS flags, consume it and
    // return without ever halting.
    let mut sys = booted(gba::default_bios());
    let mask = IrqSource::Timer0.mask();
    set_bios_flags(&mut sys, mask | 0x0100); // wanted bit plus an unrelated one
    invoke_swi(&mut sys, 0x04, 0, mask as u32);
    assert!(run_until_swi_returns(&mut sys, 5000), "IntrWait(discard=0) did not return");
    assert_eq!(bios_flags(&sys) & mask, 0, "awaited flag cleared");
    assert_eq!(bios_flags(&sys) & 0x0100, 0x0100, "unrelated flag preserved");
    assert!(sys.gba.bus.io.irq.ime(), "IntrWait force-enables IME");
}

#[test]
fn intr_wait_halts_then_returns_when_irq_fires() {
    // r0=1: halt until a *new* interrupt sets the awaited flag. Drive it with a
    // Timer0 IRQ; the user handler acks IF and posts the BIOS flags.
    let mut sys = booted(gba::default_bios());
    put_iwram(&mut sys, 0x0300_0100, USER_HANDLER);
    put_iwram(&mut sys, USER_IRQ_PTR, &[0x0300_0100]);
    let mask = IrqSource::Timer0.mask();
    sys.gba.bus.io.irq.set_ie(mask);
    let now = sys.scheduler.now();
    // A long period (0x8000 ticks): one overflow arrives during the wait, and
    // the next is far past IntrWait's return window (so it can't re-post the flag
    // before we observe the cleared state).
    sys.gba.bus.io.timers.write_reload(TimerId::Timer0, 0x8000);
    sys.gba.bus.io.timers.write_control(
        TimerId::Timer0,
        TIMER_START | TIMER_IRQ,
        now,
        &mut sys.scheduler,
    );
    invoke_swi(&mut sys, 0x04, 1, mask as u32);
    assert!(run_until_swi_returns(&mut sys, 200_000), "IntrWait did not wake and return");
    assert_eq!(bios_flags(&sys) & mask, 0, "awaited flag cleared on return");
}

#[test]
fn vblank_intr_wait_returns_after_a_vblank() {
    // VBlankIntrWait forces r0=1,r1=1 and waits for a fresh VBlank IRQ.
    let mut sys = booted(gba::default_bios());
    put_iwram(&mut sys, 0x0300_0100, USER_HANDLER);
    put_iwram(&mut sys, USER_IRQ_PTR, &[0x0300_0100]);
    sys.gba.bus.io.irq.set_ie(IrqSource::VBlank.mask());
    sys.gba.bus.io.video.write_dispstat(1 << 3); // enable the VBlank IRQ
    invoke_swi(&mut sys, 0x05, 0, 0); // r0/r1 are ignored by VBlankIntrWait
    assert!(run_until_swi_returns(&mut sys, 2_000_000), "VBlankIntrWait did not return");
    assert_eq!(bios_flags(&sys) & IrqSource::VBlank.mask(), 0, "VBlank flag cleared");
}

// --- Opt-in: equivalence against a real BIOS -------------------------------

fn real_bios() -> Option<Vec<u8>> {
    std::env::var("REWINDS_BIOS").ok().map(|p| std::fs::read(p).expect("read REWINDS_BIOS"))
}

#[test]
fn div_matches_real_bios() {
    let Some(real) = real_bios() else {
        eprintln!("skipping: set REWINDS_BIOS to a real BIOS image to run this");
        return;
    };
    let cases: &[(i32, i32)] = &[(-1234, 10), (100, 7), (-100, 7), (100, -7), (12345, 678)];
    for &(num, den) in cases {
        let ours = run_swi(gba::default_bios(), 0x06, num as u32, den as u32);
        let theirs = run_swi(&real, 0x06, num as u32, den as u32);
        assert_eq!(ours, theirs, "Div {num}/{den}: ours {ours:?} vs real {theirs:?}");
    }
}

#[test]
fn boot_handoff_matches_real_bios() {
    let Some(real) = real_bios() else {
        eprintln!("skipping: set REWINDS_BIOS (and REWINDS_ROM) to run this");
        return;
    };
    let Ok(rom_path) = std::env::var("REWINDS_ROM") else {
        eprintln!("skipping: set REWINDS_ROM to a real ROM (a real BIOS needs a valid logo)");
        return;
    };
    let rom = std::fs::read(rom_path).expect("read REWINDS_ROM");

    let handoff = |bios: &[u8]| -> (Option<Mode>, u32) {
        let mut sys = System::new();
        sys.gba.bus.load_bios(bios);
        sys.gba.bus.load_rom(rom.clone());
        sys.cpu.set_pc(0);
        assert!(boot_to_rom(&mut sys), "BIOS never handed off to ROM");
        (sys.cpu.mode(), sys.cpu.register(13))
    };

    // The documented invariants must agree: System mode, SP_usr at its top.
    let (our_mode, our_sp) = handoff(gba::default_bios());
    let (real_mode, real_sp) = handoff(&real);
    assert_eq!(our_mode, Some(Mode::System));
    assert_eq!(real_mode, Some(Mode::System));
    assert_eq!(our_sp, SP_USR);
    assert_eq!(real_sp, SP_USR);
}
