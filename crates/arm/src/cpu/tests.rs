//! Interpreter tests: run real ARM programs against a flat test memory.

use super::{Bus, Cpu, Mode, Timed};

/// A flat little-endian memory implementing the CPU [`Bus`].
struct TestBus {
    memory: Vec<u8>,
    /// A single fake coprocessor (number 15) for MRC/MCR tests: `MCR` stores its
    /// word here and `MRC` reads it back. `None` models a machine with no
    /// coprocessor, so the transfers trap as Undefined.
    coprocessor: Option<u32>,
}

impl TestBus {
    fn new(size: usize) -> Self {
        TestBus {
            memory: vec![0; size],
            coprocessor: None,
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

    fn coprocessor_write(
        &mut self,
        cp: u8,
        _opcode1: u8,
        _crn: u8,
        _crm: u8,
        _opcode2: u8,
        value: u32,
    ) -> bool {
        if cp == 15 {
            if let Some(slot) = self.coprocessor.as_mut() {
                *slot = value;
                return true;
            }
        }
        false
    }

    fn coprocessor_read(
        &mut self,
        cp: u8,
        _opcode1: u8,
        _crn: u8,
        _crm: u8,
        _opcode2: u8,
    ) -> Option<u32> {
        if cp == 15 {
            self.coprocessor
        } else {
            None
        }
    }
}

/// Run `cpu` until `r15` reaches `stop`, or `budget` steps elapse.
fn run_to(cpu: &mut Cpu, bus: &mut TestBus, stop: u32, budget: usize) {
    for _ in 0..budget {
        if cpu.register(15) == stop {
            return;
        }
        cpu.step(bus);
    }
    panic!(
        "did not reach 0x{stop:08X}; pc = 0x{:08X}",
        cpu.register(15)
    );
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
    bus.load(0, &[0xE3A0_0040, 0xE3A0_10AB, 0xE580_1000, 0xE590_2000]);
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
    bus.load(0, &[0xE3A0_0040, 0xE1D0_10B0, 0xE1D0_20F0, 0xE1D0_30D0]);
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
    assert_eq!(
        run_one(super::ArmVersion::Armv5TE, 0xE16F_0F11, &[(1, 0)]).register(0),
        32
    );
    assert_eq!(
        run_one(super::ArmVersion::Armv5TE, 0xE16F_0F11, &[(1, 0x8000_0000)]).register(0),
        0
    );
}

#[test]
fn clz_traps_as_undefined_on_v4t() {
    // On ARMv4T the CLZ encoding is not an instruction — it must not write rd.
    let cpu = run_one(
        super::ArmVersion::Armv4T,
        0xE16F_0F11,
        &[(0, 0xDEAD), (1, 0x0000_FFFF)],
    );
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
    let cpu = run_one(
        super::ArmVersion::Armv4T,
        0xE102_0051,
        &[(0, 0xBEEF), (1, 5), (2, 3)],
    );
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
    let cpu = run_one(
        Armv5TE,
        0xE100_3281,
        &[(1, 0x7FFF), (2, 0x7FFF), (3, 0x7FFF_FFFF)],
    );
    assert!(cpu.cpsr().q());
    assert_eq!(cpu.register(0), 0x3FFF_0001u32.wrapping_add(0x7FFF_FFFF));
}

#[test]
fn smulw_takes_the_top_32_of_the_48bit_product() {
    // smulwb r0, r1, r2 — (r1 * bottom(r2)) >> 16.
    let cpu = run_one(
        super::ArmVersion::Armv5TE,
        0xE120_02A1,
        &[(1, 0x0001_0000), (2, 2)],
    );
    assert_eq!(cpu.register(0), 2); // 65536 * 2 >> 16
}

#[test]
fn smlal_halfword_accumulates_into_64_bits() {
    // smlalbb r0(lo), r1(hi), r2, r3 — r1:r0 += bottom(r2)*bottom(r3).
    let cpu = run_one(
        super::ArmVersion::Armv5TE,
        0xE141_0382,
        &[(2, 0xFFFF), (3, 2)],
    ); // (-1)*2 = -2
    assert_eq!(cpu.register(0), 0xFFFF_FFFE);
    assert_eq!(cpu.register(1), 0xFFFF_FFFF); // sign-extended high word
}

#[test]
fn dsp_multiply_traps_as_undefined_on_v4t() {
    let cpu = run_one(
        super::ArmVersion::Armv4T,
        0xE160_0281,
        &[(0, 0xCAFE), (1, 3), (2, 5)],
    );
    assert_eq!(cpu.register(0), 0xCAFE); // untouched
    assert_eq!(cpu.mode(), Some(Mode::Undefined));
}

#[test]
fn blx_register_switches_state_and_links() {
    use super::ArmVersion::Armv5TE;
    // blx r2 — target bit0 selects Thumb; LR = instruction+4.
    let thumb = run_one(Armv5TE, 0xE12F_FF32, &[(2, 0x41)]);
    assert_eq!(thumb.register(15), 0x40);
    assert!(thumb.cpsr().thumb());
    assert_eq!(thumb.register(14), 4);
    let arm = run_one(Armv5TE, 0xE12F_FF32, &[(2, 0x80)]);
    assert_eq!(arm.register(15), 0x80);
    assert!(!arm.cpsr().thumb());
}

#[test]
fn blx_register_traps_on_v4t() {
    let cpu = run_one(super::ArmVersion::Armv4T, 0xE12F_FF32, &[(2, 0x40)]);
    assert_eq!(cpu.mode(), Some(Mode::Undefined));
    assert_ne!(cpu.register(15), 0x40); // did not branch
}

#[test]
fn blx_immediate_links_and_enters_thumb() {
    // blx #+ : offset 8, so PC = (0+8) + 8 = 0x10; always switches to Thumb.
    let cpu = run_one(super::ArmVersion::Armv5TE, 0xFA00_0002, &[]);
    assert_eq!(cpu.register(15), 0x10);
    assert!(cpu.cpsr().thumb());
    assert_eq!(cpu.register(14), 4);
}

#[test]
fn blx_immediate_is_nop_on_v4t() {
    // cond == 1111 is never-execute on ARMv4T: the instruction does nothing.
    let cpu = run_one(super::ArmVersion::Armv4T, 0xFA00_0002, &[(14, 0x999)]);
    assert_eq!(cpu.register(15), 4); // just advanced to the next instruction
    assert!(!cpu.cpsr().thumb());
    assert_eq!(cpu.register(14), 0x999); // LR untouched
}

#[test]
fn thumb_blx_register_links_and_exchanges() {
    use super::ArmVersion::Armv5TE;
    // Enter Thumb at 0x08, then `blx r1` (0x4788) to an ARM target at 0x40.
    // The distinguishing behavior from `bx` is that LR must be set.
    let mut bus = TestBus::new(0x100);
    bus.load(0, &[0xE12F_FF10]); // bx r0 -> Thumb at 0x08
    bus.load_thumb(0x08, &[0x4788]); // blx r1
    let mut cpu = Cpu::with_version(Armv5TE);
    cpu.set_register(0, 0x08 | 1); // Thumb entry
    cpu.set_register(1, 0x40); // ARM target (bit0 clear)
    cpu.step(&mut bus); // bx r0
    cpu.step(&mut bus); // blx r1
    assert_eq!(cpu.register(15), 0x40); // branched to the target
    assert!(!cpu.cpsr().thumb()); // exchanged to ARM
    assert_eq!(cpu.register(14), 0x0B); // LR = (0x08 + 2) | 1
}

#[test]
fn thumb_blx_register_stays_thumb_when_target_is_thumb() {
    use super::ArmVersion::Armv5TE;
    let mut bus = TestBus::new(0x100);
    bus.load(0, &[0xE12F_FF10]); // bx r0 -> Thumb at 0x08
    bus.load_thumb(0x08, &[0x4788]); // blx r1
    let mut cpu = Cpu::with_version(Armv5TE);
    cpu.set_register(0, 0x08 | 1);
    cpu.set_register(1, 0x41); // Thumb target (bit0 set)
    cpu.step(&mut bus); // bx r0
    cpu.step(&mut bus); // blx r1
    assert_eq!(cpu.register(15), 0x40);
    assert!(cpu.cpsr().thumb()); // stayed in Thumb
    assert_eq!(cpu.register(14), 0x0B);
}

#[test]
fn thumb_blx_register_traps_on_v4t() {
    // `BLX` register is undefined on the ARM7TDMI.
    let mut bus = TestBus::new(0x100);
    bus.load(0, &[0xE12F_FF10]); // bx r0 -> Thumb at 0x08
    bus.load_thumb(0x08, &[0x4788]); // blx r1 (undefined on v4t)
    let mut cpu = Cpu::with_version(super::ArmVersion::Armv4T);
    cpu.set_register(0, 0x08 | 1);
    cpu.set_register(1, 0x40);
    cpu.step(&mut bus); // bx r0
    cpu.step(&mut bus); // blx r1 -> undefined
    assert_eq!(cpu.mode(), Some(Mode::Undefined));
    assert_ne!(cpu.register(15), 0x40); // did not branch to the target
}

#[test]
fn bkpt_enters_abort_on_v5_and_traps_on_v4t() {
    let v5 = run_one(super::ArmVersion::Armv5TE, 0xE120_0070, &[]);
    assert_eq!(v5.mode(), Some(Mode::Abort));
    assert_eq!(v5.register(15), 0x0C); // prefetch-abort vector
    let v4 = run_one(super::ArmVersion::Armv4T, 0xE120_0070, &[]);
    assert_eq!(v4.mode(), Some(Mode::Undefined));
}

#[test]
fn thumb_blx_switches_back_to_arm() {
    // bx into Thumb at 0x100, run the BL/BLX pair, land back in ARM.
    let mut bus = TestBus::new(0x200);
    bus.load(0, &[0xE12F_FF10]); // bx r0
    bus.load_thumb(0x100, &[0xF000, 0xE800]); // first half + BLX second half
    let mut cpu = Cpu::with_version(super::ArmVersion::Armv5TE);
    cpu.set_register(0, 0x101); // Thumb, address 0x100
    run_to(&mut cpu, &mut bus, 0x104, 10);
    assert!(!cpu.cpsr().thumb()); // switched to ARM
    assert_eq!(cpu.register(15), 0x104);
}

#[test]
fn ldr_pc_interworks_on_v5_only() {
    // ldr pc, [r0]; the loaded value's bit 0 selects the instruction set on v5.
    let run = |version| {
        let mut bus = TestBus::new(0x300);
        bus.load(0, &[0xE590_F000]);
        bus.load(0x100, &[0x201]); // odd -> Thumb, target 0x200
        let mut cpu = Cpu::with_version(version);
        cpu.set_register(0, 0x100);
        cpu.step(&mut bus);
        cpu
    };
    let v5 = run(super::ArmVersion::Armv5TE);
    assert!(v5.cpsr().thumb()); // interworked to Thumb
    assert_eq!(v5.register(15) & !1, 0x200);
    let v4 = run(super::ArmVersion::Armv4T);
    assert!(!v4.cpsr().thumb()); // stayed ARM
}

#[test]
fn ldrd_strd_move_register_pairs_on_v5() {
    use super::ArmVersion::Armv5TE;
    // ldrd r0, [r2] — loads r0 from [r2] and r1 from [r2+4].
    let mut bus = TestBus::new(0x200);
    bus.load(0, &[0xE1C2_00D0]);
    bus.load(0x100, &[0xAAAA_BBBB, 0xCCCC_DDDD]);
    let mut cpu = Cpu::with_version(Armv5TE);
    cpu.set_register(2, 0x100);
    cpu.step(&mut bus);
    assert_eq!(cpu.register(0), 0xAAAA_BBBB);
    assert_eq!(cpu.register(1), 0xCCCC_DDDD);

    // strd r0, [r2] — stores r0 to [r2] and r1 to [r2+4].
    let mut bus = TestBus::new(0x200);
    bus.load(0, &[0xE1C2_00F0]);
    let mut cpu = Cpu::with_version(Armv5TE);
    cpu.set_register(2, 0x100);
    cpu.set_register(0, 0x1234_5678);
    cpu.set_register(1, 0x9ABC_DEF0);
    cpu.step(&mut bus);
    assert_eq!(bus.read(0x100, 4), 0x1234_5678);
    assert_eq!(bus.read(0x104, 4), 0x9ABC_DEF0);
}

#[test]
fn ldrd_preindexed_writeback_updates_base() {
    use super::ArmVersion::Armv5TE;
    // ldrd r4, [r2, #8]! — base advances by 8, loading r4/r5 from [r2+8]/[r2+12].
    let mut bus = TestBus::new(0x200);
    bus.load(0, &[0xE1E2_40D8]);
    bus.load(0x108, &[0x1111_2222, 0x3333_4444]);
    let mut cpu = Cpu::with_version(Armv5TE);
    cpu.set_register(2, 0x100);
    cpu.step(&mut bus);
    assert_eq!(cpu.register(4), 0x1111_2222);
    assert_eq!(cpu.register(5), 0x3333_4444);
    assert_eq!(cpu.register(2), 0x108); // writeback
}

#[test]
fn ldrd_traps_as_undefined_on_v4t() {
    // LDRD is not an instruction on ARMv4T — it must not write the pair.
    let mut bus = TestBus::new(0x200);
    bus.load(0, &[0xE1C2_00D0]);
    bus.load(0x100, &[0xAAAA_BBBB, 0xCCCC_DDDD]);
    let mut cpu = Cpu::with_version(super::ArmVersion::Armv4T);
    cpu.set_register(0, 0xDEAD);
    cpu.set_register(2, 0x100);
    cpu.step(&mut bus);
    assert_eq!(cpu.register(0), 0xDEAD); // untouched — trapped, not executed
    assert_eq!(cpu.mode(), Some(Mode::Undefined));
}

#[test]
fn mcr_mrc_move_words_through_a_coprocessor_on_v5() {
    use super::ArmVersion::Armv5TE;
    let mut bus = TestBus::new(0x100);
    bus.coprocessor = Some(0); // a coprocessor is present
                               // mcr p15, 0, r1, c1, c0, 0  then  mrc p15, 0, r2, c1, c0, 0
    bus.load(0, &[0xEE01_1F10, 0xEE11_2F10]);
    let mut cpu = Cpu::with_version(Armv5TE);
    cpu.set_register(1, 0xCAFE_F00D);
    cpu.step(&mut bus); // MCR: r1 -> coprocessor
    cpu.step(&mut bus); // MRC: coprocessor -> r2
    assert_eq!(bus.coprocessor, Some(0xCAFE_F00D));
    assert_eq!(cpu.register(2), 0xCAFE_F00D);
}

#[test]
fn coprocessor_transfer_traps_when_absent_or_on_v4t() {
    // No coprocessor present: MCR raises the Undefined Instruction trap.
    let mut bus = TestBus::new(0x100);
    bus.load(0, &[0xEE01_1F10]); // mcr p15, 0, r1, c1, c0, 0
    let mut cpu = Cpu::with_version(super::ArmVersion::Armv5TE);
    cpu.step(&mut bus);
    assert_eq!(cpu.mode(), Some(Mode::Undefined));

    // On ARMv4T the encoding is not an instruction at all, even with a
    // coprocessor wired up — it must trap rather than transfer.
    let mut bus = TestBus::new(0x100);
    bus.coprocessor = Some(0);
    bus.load(0, &[0xEE01_1F10]);
    let mut cpu = Cpu::new(); // v4T
    cpu.set_register(1, 0x1234);
    cpu.step(&mut bus);
    assert_eq!(cpu.mode(), Some(Mode::Undefined));
    assert_eq!(bus.coprocessor, Some(0)); // untouched — trapped, not transferred
}

#[test]
fn ldm_base_in_list_writeback_position_dependent() {
    // The armwrestler LDM cases: a base that appears in the list keeps its loaded
    // value, EXCEPT when it is the lowest register (loaded first, then overwritten
    // by the writeback). Memory: 0x11223344 @ 0x100, 0x55667788 @ 0x104.
    let make = |encoding: u32| {
        let mut bus = TestBus::new(0x200);
        bus.load(0, &[encoding]);
        bus.load(0x100, &[0x1122_3344, 0x5566_7788]);
        let mut cpu = Cpu::new();
        cpu.set_register(3, 0xFC); // base = 0x100 - 4 (LDMIB pre-increments to 0x100)
        cpu.step(&mut bus);
        cpu
    };
    // LDMIB r3!,{r3,r5}: base r3 is the lowest → writeback wins (0xFC + 2*4 = 0x104).
    let c = make(0xE9B3_0028);
    assert_eq!(c.register(3), 0x104);
    assert_eq!(c.register(5), 0x5566_7788);
    // LDMIB r3!,{r2,r3}: base r3 is not the lowest → the loaded value wins.
    let c = make(0xE9B3_000C);
    assert_eq!(c.register(2), 0x1122_3344);
    assert_eq!(c.register(3), 0x5566_7788);
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
