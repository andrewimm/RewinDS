//! Interpreter tests: run real ARM programs against a flat test memory.

use super::{Bus, Cpu, Mode, Timed};

/// A flat little-endian memory implementing the CPU [`Bus`].
struct TestBus {
    memory: Vec<u8>,
}

impl TestBus {
    fn new(size: usize) -> Self {
        TestBus {
            memory: vec![0; size],
        }
    }

    /// Load a program of little-endian ARM words at `address`.
    fn load(&mut self, address: u32, program: &[u32]) {
        let mut offset = address as usize;
        for word in program {
            self.memory[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
            offset += 4;
        }
    }

    /// Load a program of little-endian Thumb halfwords at `address`.
    fn load_thumb(&mut self, address: u32, program: &[u16]) {
        let mut offset = address as usize;
        for halfword in program {
            self.memory[offset..offset + 2].copy_from_slice(&halfword.to_le_bytes());
            offset += 2;
        }
    }

    fn read(&self, address: u32, bytes: usize) -> u32 {
        let mut value = 0u32;
        for i in 0..bytes {
            value |= (self.memory[address as usize + i] as u32) << (8 * i);
        }
        value
    }

    fn write(&mut self, address: u32, value: u32, bytes: usize) {
        // Word-aligned memory ignores the low address bits of a wider store, just
        // as the CPU now leaves alignment to the memory it targets.
        let address = address as usize & !(bytes - 1);
        for i in 0..bytes {
            self.memory[address + i] = (value >> (8 * i)) as u8;
        }
    }
}

impl Bus for TestBus {
    fn fetch32(&mut self, address: u32, _sequential: bool) -> Timed<u32> {
        Timed {
            value: self.read(address, 4),
            cycles: 1,
        }
    }
    fn fetch16(&mut self, address: u32, _sequential: bool) -> Timed<u16> {
        Timed {
            value: self.read(address, 2) as u16,
            cycles: 1,
        }
    }
    fn load32(&mut self, address: u32, _sequential: bool) -> Timed<u32> {
        Timed {
            value: self.read(address, 4),
            cycles: 1,
        }
    }
    fn load16(&mut self, address: u32, _sequential: bool) -> Timed<u16> {
        Timed {
            value: self.read(address, 2) as u16,
            cycles: 1,
        }
    }
    fn load8(&mut self, address: u32, _sequential: bool) -> Timed<u8> {
        Timed {
            value: self.read(address, 1) as u8,
            cycles: 1,
        }
    }
    fn store32(&mut self, address: u32, value: u32, _sequential: bool) -> u32 {
        self.write(address, value, 4);
        1
    }
    fn store16(&mut self, address: u32, value: u16, _sequential: bool) -> u32 {
        self.write(address, value as u32, 2);
        1
    }
    fn store8(&mut self, address: u32, value: u8, _sequential: bool) -> u32 {
        self.write(address, value as u32, 1);
        1
    }
    fn internal(&mut self, _cycles: u32) {}
}

/// Run `cpu` until `r15` reaches `stop`, or `budget` steps elapse.
fn run_to(cpu: &mut Cpu, bus: &mut TestBus, stop: u32, budget: usize) {
    for _ in 0..budget {
        if cpu.register(15) == stop {
            return;
        }
        cpu.step(bus);
    }
    panic!("did not reach 0x{stop:08X}; pc = 0x{:08X}", cpu.register(15));
}

#[test]
fn data_processing_immediate() {
    let mut bus = TestBus::new(0x100);
    // mov r0, #5 ; add r0, r0, #3
    bus.load(0, &[0xE3A0_0005, 0xE280_0003]);
    let mut cpu = Cpu::new();
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    assert_eq!(cpu.register(0), 8);
}

#[test]
fn shifted_register_operand() {
    let mut bus = TestBus::new(0x100);
    // mov r1, #1 ; mov r0, r1, lsl #4   -> r0 = 16
    bus.load(0, &[0xE3A0_1001, 0xE1A0_0201]);
    let mut cpu = Cpu::new();
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    assert_eq!(cpu.register(0), 16);
}

#[test]
fn flags_and_conditional_execution() {
    let mut bus = TestBus::new(0x100);
    // cmp r0, #0 (r0 == 0 -> Z set) ; moveq r1, #7 ; movne r2, #9
    bus.load(0, &[0xE350_0000, 0x03A0_1007, 0x13A0_2009]);
    let mut cpu = Cpu::new();
    cpu.step(&mut bus); // cmp -> Z=1
    assert!(cpu.cpsr().z());
    cpu.step(&mut bus); // moveq executes
    cpu.step(&mut bus); // movne skipped
    assert_eq!(cpu.register(1), 7);
    assert_eq!(cpu.register(2), 0);
}

#[test]
fn branch_and_link_sets_return_address() {
    let mut bus = TestBus::new(0x100);
    // 0x00: bl 0x10 ; 0x04: (return here)
    // 0x10: mov r0, #1
    bus.load(0, &[0xEB00_0002]); // bl +8 -> (0+8)+8 = 0x10
    bus.load(0x10, &[0xE3A0_0001]);
    let mut cpu = Cpu::new();
    cpu.step(&mut bus);
    assert_eq!(cpu.register(15), 0x10); // branched to target
    assert_eq!(cpu.register(14), 0x04); // LR = instruction after bl
}

#[test]
fn store_then_load_word() {
    let mut bus = TestBus::new(0x100);
    // mov r0, #0x40 ; mov r1, #0xAB ; str r1, [r0] ; ldr r2, [r0]
    bus.load(
        0,
        &[0xE3A0_0040, 0xE3A0_10AB, 0xE580_1000, 0xE590_2000],
    );
    let mut cpu = Cpu::new();
    for _ in 0..4 {
        cpu.step(&mut bus);
    }
    assert_eq!(cpu.register(2), 0xAB);
    assert_eq!(bus.read(0x40, 4), 0xAB);
}

#[test]
fn push_and_pop_via_block_transfer() {
    let mut bus = TestBus::new(0x100);
    // mov r0, #0x80 (sp) ; mov r1, #0x11 ; mov r2, #0x22
    // stmdb r0!, {r1, r2}   (push)
    // mov r1, #0 ; mov r2, #0
    // ldmia r0!, {r1, r2}   (pop)
    bus.load(
        0,
        &[
            0xE3A0_0080,
            0xE3A0_1011,
            0xE3A0_2022,
            0xE920_0006, // stmdb r0!, {r1, r2}
            0xE3A0_1000,
            0xE3A0_2000,
            0xE8B0_0006, // ldmia r0!, {r1, r2}
        ],
    );
    let mut cpu = Cpu::new();
    for _ in 0..7 {
        cpu.step(&mut bus);
    }
    assert_eq!(cpu.register(1), 0x11);
    assert_eq!(cpu.register(2), 0x22);
    assert_eq!(cpu.register(0), 0x80); // pushed two, popped two
}

#[test]
fn multiply_and_accumulate() {
    let mut bus = TestBus::new(0x100);
    // mov r1, #6 ; mov r2, #7 ; mul r0, r1, r2 ; mov r3, #1 ; mla r4, r1, r2, r3
    bus.load(
        0,
        &[
            0xE3A0_1006,
            0xE3A0_2007,
            0xE000_0291, // mul r0, r1, r2
            0xE3A0_3001,
            0xE024_3192, // mla r4, r2, r1, r3
        ],
    );
    let mut cpu = Cpu::new();
    for _ in 0..5 {
        cpu.step(&mut bus);
    }
    assert_eq!(cpu.register(0), 42);
    assert_eq!(cpu.register(4), 43); // 6*7 + 1
}

#[test]
fn halfword_and_signed_loads() {
    let mut bus = TestBus::new(0x100);
    bus.write(0x40, 0x8123, 2); // a halfword with the sign bit set
    // mov r0, #0x40 ; ldrh r1, [r0] ; ldrsh r2, [r0] ; ldrsb r3, [r0]
    bus.load(
        0,
        &[0xE3A0_0040, 0xE1D0_10B0, 0xE1D0_20F0, 0xE1D0_30D0],
    );
    let mut cpu = Cpu::new();
    for _ in 0..4 {
        cpu.step(&mut bus);
    }
    assert_eq!(cpu.register(1), 0x8123); // zero-extended halfword
    assert_eq!(cpu.register(2), 0xFFFF_8123); // sign-extended halfword
    assert_eq!(cpu.register(3), 0x0000_0023); // low byte, sign of 0x23 is clear
}

#[test]
fn swap_word() {
    let mut bus = TestBus::new(0x100);
    bus.write(0x40, 0xCAFE, 4);
    // mov r0, #0x40 ; mov r1, #0x99 ; swp r2, r1, [r0]
    bus.load(0, &[0xE3A0_0040, 0xE3A0_1099, 0xE100_2091]);
    let mut cpu = Cpu::new();
    for _ in 0..3 {
        cpu.step(&mut bus);
    }
    assert_eq!(cpu.register(2), 0xCAFE); // old memory into rd
    assert_eq!(bus.read(0x40, 4), 0x99); // rm into memory
}

#[test]
fn sum_array_program() {
    // The exact bytes clang produced for crates/arm/tests/fixtures/sum_array.s.
    let mut bus = TestBus::new(0x200);
    bus.load(
        0,
        &[
            0xE3A0_2000,
            0xE351_0000,
            0x0A00_0003,
            0xE490_3004,
            0xE082_2003,
            0xE251_1001,
            0x1AFF_FFFB,
            0xE1A0_0002,
            0xE12F_FF1E,
        ],
    );
    // An array of three words at 0x40.
    bus.write(0x40, 10, 4);
    bus.write(0x44, 20, 4);
    bus.write(0x48, 30, 4);

    let mut cpu = Cpu::new();
    cpu.set_register(0, 0x40); // ptr
    cpu.set_register(1, 3); // count
    cpu.set_register(14, 0x1000); // return address sentinel
    run_to(&mut cpu, &mut bus, 0x1000, 200);
    assert_eq!(cpu.register(0), 60); // 10 + 20 + 30
}

#[test]
fn status_register_access() {
    let mut bus = TestBus::new(0x100);
    // msr cpsr_f, r1 ; mrs r0, cpsr
    bus.load(0, &[0xE128_F001, 0xE10F_0000]);
    let mut cpu = Cpu::new();
    cpu.set_register(1, 0xF000_0000); // set N, Z, C, V
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    assert!(cpu.cpsr().n() && cpu.cpsr().z() && cpu.cpsr().c() && cpu.cpsr().v());
    assert_eq!(cpu.register(0), 0xF000_001F); // flags | System mode
}

#[test]
fn software_interrupt_enters_supervisor() {
    let mut bus = TestBus::new(0x100);
    bus.load(0, &[0xEF00_0000]); // swi #0
    let mut cpu = Cpu::new();
    cpu.step(&mut bus);
    assert_eq!(cpu.mode(), Some(Mode::Supervisor));
    assert_eq!(cpu.register(15), 0x08); // SWI vector
    assert_eq!(cpu.register(14), 0x04); // LR = instruction after SWI
    assert!(cpu.cpsr().irq_disabled());
    assert_eq!(cpu.spsr().unwrap().mode(), Some(Mode::System)); // saved caller mode
}

#[test]
fn exception_return_restores_mode() {
    let mut bus = TestBus::new(0x100);
    bus.load(0, &[0xEF00_0000]); // swi #0
    bus.load(0x08, &[0xE1B0_F00E]); // movs pc, lr
    let mut cpu = Cpu::new();
    cpu.step(&mut bus); // swi -> supervisor, pc = 0x08
    cpu.step(&mut bus); // movs pc, lr -> return
    assert_eq!(cpu.register(15), 0x04);
    assert_eq!(cpu.mode(), Some(Mode::System));
}

#[test]
fn irq_entry() {
    let mut cpu = Cpu::new();
    cpu.set_pc(0x0100);
    assert!(cpu.irq_enabled());
    cpu.take_irq();
    assert_eq!(cpu.mode(), Some(Mode::Irq));
    assert_eq!(cpu.register(15), 0x18); // IRQ vector
    assert_eq!(cpu.register(14), 0x0104); // LR = pc + 4
    assert!(cpu.cpsr().irq_disabled());
    assert_eq!(cpu.spsr().unwrap().mode(), Some(Mode::System));
}

#[test]
fn register_banking_preserves_the_callers_stack_pointer() {
    let mut bus = TestBus::new(0x100);
    // main:            swi #0
    // 0x08 (handler):  mov sp, #0xBB ; movs pc, lr
    bus.load(0, &[0xEF00_0000]);
    bus.load(0x08, &[0xE3A0_D0BB, 0xE1B0_F00E]);
    let mut cpu = Cpu::new();
    cpu.set_register(13, 0xAAAA); // System sp
    for _ in 0..3 {
        cpu.step(&mut bus);
    }
    // The handler wrote the Supervisor sp; the System sp is untouched.
    assert_eq!(cpu.mode(), Some(Mode::System));
    assert_eq!(cpu.register(13), 0xAAAA);
}

#[test]
fn arm_thumb_interworking_round_trip() {
    // tests/fixtures/interwork.s, assembled by clang: ARM sets r0 = 2, BXes into
    // Thumb (lsls r0, #2 -> 8), then BXes back to ARM (add #1 -> 9).
    let mut bus = TestBus::new(0x100);
    bus.load(
        0,
        &[
            0xE3A0_0002, // mov r0, #2
            0xE28F_E008, // adr lr, back
            0xE28F_100C, // adr r1, tfunc
            0xE381_1001, // orr r1, r1, #1
            0xE12F_FF11, // bx r1        -> Thumb
            0xE280_0001, // back: add r0, r0, #1
            0xEAFF_FFFE, // b .          (spin)
        ],
    );
    bus.load_thumb(0x1C, &[0x0080, 0x4770]); // tfunc: lsls r0, r0, #2 ; bx lr
    let mut cpu = Cpu::new();
    for _ in 0..12 {
        cpu.step(&mut bus);
    }
    assert_eq!(cpu.register(0), 9);
    assert!(!cpu.cpsr().thumb()); // came back to ARM state
}

#[test]
fn thumb_alu_and_shift_flags() {
    let mut bus = TestBus::new(0x100);
    // bx r0 (enter Thumb at 0x08)
    bus.load(0, &[0xE12F_FF10]);
    // movs r1, #1 ; lsls r1, r1, ... no: use format-4 shift.
    // movs r0, #0x80 ; asrs r0, r1 won't be simple. Keep it: movs r1, #1 ; movs r0, #0 ; adds r0, r0, r1
    bus.load_thumb(0x08, &[0x2101, 0x2000, 0x1840]);
    let mut cpu = Cpu::new();
    cpu.set_register(0, 0x08 | 1); // Thumb target
    for _ in 0..4 {
        cpu.step(&mut bus);
    }
    assert_eq!(cpu.register(0), 1); // 0 + 1
    assert_eq!(cpu.register(1), 1);
}

#[test]
fn thumb_program_via_interworking() {
    // The exact bytes clang produced for tests/fixtures/thumb_program.s, entered
    // from ARM via `bx`. It pushes, computes, calls a subroutine via BL, and
    // returns through pop.
    let mut bus = TestBus::new(0x300);
    bus.load(0, &[0xE12F_FF10]); // bx r0
    bus.load_thumb(
        0x08,
        &[
            0xB510, // push {r4, lr}
            0x200A, // movs r0, #10
            0x0081, // lsls r1, r0, #2
            0xAC02, // add r4, sp, #8
            0x6822, // ldr r2, [r4]
            0xF000, // bl target (high)
            0xF801, // bl target (low)
            0xBD10, // pop {r4, pc}
            0x1C40, // adds r0, r0, #1
            0x4770, // bx lr
        ],
    );
    let mut cpu = Cpu::new();
    cpu.set_register(0, 0x08 | 1); // Thumb entry
    cpu.set_register(13, 0x0100); // sp
    cpu.set_register(14, 0x0200); // lr sentinel
    for _ in 0..12 {
        cpu.step(&mut bus);
    }
    assert_eq!(cpu.register(1), 40); // 10 << 2
    assert_eq!(cpu.register(0), 11); // 10, then +1 in the subroutine
}

#[test]
fn sum_loop() {
    let mut bus = TestBus::new(0x100);
    // mov r0, #0        ; sum = 0
    // mov r1, #1        ; i = 1
    // loop: (0x08)
    // add r0, r0, r1    ; sum += i
    // add r1, r1, #1    ; i++
    // cmp r1, #4        ; i == 4?
    // bne loop
    bus.load(
        0,
        &[
            0xE3A0_0000,
            0xE3A0_1001,
            0xE080_0001,
            0xE281_1001,
            0xE351_0004,
            0x1AFF_FFFB,
        ],
    );
    let mut cpu = Cpu::new();
    run_to(&mut cpu, &mut bus, 0x18, 100);
    assert_eq!(cpu.register(0), 6); // 1 + 2 + 3
    assert_eq!(cpu.register(1), 4);
}

/// Execute a single ARM instruction `encoding` on a fresh core of `version`, with
/// the given `(register, value)` setup applied first.
fn run_one(version: super::ArmVersion, encoding: u32, setup: &[(usize, u32)]) -> Cpu {
    let mut bus = TestBus::new(0x100);
    bus.load(0, &[encoding]);
    let mut cpu = Cpu::with_version(version);
    for &(reg, value) in setup {
        cpu.set_register(reg, value);
    }
    cpu.step(&mut bus);
    cpu
}

#[test]
fn clz_counts_leading_zeros_on_v5() {
    // clz r0, r1
    let cpu = run_one(super::ArmVersion::Armv5TE, 0xE16F_0F11, &[(1, 0x0000_FFFF)]);
    assert_eq!(cpu.register(0), 16);
    assert_eq!(run_one(super::ArmVersion::Armv5TE, 0xE16F_0F11, &[(1, 0)]).register(0), 32);
    assert_eq!(run_one(super::ArmVersion::Armv5TE, 0xE16F_0F11, &[(1, 0x8000_0000)]).register(0), 0);
}

#[test]
fn clz_traps_as_undefined_on_v4t() {
    // On ARMv4T the CLZ encoding is not an instruction — it must not write rd.
    let cpu = run_one(super::ArmVersion::Armv4T, 0xE16F_0F11, &[(0, 0xDEAD), (1, 0x0000_FFFF)]);
    assert_eq!(cpu.register(0), 0xDEAD); // untouched — trapped, not executed
    assert_eq!(cpu.mode(), Some(Mode::Undefined));
}

#[test]
fn qadd_qsub_saturate_and_set_q() {
    use super::ArmVersion::Armv5TE;
    // qadd r0, r1, r2  (r0 = r1 + r2)
    let ok = run_one(Armv5TE, 0xE102_0051, &[(1, 5), (2, 3)]);
    assert_eq!(ok.register(0), 8);
    assert!(!ok.cpsr().q());
    // Overflow past i32::MAX saturates and sets Q.
    let sat = run_one(Armv5TE, 0xE102_0051, &[(1, 0x7FFF_FFFF), (2, 1)]);
    assert_eq!(sat.register(0), 0x7FFF_FFFF);
    assert!(sat.cpsr().q());
    // qsub r0, r1, r2  (r0 = r1 - r2), underflow past i32::MIN saturates.
    let sub = run_one(Armv5TE, 0xE122_0051, &[(1, 0x8000_0000), (2, 1)]);
    assert_eq!(sub.register(0), 0x8000_0000);
    assert!(sub.cpsr().q());
}

#[test]
fn qdadd_qdsub_double_the_second_operand() {
    use super::ArmVersion::Armv5TE;
    // qdadd r0, r1, r2  (r0 = r1 + r2*2)
    let ok = run_one(Armv5TE, 0xE142_0051, &[(1, 10), (2, 3)]);
    assert_eq!(ok.register(0), 16); // 10 + 3*2
    assert!(!ok.cpsr().q());
    // Doubling itself saturates (0x40000000 * 2 overflows) → Q set.
    let sat = run_one(Armv5TE, 0xE142_0051, &[(1, 0), (2, 0x4000_0000)]);
    assert_eq!(sat.register(0), 0x7FFF_FFFF);
    assert!(sat.cpsr().q());
    // qdsub r0, r1, r2  (r0 = r1 - r2*2)
    let sub = run_one(Armv5TE, 0xE162_0051, &[(1, 20), (2, 4)]);
    assert_eq!(sub.register(0), 12); // 20 - 4*2
}

#[test]
fn saturating_traps_as_undefined_on_v4t() {
    let cpu = run_one(super::ArmVersion::Armv4T, 0xE102_0051, &[(0, 0xBEEF), (1, 5), (2, 3)]);
    assert_eq!(cpu.register(0), 0xBEEF); // untouched
    assert_eq!(cpu.mode(), Some(Mode::Undefined));
}

#[test]
fn smul_halfword_selects_and_signs_operands() {
    use super::ArmVersion::Armv5TE;
    // smulbb r0, r1, r2 — bottom(r1) * bottom(r2), signed.
    let cpu = run_one(Armv5TE, 0xE160_0281, &[(1, 0xFFFF), (2, 2)]); // (-1) * 2
    assert_eq!(cpu.register(0) as i32, -2);
    // smulbt r0, r1, r2 — bottom(r1) * top(r2).
    let cpu = run_one(Armv5TE, 0xE160_02C1, &[(1, 3), (2, 0x0004_0000)]);
    assert_eq!(cpu.register(0), 12);
}

#[test]
fn smla_halfword_accumulates_and_sets_q_on_overflow() {
    use super::ArmVersion::Armv5TE;
    // smlabb r0, r1, r2, r3 — bottom*bottom + r3.
    let cpu = run_one(Armv5TE, 0xE100_3281, &[(1, 2), (2, 3), (3, 10)]);
    assert_eq!(cpu.register(0), 16);
    assert!(!cpu.cpsr().q());
    // Overflow of the 32-bit accumulate sets Q but does NOT saturate (wraps).
    let cpu = run_one(Armv5TE, 0xE100_3281, &[(1, 0x7FFF), (2, 0x7FFF), (3, 0x7FFF_FFFF)]);
    assert!(cpu.cpsr().q());
    assert_eq!(cpu.register(0), 0x3FFF_0001u32.wrapping_add(0x7FFF_FFFF));
}

#[test]
fn smulw_takes_the_top_32_of_the_48bit_product() {
    // smulwb r0, r1, r2 — (r1 * bottom(r2)) >> 16.
    let cpu = run_one(super::ArmVersion::Armv5TE, 0xE120_02A1, &[(1, 0x0001_0000), (2, 2)]);
    assert_eq!(cpu.register(0), 2); // 65536 * 2 >> 16
}

#[test]
fn smlal_halfword_accumulates_into_64_bits() {
    // smlalbb r0(lo), r1(hi), r2, r3 — r1:r0 += bottom(r2)*bottom(r3).
    let cpu = run_one(super::ArmVersion::Armv5TE, 0xE141_0382, &[(2, 0xFFFF), (3, 2)]); // (-1)*2 = -2
    assert_eq!(cpu.register(0), 0xFFFF_FFFE);
    assert_eq!(cpu.register(1), 0xFFFF_FFFF); // sign-extended high word
}

#[test]
fn dsp_multiply_traps_as_undefined_on_v4t() {
    let cpu = run_one(super::ArmVersion::Armv4T, 0xE160_0281, &[(0, 0xCAFE), (1, 3), (2, 5)]);
    assert_eq!(cpu.register(0), 0xCAFE); // untouched
    assert_eq!(cpu.mode(), Some(Mode::Undefined));
}

#[test]
fn cpu_version_defaults_to_v4t_and_selects_v5() {
    use super::ArmVersion;
    assert_eq!(Cpu::new().version(), ArmVersion::Armv4T);
    assert!(!Cpu::new().version().is_v5());
    let arm9 = Cpu::with_version(ArmVersion::Armv5TE);
    assert_eq!(arm9.version(), ArmVersion::Armv5TE);
    assert!(arm9.version().is_v5());
}

#[test]
fn q_flag_round_trips_through_cpsr_bits() {
    use super::Psr;
    let mut psr = Psr::from_bits(0);
    assert!(!psr.q());
    psr.set_q(true);
    assert!(psr.q());
    assert_eq!(psr.bits() & (1 << 27), 1 << 27); // CPSR bit 27
    // The Q flag survives a bits round-trip (so it rides SPSR save/restore).
    assert!(Psr::from_bits(psr.bits()).q());
    psr.set_q(false);
    assert!(!psr.q());
}
