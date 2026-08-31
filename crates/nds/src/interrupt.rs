//! The per-core interrupt controller (`IME`/`IE`/`IF`).
//!
//! Each DS core has its own controller. Devices raise an interrupt by setting a
//! bit in `IF`; the CPU line is asserted when any enabled-and-pending interrupt
//! exists and the master enable is on. The interleave orchestrator samples the
//! line at each core's instruction boundary and vectors the core if its CPSR
//! allows it. Unlike the GBA, `IE`/`IF` are 32-bit.

/// An interrupt source, numbered by its `IE`/`IF` bit. Only the sources the
/// current milestones raise are listed; the encoding follows GBATEK.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum IrqSource {
    VBlank = 0,
    HBlank = 1,
    VCount = 2,
    Timer0 = 3,
    Timer1 = 4,
    Timer2 = 5,
    Timer3 = 6,
    Dma0 = 8,
    Dma1 = 9,
    Dma2 = 10,
    Dma3 = 11,
    /// IPCSYNC remote-triggered interrupt.
    IpcSync = 16,
    /// The local send FIFO became empty (with its IRQ enabled).
    IpcSendEmpty = 17,
    /// The local receive FIFO became non-empty (with its IRQ enabled).
    IpcRecvNotEmpty = 18,
}

impl IrqSource {
    /// This source's bit mask within `IE`/`IF`.
    pub const fn mask(self) -> u32 {
        1 << (self as u32)
    }
}

/// One core's interrupt controller.
#[derive(Clone, Debug, Default)]
pub struct Interrupts {
    ime: bool,
    ie: u32,
    iflags: u32,
}

impl Interrupts {
    pub fn new() -> Self {
        Interrupts::default()
    }

    /// Raise an interrupt (set its `IF` bit).
    pub fn request(&mut self, source: IrqSource) {
        self.iflags |= source.mask();
    }

    /// Whether an enabled interrupt is pending (`IE & IF`), regardless of `IME`.
    pub fn pending(&self) -> bool {
        self.ie & self.iflags != 0
    }

    /// Whether the CPU's interrupt line is asserted (a pending interrupt and
    /// `IME`). The orchestrator gates this further on the CPSR I-bit.
    pub fn line_asserted(&self) -> bool {
        self.ime && self.pending()
    }

    pub fn ime(&self) -> bool {
        self.ime
    }
    pub fn set_ime(&mut self, value: bool) {
        self.ime = value;
    }

    pub fn ie(&self) -> u32 {
        self.ie
    }
    pub fn set_ie(&mut self, value: u32) {
        self.ie = value;
    }

    pub fn iflags(&self) -> u32 {
        self.iflags
    }

    /// Acknowledge interrupts: writing a 1 bit to `IF` clears it.
    pub fn acknowledge(&mut self, mask: u32) {
        self.iflags &= !mask;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_gates_on_ime_ie_and_if() {
        let mut irq = Interrupts::new();
        irq.request(IrqSource::IpcSync);
        assert!(!irq.line_asserted()); // IME off, IE clear
        irq.set_ie(IrqSource::IpcSync.mask());
        assert!(!irq.line_asserted()); // IME still off
        irq.set_ime(true);
        assert!(irq.line_asserted());
        // Acknowledging the flag drops the line.
        irq.acknowledge(IrqSource::IpcSync.mask());
        assert!(!irq.line_asserted());
    }

    #[test]
    fn unenabled_source_does_not_assert() {
        let mut irq = Interrupts::new();
        irq.set_ime(true);
        irq.set_ie(IrqSource::Timer0.mask());
        irq.request(IrqSource::IpcRecvNotEmpty); // not in IE
        assert!(!irq.line_asserted());
        irq.request(IrqSource::Timer0);
        assert!(irq.line_asserted());
    }
}
