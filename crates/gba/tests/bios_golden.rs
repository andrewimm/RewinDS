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

fn booted_with_rom(bios: &[u8], rom: Vec<u8>) -> System {
    let mut sys = System::new();
    sys.gba.bus.load_bios(bios);
    sys.gba.bus.load_rom(rom);
    sys.cpu.set_pc(0);
    assert!(boot_to_rom(&mut sys), "BIOS never handed off to ROM");
    sys
}

/// Boot `bios` far enough that the privileged stacks are set up. Our BIOS hands
/// off from a bare cartridge immediately; a real BIOS needs a valid logo, so
/// real-BIOS callers must boot through a real ROM (see `booted_with_rom`).
fn booted(bios: &[u8]) -> System {
    booted_with_rom(bios, spin_cart())
}

/// Invoke `swi_number` on an already-booted system (stacks established), with
/// r0/r1 preloaded, and run until the SWI returns. Returns (r0, r1, r3).
fn run_swi_on(sys: &mut System, swi_number: u8, r0: u32, r1: u32) -> (u32, u32, u32) {
    invoke_swi(sys, swi_number, r0, r1);
    assert!(
        run_until_swi_returns(sys, 5000),
        "SWI {swi_number:#x} did not return to its caller"
    );
    (sys.cpu.register(0), sys.cpu.register(1), sys.cpu.register(3))
}

/// Invoke `swi_number` after booting `bios` from a bare cartridge.
fn run_swi(bios: &[u8], swi_number: u8, r0: u32, r1: u32) -> (u32, u32, u32) {
    let mut sys = booted(bios);
    run_swi_on(&mut sys, swi_number, r0, r1)
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
    invoke_swi3(sys, num, r0, r1, 0);
}

/// As [`invoke_swi`], also preloading r2 (used by the memory SWIs).
fn invoke_swi3(sys: &mut System, num: u8, r0: u32, r1: u32, r2: u32) {
    invoke_swi4(sys, num, r0, r1, r2, 0);
}

/// As [`invoke_swi3`], also preloading r3 (used by ObjAffineSet's stride).
fn invoke_swi4(sys: &mut System, num: u8, r0: u32, r1: u32, r2: u32, r3: u32) {
    let swi = 0xEF00_0000 | ((num as u32) << 16);
    put_iwram(sys, 0x0300_0000, &[swi, 0xEAFF_FFFE]);
    sys.cpu.set_register(0, r0);
    sys.cpu.set_register(1, r1);
    sys.cpu.set_register(2, r2);
    sys.cpu.set_register(3, r3);
    sys.cpu.set_pc(0x0300_0000);
}

const EWRAM_BASE: u32 = 0x0200_0000;

