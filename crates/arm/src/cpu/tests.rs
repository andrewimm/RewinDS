//! Interpreter tests: run real ARM programs against a flat test memory.

use super::{Bus, Cpu, Timed};

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

    fn read(&self, address: u32, bytes: usize) -> u32 {
        let mut value = 0u32;
        for i in 0..bytes {
            value |= (self.memory[address as usize + i] as u32) << (8 * i);
        }
        value
    }

    fn write(&mut self, address: u32, value: u32, bytes: usize) {
        for i in 0..bytes {
            self.memory[address as usize + i] = (value >> (8 * i)) as u8;
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
