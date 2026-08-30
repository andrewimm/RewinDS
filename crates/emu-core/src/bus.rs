//! Bus access vocabulary shared across machines.
//!
//! A memory access is not just an address and a width. The machine cares *who*
//! is accessing (CPU vs a DMA channel), *why* (instruction fetch vs data), and
//! *how* it relates to the previous access (sequential vs non-sequential) —
//! because those determine wait states, especially on cartridge ROM. This
//! module defines that vocabulary; the concrete memory map lives in each machine
//! crate.

/// Which bus master is performing an access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessMaster {
    Cpu,
    /// A DMA channel, by index.
    Dma(u8),
}

/// Whether an access is an instruction fetch or a data access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessKind {
    Instruction,
    Data,
}

/// Whether an access immediately follows the previous one in address order.
/// Sequential accesses are cheaper on wait-stated regions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessSequence {
    NonSequential,
    Sequential,
}

/// The width of an access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessWidth {
    Byte,
    Half,
    Word,
}

impl AccessWidth {
    /// The width in bytes.
    pub const fn bytes(self) -> u32 {
        match self {
            AccessWidth::Byte => 1,
            AccessWidth::Half => 2,
            AccessWidth::Word => 4,
        }
    }
}

/// The full description of a bus access, minus address and width (which the
/// read/write method names carry).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    pub master: AccessMaster,
    pub kind: AccessKind,
    pub sequence: AccessSequence,
}

impl Access {
    /// A CPU access of the given kind and sequence.
    pub const fn cpu(kind: AccessKind, sequence: AccessSequence) -> Access {
        Access {
            master: AccessMaster::Cpu,
            kind,
            sequence,
        }
    }

    /// A non-sequential CPU data access — the conservative default for one-off
    /// accesses (e.g. tests and debugger pokes).
    pub const fn cpu_data() -> Access {
        Access::cpu(AccessKind::Data, AccessSequence::NonSequential)
    }
}

/// The outcome of a bus access: the value read (or `()` for a write), the guest
/// cycles it consumed, and whether it changed the scheduler's next deadline.
///
/// `scheduling_changed` matters for writes to scheduling-sensitive MMIO (a timer
/// control register, say): the CPU backend must side-exit and recompute its
/// deadline rather than trust the one it entered with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BusResult<T> {
    pub value: T,
    pub cycles: u32,
    pub scheduling_changed: bool,
}

impl<T> BusResult<T> {
    /// A result that did not affect scheduling.
    pub fn plain(value: T, cycles: u32) -> Self {
        BusResult {
            value,
            cycles,
            scheduling_changed: false,
        }
    }
}
