//! The memory interface the interpreter executes against.
//!
//! The `arm` crate knows nothing about any particular machine, so the CPU talks
//! to memory through this trait. A machine crate implements it — translating to
//! its real bus, wait states, and scheduler. Every access reports the guest
//! cycles it consumed so the CPU can track time; instruction fetches are
//! distinguished from data accesses (they may be prefetched) and each access
//! says whether it is sequential (cheaper on wait-stated regions).

/// A value read from the bus together with the cycles the access took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timed<T> {
    pub value: T,
    pub cycles: u32,
}

/// The memory the CPU executes against.
pub trait Bus {
    /// Fetch a 32-bit ARM opcode.
    fn fetch32(&mut self, address: u32, sequential: bool) -> Timed<u32>;
    /// Fetch a 16-bit Thumb opcode.
    fn fetch16(&mut self, address: u32, sequential: bool) -> Timed<u16>;

    fn load32(&mut self, address: u32, sequential: bool) -> Timed<u32>;
    fn load16(&mut self, address: u32, sequential: bool) -> Timed<u16>;
    fn load8(&mut self, address: u32, sequential: bool) -> Timed<u8>;

    /// Store a value; returns the cycles the access took.
    fn store32(&mut self, address: u32, value: u32, sequential: bool) -> u32;
    fn store16(&mut self, address: u32, value: u16, sequential: bool) -> u32;
    fn store8(&mut self, address: u32, value: u8, sequential: bool) -> u32;

    /// Consume `cycles` internal (non-bus) cycles — the CPU's I-cycles, during
    /// which a machine may advance a ROM prefetcher.
    fn internal(&mut self, cycles: u32);

    /// `MCR` — move `value` from an ARM register into the coprocessor register
    /// named by `cp`/`opcode1`/`crn`/`crm`/`opcode2`. Returns `true` if a
    /// coprocessor accepted the write; `false` raises the Undefined Instruction
    /// trap, as on hardware with no such coprocessor. The default has none.
    fn coprocessor_write(
        &mut self,
        _cp: u8,
        _opcode1: u8,
        _crn: u8,
        _crm: u8,
        _opcode2: u8,
        _value: u32,
    ) -> bool {
        false
    }

    /// `MRC` — read the coprocessor register named by the selectors into an ARM
    /// register. `None` raises the Undefined Instruction trap. The default core
    /// exposes no coprocessors.
    fn coprocessor_read(
        &mut self,
        _cp: u8,
        _opcode1: u8,
        _crn: u8,
        _crm: u8,
        _opcode2: u8,
    ) -> Option<u32> {
        None
    }
}
