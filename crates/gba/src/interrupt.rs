//! The GBA interrupt controller (`IE` / `IF` / `IME`).
//!
//! Devices don't call interrupt handlers. They request an interrupt *source*
//! here; the controller records it in `IF`, and the CPU samples the resulting
//! IRQ line at an architecturally legal instruction boundary. Whether the line
//! is asserted depends on `IE` (per-source enable) and `IME` (master enable);
//! the CPSR I-bit is a further gate owned by the CPU core, not modeled here.

/// A hardware interrupt source, as laid out in `IE`/`IF` (bit index = value).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum IrqSource {
    VBlank = 0,
    HBlank = 1,
    VCounterMatch = 2,
    Timer0 = 3,
    Timer1 = 4,
    Timer2 = 5,
    Timer3 = 6,
    Serial = 7,
    Dma0 = 8,
    Dma1 = 9,
    Dma2 = 10,
    Dma3 = 11,
    Keypad = 12,
    GamePak = 13,
}

impl IrqSource {
    /// The single-bit mask this source occupies in `IE`/`IF`.
    pub const fn mask(self) -> u16 {
        1 << (self as u16)
    }
}

/// The `IE`/`IF`/`IME` interrupt controller.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InterruptController {
    /// `IE` — per-source enable bits (`4000200h`).
    ie: u16,
    /// `IF` — pending request flags (`4000202h`).
    iflags: u16,
    /// `IME` — master enable (`4000208h`, bit 0).
    ime: bool,
}

impl InterruptController {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an interrupt request from `source` by setting its `IF` bit. This
    /// is what devices call; it is independent of `IE`/`IME`, which only gate
    /// whether the request reaches or is accepted by the CPU.
    pub fn request(&mut self, source: IrqSource) {
        self.iflags |= source.mask();
    }

    /// Write `IE`.
    pub fn set_ie(&mut self, ie: u16) {
        self.ie = ie;
    }

    /// Read `IE`.
    pub fn ie(&self) -> u16 {
        self.ie
    }

    /// Write `IME` (only bit 0 is meaningful).
    pub fn set_ime(&mut self, enabled: bool) {
        self.ime = enabled;
    }

    /// Read `IME`.
    pub fn ime(&self) -> bool {
        self.ime
    }

    /// Read `IF`.
    pub fn iflags(&self) -> u16 {
        self.iflags
    }

    /// Acknowledge interrupts: a CPU write to `IF` clears every bit set in
    /// `mask` (write-1-to-clear).
    pub fn acknowledge(&mut self, mask: u16) {
        self.iflags &= !mask;
    }

    /// Whether any enabled interrupt is pending, `IME` aside. This is the
    /// condition that wakes a halted CPU.
    pub fn pending(&self) -> bool {
        self.ie & self.iflags != 0
    }

    /// Whether an enabled interrupt within `mask` is pending. Used for Stop
    /// mode, which only wakes on a subset of sources.
    pub fn pending_within(&self, mask: u16) -> bool {
        self.ie & self.iflags & mask != 0
    }

    /// Whether the CPU IRQ input line is asserted: an enabled request is pending
    /// and the master enable is set. The CPU still applies the CPSR I-bit before
    /// taking the exception.
    pub fn line_asserted(&self) -> bool {
        self.ime && self.pending()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_masks() {
        assert_eq!(IrqSource::VBlank.mask(), 0x0001);
        assert_eq!(IrqSource::Timer0.mask(), 0x0008);
        assert_eq!(IrqSource::Timer3.mask(), 0x0040);
        assert_eq!(IrqSource::GamePak.mask(), 0x2000);
    }

    #[test]
    fn line_requires_enable_and_master() {
        let mut ic = InterruptController::new();
        ic.request(IrqSource::Timer0);
        // Pending in IF, but not enabled in IE.
        assert!(!ic.pending());
        assert!(!ic.line_asserted());

        ic.set_ie(IrqSource::Timer0.mask());
        assert!(ic.pending()); // enabled request is pending
        assert!(!ic.line_asserted()); // but IME is still off

        ic.set_ime(true);
        assert!(ic.line_asserted());
    }

    #[test]
    fn acknowledge_clears_only_written_bits() {
        let mut ic = InterruptController::new();
        ic.set_ie(0xFFFF);
        ic.set_ime(true);
        ic.request(IrqSource::Timer0);
        ic.request(IrqSource::VBlank);
        assert_eq!(ic.iflags(), IrqSource::Timer0.mask() | IrqSource::VBlank.mask());

        ic.acknowledge(IrqSource::Timer0.mask());
        assert_eq!(ic.iflags(), IrqSource::VBlank.mask());
        assert!(ic.line_asserted()); // VBlank still pending
    }
}