fn put_ewram(sys: &mut System, off: usize, words: &[u32]) {
    for (i, w) in words.iter().enumerate() {
        sys.gba.bus.memory.ewram[off + i * 4..off + i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
}

fn ewram_word(sys: &System, off: usize) -> u32 {
    u32::from_le_bytes(sys.gba.bus.memory.ewram[off..off + 4].try_into().unwrap())
}

fn ewram_half(sys: &System, off: usize) -> u16 {
    u16::from_le_bytes(sys.gba.bus.memory.ewram[off..off + 2].try_into().unwrap())
}

fn put_ewram_bytes(sys: &mut System, off: usize, bytes: &[u8]) {
    sys.gba.bus.memory.ewram[off..off + bytes.len()].copy_from_slice(bytes);
}

fn ewram_bytes(sys: &System, off: usize, len: usize) -> Vec<u8> {
    sys.gba.bus.memory.ewram[off..off + len].to_vec()
}

fn ewram_halfs(sys: &System, off: usize, count: usize) -> Vec<u16> {
    (0..count).map(|i| ewram_half(sys, off + i * 2)).collect()
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

// --- CpuSet (0x0B) / CpuFastSet (0x0C) -------------------------------------

const CPUSET_FILL: u32 = 1 << 24; // fixed source address
const CPUSET_32BIT: u32 = 1 << 26; // datasize (CpuSet only)
const SRC: u32 = EWRAM_BASE; // ewram offset 0
const DST: u32 = EWRAM_BASE + 0x1000; // ewram offset 0x1000
const DST_OFF: usize = 0x1000;

#[test]
fn cpu_set_copies_words() {
    let mut sys = booted(gba::default_bios());
    let src = [0x1111_1111u32, 0x2222_2222, 0x3333_3333, 0x4444_4444];
    put_ewram(&mut sys, 0, &src);
    invoke_swi3(&mut sys, 0x0B, SRC, DST, 4 | CPUSET_32BIT);
    assert!(run_until_swi_returns(&mut sys, 5000));
    for (i, w) in src.iter().enumerate() {
        assert_eq!(ewram_word(&sys, DST_OFF + i * 4), *w, "word {i}");
    }
}

#[test]
fn cpu_set_fills_words() {
    let mut sys = booted(gba::default_bios());
    put_ewram(&mut sys, 0, &[0xDEAD_BEEF]);
    put_ewram(&mut sys, DST_OFF, &[0; 4]); // sentinel
    invoke_swi3(&mut sys, 0x0B, SRC, DST, 3 | CPUSET_32BIT | CPUSET_FILL);
    assert!(run_until_swi_returns(&mut sys, 5000));
    assert_eq!(ewram_word(&sys, DST_OFF), 0xDEAD_BEEF);
    assert_eq!(ewram_word(&sys, DST_OFF + 4), 0xDEAD_BEEF);
    assert_eq!(ewram_word(&sys, DST_OFF + 8), 0xDEAD_BEEF);
    assert_eq!(ewram_word(&sys, DST_OFF + 12), 0, "beyond count untouched");
}

#[test]
fn cpu_set_copies_halfwords() {
    let mut sys = booted(gba::default_bios());
    // Halfwords AAAA, BBBB, CCCC, DDDD packed into two words.
    put_ewram(&mut sys, 0, &[0xBBBB_AAAA, 0xDDDD_CCCC]);
    invoke_swi3(&mut sys, 0x0B, SRC, DST, 4); // 16-bit, copy
    assert!(run_until_swi_returns(&mut sys, 5000));
    assert_eq!(ewram_half(&sys, DST_OFF), 0xAAAA);
    assert_eq!(ewram_half(&sys, DST_OFF + 2), 0xBBBB);
    assert_eq!(ewram_half(&sys, DST_OFF + 4), 0xCCCC);
    assert_eq!(ewram_half(&sys, DST_OFF + 6), 0xDDDD);
}

#[test]
fn cpu_set_fills_halfwords() {
    let mut sys = booted(gba::default_bios());
    put_ewram(&mut sys, 0, &[0x0000_1357]); // low halfword 0x1357 is the fill value
    put_ewram(&mut sys, DST_OFF, &[0; 2]);
    invoke_swi3(&mut sys, 0x0B, SRC, DST, 3 | CPUSET_FILL); // 16-bit fill, 3 halfwords
    assert!(run_until_swi_returns(&mut sys, 5000));
    assert_eq!(ewram_half(&sys, DST_OFF), 0x1357);
    assert_eq!(ewram_half(&sys, DST_OFF + 2), 0x1357);
    assert_eq!(ewram_half(&sys, DST_OFF + 4), 0x1357);
    assert_eq!(ewram_half(&sys, DST_OFF + 6), 0, "beyond count untouched");
}

#[test]
fn cpu_fast_set_copies_and_rounds_word_count_up_to_eight() {
    let mut sys = booted(gba::default_bios());
    let src: Vec<u32> = (0..16).map(|i| 0x1000_0000 + i).collect();
    put_ewram(&mut sys, 0, &src);
    put_ewram(&mut sys, DST_OFF, &[0; 16]); // sentinel
    // Request 10 words; the GBA rounds up to 16 (two 8-word blocks).
    invoke_swi3(&mut sys, 0x0C, SRC, DST, 10);
    assert!(run_until_swi_returns(&mut sys, 5000));
    for (i, w) in src.iter().enumerate() {
        assert_eq!(ewram_word(&sys, DST_OFF + i * 4), *w, "word {i} (incl. round-up 10->16)");
    }
}

#[test]
fn cpu_fast_set_fills() {
    let mut sys = booted(gba::default_bios());
    put_ewram(&mut sys, 0, &[0xCAFE_F00D]);
    put_ewram(&mut sys, DST_OFF, &[0; 8]);
    invoke_swi3(&mut sys, 0x0C, SRC, DST, 3 | CPUSET_FILL); // 3 -> rounds up to 8, fill
    assert!(run_until_swi_returns(&mut sys, 5000));
    for i in 0..8 {
        assert_eq!(ewram_word(&sys, DST_OFF + i * 4), 0xCAFE_F00D, "word {i}");
    }
}

#[test]
fn cpu_set_rejects_a_bios_source() {
    // GBA silently does nothing when the source reaches into the BIOS area.
    let mut sys = booted(gba::default_bios());
    put_ewram(&mut sys, DST_OFF, &[0x5555_5555; 4]);
    invoke_swi3(&mut sys, 0x0B, 0x0000_0000, DST, 4 | CPUSET_32BIT);
    assert!(run_until_swi_returns(&mut sys, 5000));
    for i in 0..4 {
        assert_eq!(ewram_word(&sys, DST_OFF + i * 4), 0x5555_5555, "word {i} untouched");
    }
}

// --- Sqrt (0x08) / ArcTan (0x09) / ArcTan2 (0x0A) --------------------------

/// Exact floor(sqrt(n)) reference.
fn isqrt(n: u32) -> u32 {
    let mut x = (n as f64).sqrt() as u64;
    while (x + 1) * (x + 1) <= n as u64 {
        x += 1;
    }
    while x * x > n as u64 {
        x -= 1;
    }
    x as u32
}

/// Radians → the GBA's 16-bit angle unit (full circle = 0x10000).
fn angle16(rad: f64) -> u16 {
    let u = (rad / std::f64::consts::TAU * 65536.0).round() as i64;
    (u & 0xFFFF) as u16
}

/// Signed circular distance between two 16-bit angles.
fn circ_diff(a: u16, b: u16) -> i32 {
    (a.wrapping_sub(b) as i16) as i32
}

#[test]
fn sqrt_matches_integer_sqrt() {
    let cases = [
        0u32, 1, 2, 3, 4, 8, 15, 16, 17, 99, 255, 256, 1000, 65535, 65536, 123_456_789,
        0x4000_0000, 0x7FFF_FFFF, 0xFFFF_FFFF,
    ];
    for n in cases {
        let (r, _, _) = run_swi(gba::default_bios(), 0x08, n, 0);
        assert_eq!(r, isqrt(n), "sqrt({n})");
    }
}

#[test]
fn arctan_approximates_arctangent() {
    // ArcTan input is 1.14 fixed-point tangent. Our CORDIC result should track
    // the true arctangent to within a couple of angle units.
    const TOL: i32 = 8;
    let tans: [i16; 11] =
        [0, 0x0800, -0x0800, 0x1000, -0x1000, 0x2000, -0x2000, 0x4000, -0x4000, 0x6000, 0x7FFF];
    for t in tans {
        let (r, _, _) = run_swi(gba::default_bios(), 0x09, (t as u16) as u32, 0);
        let want = angle16((t as f64 / 16384.0).atan());
        let d = circ_diff(r as u16, want).abs();
        assert!(d <= TOL, "arctan({t}): got {:#06X} want ~{:#06X} (off {d})", r, want);
    }
}

#[test]
fn arctan2_covers_the_full_circle() {
    const TOL: i32 = 8;
    // 1.0 = 0x4000 in 1.14; the eight octants plus a few off-axis vectors.
    let vecs: [(i16, i16); 12] = [
        (0x4000, 0),
        (0x4000, 0x4000),
        (0, 0x4000),
        (-0x4000, 0x4000),
        (-0x4000, 0),
        (-0x4000, -0x4000),
        (0, -0x4000),
        (0x4000, -0x4000),
        (0x4000, 0x2000),
        (0x2000, 0x4000),
        (-0x4000, 0x2000),
        (0x4000, -0x2000),
    ];
    for (x, y) in vecs {
        let (r, _, _) = run_swi(gba::default_bios(), 0x0A, (x as u16) as u32, (y as u16) as u32);
        let want = angle16((y as f64).atan2(x as f64));
        let d = circ_diff(r as u16, want).abs();
        assert!(d <= TOL, "atan2({y},{x}): got {:#06X} want ~{:#06X} (off {d})", r, want);
    }
}

// --- Decompressors (0x10 BitUnPack, 0x11/0x12 LZ77, 0x14/0x15 RLE,
//     0x16/0x17 Diff8, 0x18 Diff16) --------------------------------------------
//
// Trusted reference decoders (kept deliberately simple) decode the same crafted
// input the BIOS is given; the BIOS output must match them byte-for-byte.

fn header_size(src: &[u8]) -> usize {
    (u32::from_le_bytes([src[0], src[1], src[2], src[3]]) >> 8) as usize
}

fn ref_lz77(src: &[u8]) -> Vec<u8> {
    let size = header_size(src);
    let mut out = Vec::new();
    let mut i = 4;
    while out.len() < size {
        let flags = src[i];
        i += 1;
        for b in 0..8 {
            if out.len() >= size {
                break;
            }
            if flags & (0x80 >> b) != 0 {
                let (b0, b1) = (src[i], src[i + 1]);
                i += 2;
                let len = (b0 >> 4) as usize + 3;
                let disp = ((((b0 & 0xF) as usize) << 8) | b1 as usize) + 1;
                for _ in 0..len {
                    if out.len() >= size {
                        break;
                    }
                    let c = out[out.len() - disp];
                    out.push(c);
                }
            } else {
                out.push(src[i]);
                i += 1;
            }
        }
    }
    out
}

fn ref_rle(src: &[u8]) -> Vec<u8> {
    let size = header_size(src);
    let mut out = Vec::new();
    let mut i = 4;
    while out.len() < size {
        let f = src[i];
        i += 1;
        if f & 0x80 != 0 {
            let n = (f & 0x7F) as usize + 3;
            let byte = src[i];
            i += 1;
            out.extend(std::iter::repeat(byte).take(n));
        } else {
            let n = (f & 0x7F) as usize + 1;
            out.extend_from_slice(&src[i..i + n]);
            i += n;
        }
    }
    out.truncate(size);
    out
}

fn ref_diff8(src: &[u8]) -> Vec<u8> {
    let size = header_size(src);
    let mut acc = 0u8;
    (0..size)
        .map(|k| {
            acc = acc.wrapping_add(src[4 + k]);
            acc
        })
        .collect()
}

fn ref_diff16(src: &[u8]) -> Vec<u16> {
    let n = header_size(src) / 2;
    let mut acc = 0u16;
    (0..n)
        .map(|k| {
            let d = u16::from_le_bytes([src[4 + k * 2], src[4 + k * 2 + 1]]);
            acc = acc.wrapping_add(d);
            acc
        })
        .collect()
}

fn ref_bit_unpack(src: &[u8], srcw: u32, dstw: u32, offset: u32, zeroflag: bool) -> Vec<u32> {
    let mut words = Vec::new();
    let (mut outbuf, mut outbits) = (0u32, 0u32);
    let mask = (1u32 << srcw) - 1;
    for &byte in src {
        let (mut b, mut avail) = (byte as u32, 8i32);
        while avail > 0 {
            let unit = b & mask;
            b >>= srcw;
            avail -= srcw as i32;
            let val = if unit != 0 || zeroflag { unit + offset } else { unit };
            outbuf |= val << outbits;
            outbits += dstw;
            if outbits == 32 {
                words.push(outbuf);
                outbuf = 0;
                outbits = 0;
            }
        }
    }
    words
}

fn ref_huff(src: &[u8]) -> Vec<u8> {
    let header = u32::from_le_bytes([src[0], src[1], src[2], src[3]]);
    let datasize = (header & 0xF) as u32;
    let total = (header >> 8) as usize;
    let treesize = src[4] as usize;
    let tree_base = 4usize;
    let root = tree_base + 1;
    let mut bs = tree_base + (treesize + 1) * 2;
    let mask = (1u32 << datasize) - 1;
    let mut node = root;
    let (mut outbuf, mut outbits) = (0u32, 0u32);
    let (mut word, mut bitcount) = (0u32, 0u32);
    let mut out: Vec<u8> = Vec::new();
    while out.len() < total {
        if bitcount == 0 {
            word = u32::from_le_bytes([src[bs], src[bs + 1], src[bs + 2], src[bs + 3]]);
            bs += 4;
            bitcount = 32;
        }
        let bit = (word >> 31) & 1;
        word <<= 1;
        bitcount -= 1;
        let nb = src[node] as u32;
        let base = (node & !1) + (nb & 0x3F) as usize * 2 + 2;
        let (next, is_data) =
            if bit == 0 { (base, nb & 0x80 != 0) } else { (base + 1, nb & 0x40 != 0) };
        if is_data {
            outbuf |= (src[next] as u32 & mask) << outbits;
            outbits += datasize;
            if outbits == 32 {
                out.extend_from_slice(&outbuf.to_le_bytes());
                outbuf = 0;
                outbits = 0;
            }
            node = root;
        } else {
            node = next;
        }
    }
    out.truncate(total);
    out
}

/// Decompress `src` with `swi_number` (r2 = 0), returning `out_len` bytes.
fn decompress(swi_number: u8, src: &[u8], out_len: usize) -> Vec<u8> {
    let mut sys = booted(gba::default_bios());
    put_ewram_bytes(&mut sys, 0, src);
    invoke_swi3(&mut sys, swi_number, SRC, DST, 0);
    assert!(run_until_swi_returns(&mut sys, 100_000), "decompress SWI {swi_number:#x} hung");
    ewram_bytes(&sys, DST_OFF, out_len)
}

#[test]
fn lz77_decompresses_literals_and_back_references() {
    // "AB" then a back-reference producing "ABABABAB".
    let src = [0x10, 0x08, 0, 0, 0x20, 0x41, 0x42, 0x30, 0x01];
    let expected = ref_lz77(&src);
    assert_eq!(expected, b"ABABABAB");
    assert_eq!(decompress(0x11, &src, expected.len()), expected); // Wram
    assert_eq!(decompress(0x12, &src, expected.len()), expected); // Vram
}

#[test]
fn rle_decompresses_runs_and_literals() {
    // 0x82: compressed run of 5 × 0xAA; 0x02: literal 1,2,3.
    let src = [0x30, 0x08, 0, 0, 0x82, 0xAA, 0x02, 0x01, 0x02, 0x03];
    let expected = ref_rle(&src);
    assert_eq!(expected, vec![0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 1, 2, 3]);
    assert_eq!(decompress(0x14, &src, expected.len()), expected); // Wram
    assert_eq!(decompress(0x15, &src, expected.len()), expected); // Vram
}

#[test]
fn diff8_unfilters_a_delta_stream() {
    // Header size 6, then Data0=10 and +1 differences.
    let src = [0x81, 0x06, 0, 0, 10, 1, 1, 1, 1, 1];
    let expected = ref_diff8(&src);
    assert_eq!(expected, vec![10, 11, 12, 13, 14, 15]);
    assert_eq!(decompress(0x16, &src, expected.len()), expected); // Wram
    assert_eq!(decompress(0x17, &src, expected.len()), expected); // Vram
}

#[test]
fn diff16_unfilters_a_16bit_delta_stream() {
    // Header size 8 bytes (4 halfwords); Data0=100, +5, -2, +7.
    let src = [0x82, 0x08, 0, 0, 100, 0, 5, 0, 0xFE, 0xFF, 7, 0];
    let expected = ref_diff16(&src);
    assert_eq!(expected, vec![100, 105, 103, 110]);
    let mut sys = booted(gba::default_bios());
    put_ewram_bytes(&mut sys, 0, &src);
    invoke_swi3(&mut sys, 0x18, SRC, DST, 0);
    assert!(run_until_swi_returns(&mut sys, 100_000));
    assert_eq!(ewram_halfs(&sys, DST_OFF, expected.len()), expected);
}

fn run_bit_unpack(src: &[u8], srcw: u8, dstw: u8, offset_word: u32) -> Vec<u32> {
    let mut sys = booted(gba::default_bios());
    put_ewram_bytes(&mut sys, 0, src);
    // UnPack info block at ewram 0x800: u16 len, u8 srcw, u8 dstw, u32 offset.
    let mut info = Vec::new();
    info.extend_from_slice(&(src.len() as u16).to_le_bytes());
    info.push(srcw);
    info.push(dstw);
    info.extend_from_slice(&offset_word.to_le_bytes());
    put_ewram_bytes(&mut sys, 0x800, &info);
    invoke_swi3(&mut sys, 0x10, SRC, DST, EWRAM_BASE + 0x800);
    assert!(run_until_swi_returns(&mut sys, 100_000));
    let out_words = ref_bit_unpack(src, srcw as u32, dstw as u32, offset_word & 0x7FFF_FFFF, offset_word >> 31 != 0).len();
    (0..out_words).map(|i| ewram_word(&sys, DST_OFF + i * 4)).collect()
}

#[test]
fn huffman_decompresses_a_tree_and_bitstream() {
    // The GBATEK "Huff" example: root.0 -> data 'f'; root.1 -> a child whose
    // node0/node1 are data 'H'/'u'. Bits (MSB first): H=10, u=11, f=0, f=0. The
    // tree region is padded to 8 bytes so the bitstream word is 4-aligned.
    let src = [
        0x28, 0x04, 0, 0, // header: datasize 8, type 2, size 4
        0x03, // tree-size byte -> bitstream at src + (3+1)*2 = src+8..
        0x80, // root: offset 0, node0 is data
        0x66, // 'f'
        0xC0, // child: offset 0, node0 and node1 are data
        0x48, // 'H'
        0x75, // 'u'
        0, 0, // padding to word-align the bitstream
        0x00, 0x00, 0x00, 0xB0, // bitstream word 0xB0000000 = 1,0,1,1,0,0...
    ];
    let expected = ref_huff(&src);
    assert_eq!(expected, b"Huff");
    assert_eq!(decompress(0x13, &src, expected.len()), expected);
}

#[test]
fn bit_unpack_widens_1bit_units_to_bytes() {
    let src = [0xB1u8]; // bits LSB-first: 1,0,0,0,1,1,0,1
    let want = ref_bit_unpack(&src, 1, 8, 0, false);
    assert_eq!(want, vec![0x0000_0001, 0x0100_0101]);
    assert_eq!(run_bit_unpack(&src, 1, 8, 0), want);
}

#[test]
fn bit_unpack_applies_offset_and_zero_flag() {
    let src = [0xB1u8];
    // offset 0x30, zero-data flag set (bit31): every unit, including zeros, +0x30.
    let offset_word = 0x30 | (1 << 31);
    let want = ref_bit_unpack(&src, 1, 8, 0x30, true);
    assert_eq!(run_bit_unpack(&src, 1, 8, offset_word), want);
    // 4-bit source units widened to 8-bit with an offset, zero flag clear.
    let src4 = [0x21u8, 0x43];
    let want4 = ref_bit_unpack(&src4, 4, 8, 0x10, false);
    assert_eq!(run_bit_unpack(&src4, 4, 8, 0x10), want4);
}

// --- BgAffineSet (0x0E) / ObjAffineSet (0x0F) ------------------------------
//
// Reference uses the same Q14 sine table and integer formulas as the BIOS, so
// the comparison is exact. (Like ArcTan, the values are an independent
// approximation and are not compared against a real BIOS's own sine table.)

fn sintab_q14(i: usize) -> i32 {
    ((2.0 * std::f64::consts::PI * (i & 255) as f64 / 256.0).sin() * 16384.0).round() as i32
}

/// The affine matrix PA/PB/PC/PD (untruncated 8.8, as the BIOS keeps them for
/// the start-coordinate math; the stored halfwords are these truncated to i16).
fn affine_matrix(sx: i16, sy: i16, angle: u16) -> (i32, i32, i32, i32) {
    let idx = (angle >> 8) as usize;
    let (cos, sin) = (sintab_q14(idx + 64), sintab_q14(idx));
    (
        (sx as i32 * cos) >> 14,
        -((sx as i32 * sin) >> 14),
        (sy as i32 * sin) >> 14,
        (sy as i32 * cos) >> 14,
    )
}

fn ewram_i32(sys: &System, off: usize) -> i32 {
    ewram_word(sys, off) as i32
}

#[test]
fn obj_affine_set_computes_the_matrix() {
    let cases: [(i16, i16, u16); 6] = [
        (0x100, 0x100, 0x0000), // identity
        (0x100, 0x100, 0x4000), // 90 degrees
        (0x100, 0x100, 0x2000), // 45 degrees
        (0x100, 0x80, 0x8000),  // 180, sy = 0.5
        (0x80, 0x100, 0xC000),  // 270, sx = 0.5
        (0x100, 0x100, 0x1234), // arbitrary (upper 8 bits = 0x12)
    ];
    for &(sx, sy, angle) in &cases {
        for &offset in &[2u32, 8] {
            let mut sys = booted(gba::default_bios());
            let mut src = Vec::new();
            src.extend_from_slice(&sx.to_le_bytes());
            src.extend_from_slice(&sy.to_le_bytes());
            src.extend_from_slice(&angle.to_le_bytes());
            put_ewram_bytes(&mut sys, 0, &src);
            invoke_swi4(&mut sys, 0x0F, SRC, DST, 1, offset); // r2 = count, r3 = stride
            assert!(run_until_swi_returns(&mut sys, 5000));
            let (pa, pb, pc, pd) = affine_matrix(sx, sy, angle);
            let o = offset as usize;
            let got = |k: usize| ewram_half(&sys, DST_OFF + k * o) as i16;
            assert_eq!((got(0), got(1), got(2), got(3)), (pa as i16, pb as i16, pc as i16, pd as i16), "angle {angle:#06x} offset {offset}");
        }
    }
}

#[test]
fn bg_affine_set_computes_matrix_and_start() {
    let (cx, cy) = (120i32 << 8, 80i32 << 8); // centre in 24.8
    let (scrx, scry) = (120i16, 80i16);
    let cases: [(i16, i16, u16); 4] =
        [(0x100, 0x100, 0), (0x100, 0x100, 0x4000), (0x100, 0x80, 0x2000), (0x200, 0x100, 0x1234)];
    for &(sx, sy, angle) in &cases {
        let mut sys = booted(gba::default_bios());
        let mut src = Vec::new();
        src.extend_from_slice(&cx.to_le_bytes());
        src.extend_from_slice(&cy.to_le_bytes());
        src.extend_from_slice(&scrx.to_le_bytes());
        src.extend_from_slice(&scry.to_le_bytes());
        src.extend_from_slice(&sx.to_le_bytes());
        src.extend_from_slice(&sy.to_le_bytes());
        src.extend_from_slice(&angle.to_le_bytes());
        put_ewram_bytes(&mut sys, 0, &src);
        invoke_swi3(&mut sys, 0x0E, SRC, DST, 1);
        assert!(run_until_swi_returns(&mut sys, 5000));

        let (pa, pb, pc, pd) = affine_matrix(sx, sy, angle);
        assert_eq!(ewram_half(&sys, DST_OFF) as i16, pa as i16, "PA angle {angle:#06x}");
        assert_eq!(ewram_half(&sys, DST_OFF + 2) as i16, pb as i16, "PB");
        assert_eq!(ewram_half(&sys, DST_OFF + 4) as i16, pc as i16, "PC");
        assert_eq!(ewram_half(&sys, DST_OFF + 6) as i16, pd as i16, "PD");
        // startx = cx - PA*scrx - PB*scry ; starty = cy - PC*scrx - PD*scry
        let startx = cx - pa * scrx as i32 - pb * scry as i32;
        let starty = cy - pc * scrx as i32 - pd * scry as i32;
        assert_eq!(ewram_i32(&sys, DST_OFF + 8), startx, "startx angle {angle:#06x}");
        assert_eq!(ewram_i32(&sys, DST_OFF + 12), starty, "starty angle {angle:#06x}");
    }
}

// --- SoftReset (0x00) / RegisterRamReset (0x01) ----------------------------

#[test]
fn soft_reset_returns_to_rom_and_clears_state() {
    let mut sys = booted(gba::default_bios());
    // Dirty the BIOS RAM area and the general registers; flag 0 -> return to ROM.
    sys.gba.bus.memory.iwram[0x7E00] = 0xAA;
    sys.gba.bus.memory.iwram[0x7FFC] = 0xBB;
    sys.gba.bus.memory.iwram[0x7FFA] = 0x00;
    for i in 0..13 {
        sys.cpu.set_register(i, 0xDEAD_0000 + i as u32);
    }
    put_iwram(&mut sys, 0x0300_0000, &[0xEF00_0000, 0xEAFF_FFFE]); // swi #0 ; b .
    sys.cpu.set_pc(0x0300_0000);
    let mut reached = false;
    for _ in 0..2000 {
        if in_rom(sys.cpu.register(15)) {
            reached = true;
            break;
        }
        sys.step();
    }
    assert!(reached, "SoftReset did not jump to the ROM entry");
    assert_eq!(sys.cpu.mode(), Some(Mode::System));
    assert_eq!(sys.cpu.register(13), SP_USR, "SP_usr");
    for i in 0..13 {
        assert_eq!(sys.cpu.register(i), 0, "r{i} zeroed");
    }
    assert_eq!(sys.gba.bus.memory.iwram[0x7E00], 0, "BIOS RAM cleared");
    assert_eq!(sys.gba.bus.memory.iwram[0x7FFC], 0, "BIOS RAM cleared");
}

#[test]
fn soft_reset_to_ram_enters_at_ewram() {
    let mut sys = booted(gba::default_bios());
    put_ewram(&mut sys, 0, &[0xEAFF_FFFE]); // b . at 0x02000000
    sys.gba.bus.memory.iwram[0x7FFA] = 0x01; // flag non-zero -> return to RAM
    put_iwram(&mut sys, 0x0300_0000, &[0xEF00_0000, 0xEAFF_FFFE]);
    sys.cpu.set_pc(0x0300_0000);
    let mut reached = false;
    for _ in 0..2000 {
        if sys.cpu.register(15) == 0x0200_0000 {
            reached = true;
            break;
        }
        sys.step();
    }
    assert!(reached, "SoftReset did not enter RAM");
    assert_eq!(sys.cpu.mode(), Some(Mode::System));
}

#[test]
fn register_ram_reset_clears_ewram_and_forces_blank() {
    let mut sys = booted(gba::default_bios());
    put_ewram(&mut sys, 0, &[0xDEAD_BEEF; 8]);
    put_ewram(&mut sys, 0x3_FFFC, &[0x1234_5678]); // last word of the 256K region
    sys.gba.bus.io.video.write_dispcnt(0x1234);
    invoke_swi(&mut sys, 0x01, 0x01, 0); // r0 = flags: clear on-board WRAM
    assert!(run_until_swi_returns(&mut sys, 500_000));
    assert_eq!(ewram_word(&sys, 0), 0);
    assert_eq!(ewram_word(&sys, 0x3_FFFC), 0);
    assert_eq!(sys.gba.bus.io.video.read_dispcnt(), 0x0080, "forced blank");
}

#[test]
fn register_ram_reset_clears_iwram_but_preserves_the_last_512_bytes() {
    let mut sys = booted(gba::default_bios());
    // Run the invoking stub from EWRAM so clearing IWRAM does not erase it.
    sys.gba.bus.memory.iwram[0x0100] = 0xAA; // cleared
    sys.gba.bus.memory.iwram[0x7F00] = 0xBB; // in the excluded last 0x200 bytes
    put_ewram(&mut sys, 0x100, &[0xEF01_0000, 0xEAFF_FFFE]); // swi #1 ; b .
    sys.cpu.set_register(0, 0x02); // clear on-chip WRAM
    sys.cpu.set_pc(0x0200_0100);
    let mut returned = false;
    for _ in 0..500_000 {
        if sys.cpu.register(15) == 0x0200_0104 {
            returned = true;
            break;
        }
        sys.step();
    }
    assert!(returned, "RegisterRamReset did not return");
    assert_eq!(sys.gba.bus.memory.iwram[0x0100], 0, "IWRAM cleared");
    assert_eq!(sys.gba.bus.memory.iwram[0x7F00], 0xBB, "last 0x200 bytes preserved");
}

#[test]
fn register_ram_reset_clears_palette_vram_and_oam() {
    let mut sys = booted(gba::default_bios());
    sys.gba.bus.memory.palette[0] = 0xFF;
    sys.gba.bus.memory.palette[0x3FF] = 0xFF;
    sys.gba.bus.memory.vram[0] = 0xFF;
    sys.gba.bus.memory.vram[0x17FFF] = 0xFF;
    sys.gba.bus.memory.oam[0] = 0xFF;
    sys.gba.bus.memory.oam[0x3FF] = 0xFF;
    invoke_swi(&mut sys, 0x01, 0x04 | 0x08 | 0x10, 0); // palette + VRAM + OAM
    assert!(run_until_swi_returns(&mut sys, 500_000));
    assert!(sys.gba.bus.memory.palette.iter().all(|&b| b == 0), "palette cleared");
    assert!(sys.gba.bus.memory.vram.iter().all(|&b| b == 0), "VRAM cleared");
    assert!(sys.gba.bus.memory.oam.iter().all(|&b| b == 0), "OAM cleared");
}

#[test]
fn sound_bias_sets_level_and_preserves_upper_bits() {
    let mut sys = booted(gba::default_bios());
    // Upper bits (amplitude resolution) set, plus an arbitrary current level.
    sys.gba.bus.io.apu.write16(0x088, 0xC155, 0xFFFF);
    invoke_swi(&mut sys, 0x19, 1, 0); // non-zero -> level 0x200
    assert!(run_until_swi_returns(&mut sys, 5000));
    assert_eq!(sys.gba.bus.io.apu.read16(0x088), 0xC200);

    sys.gba.bus.io.apu.write16(0x088, 0xC155, 0xFFFF);
    invoke_swi(&mut sys, 0x19, 0, 0); // zero -> level 0x000
    assert!(run_until_swi_returns(&mut sys, 5000));
    assert_eq!(sys.gba.bus.io.apu.read16(0x088), 0xC000);
}

#[test]
fn get_bios_checksum_sums_the_image() {
    // GetBiosChecksum reads the BIOS in 32-bit units and adds them up. It runs
    // inside the BIOS, so the read-protection returns the real bytes. We return
    // our own image's checksum (a real BIOS would give 0xBAAE187F).
    let expected = gba::default_bios()
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .fold(0u32, |a, w| a.wrapping_add(w));
    let mut sys = booted(gba::default_bios());
    invoke_swi(&mut sys, 0x0D, 0, 0);
    // ~4096 iterations, so give the loop more than run_swi's small budget.
    assert!(run_until_swi_returns(&mut sys, 100_000), "GetBiosChecksum did not return");
    assert_eq!(sys.cpu.register(0), expected);
}

// --- Opt-in: equivalence against a real BIOS -------------------------------

fn real_bios() -> Option<Vec<u8>> {
    std::env::var("REWINDS_BIOS").ok().map(|p| std::fs::read(p).expect("read REWINDS_BIOS"))
}

fn real_rom() -> Option<Vec<u8>> {
    std::env::var("REWINDS_ROM").ok().map(|p| std::fs::read(p).expect("read REWINDS_ROM"))
}

#[test]
fn div_matches_real_bios() {
    let (Some(real), Some(rom)) = (real_bios(), real_rom()) else {
        eprintln!("skipping: set REWINDS_BIOS and REWINDS_ROM to run this");
        return;
    };
    // A real BIOS only hands off from a valid ROM; boot both through it, then run
    // the SWI from our own stub (the game code the ROM contains is irrelevant).
    let cases: &[(i32, i32)] = &[(-1234, 10), (100, 7), (-100, 7), (100, -7), (12345, 678)];
    for &(num, den) in cases {
        let mut ours = booted_with_rom(gba::default_bios(), rom.clone());
        let mut theirs = booted_with_rom(&real, rom.clone());
        let a = run_swi_on(&mut ours, 0x06, num as u32, den as u32);
        let b = run_swi_on(&mut theirs, 0x06, num as u32, den as u32);
        assert_eq!(a, b, "Div {num}/{den}: ours {a:?} vs real {b:?}");
    }
}

#[test]
fn cpu_set_matches_real_bios() {
    let (Some(real), Some(rom)) = (real_bios(), real_rom()) else {
        eprintln!("skipping: set REWINDS_BIOS and REWINDS_ROM to run this");
        return;
    };
    // Compare a 32-bit CpuSet copy's result in memory between the two BIOSes.
    let src = [0x0BAD_F00Du32, 0x1234_5678, 0xFEED_FACE, 0x0000_0001, 0xFFFF_FFFF];
    let copy = |bios: &[u8]| -> Vec<u32> {
        let mut sys = booted_with_rom(bios, rom.clone());
        put_ewram(&mut sys, 0, &src);
        put_ewram(&mut sys, DST_OFF, &[0; 5]);
        invoke_swi3(&mut sys, 0x0B, SRC, DST, 5 | CPUSET_32BIT);
        assert!(run_until_swi_returns(&mut sys, 5000));
        (0..5).map(|i| ewram_word(&sys, DST_OFF + i * 4)).collect()
    };
    assert_eq!(copy(gba::default_bios()), copy(&real));
    assert_eq!(copy(gba::default_bios()), src.to_vec());
}

#[test]
fn decompressors_match_real_bios() {
    let (Some(real), Some(rom)) = (real_bios(), real_rom()) else {
        eprintln!("skipping: set REWINDS_BIOS and REWINDS_ROM to run this");
        return;
    };
    // Every decompressor is an exact algorithm, so it must match bit-for-bit.
    let lz77 = vec![0x10u8, 0x08, 0, 0, 0x20, 0x41, 0x42, 0x30, 0x01];
    let rle = vec![0x30u8, 0x08, 0, 0, 0x82, 0xAA, 0x02, 0x01, 0x02, 0x03];
    let huff = vec![
        0x28u8, 0x04, 0, 0, 0x03, 0x80, 0x66, 0xC0, 0x48, 0x75, 0, 0, 0x00, 0x00, 0x00, 0xB0,
    ];
    let cases: &[(u8, &[u8], usize)] = &[(0x11, &lz77, 8), (0x14, &rle, 8), (0x13, &huff, 4)];

    let run = |bios: &[u8], swi: u8, src: &[u8], len: usize| -> Vec<u8> {
        let mut sys = booted_with_rom(bios, rom.clone());
        put_ewram_bytes(&mut sys, 0, src);
        invoke_swi3(&mut sys, swi, SRC, DST, 0);
        assert!(run_until_swi_returns(&mut sys, 100_000));
        ewram_bytes(&sys, DST_OFF, len)
    };
    for &(swi, src, len) in cases {
        assert_eq!(run(gba::default_bios(), swi, src, len), run(&real, swi, src, len), "SWI {swi:#x}");
    }
}

#[test]
fn sqrt_matches_real_bios() {
    let (Some(real), Some(rom)) = (real_bios(), real_rom()) else {
        eprintln!("skipping: set REWINDS_BIOS and REWINDS_ROM to run this");
        return;
    };
    // Sqrt is an exact integer function, so it can match bit-for-bit. (ArcTan and
    // ArcTan2 are deliberately independent approximations — see the always-run
    // tests — so they are not compared against the real BIOS here.)
    let cases = [0u32, 2, 15, 16, 255, 65535, 0xDEAD_BEEF, 0xFFFF_FFFF];
    for n in cases {
        let mut ours = booted_with_rom(gba::default_bios(), rom.clone());
        let mut theirs = booted_with_rom(&real, rom.clone());
        let a = run_swi_on(&mut ours, 0x08, n, 0).0;
        let b = run_swi_on(&mut theirs, 0x08, n, 0).0;
        assert_eq!(a, b, "sqrt({n}): ours {a} vs real {b}");
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
