//! A minimal DS Wi-Fi register block (`0x4808000`-`0x4808FFF`, ARM7).
//!
//! At boot the ARM7 copies the firmware's Wi-Fi calibration into these registers and
//! verifies each write with an immediate read-back (a retry loop that gives up, and
//! deadlocks the inter-core boot handshake, if the value doesn't stick). We back the
//! region with plain read/write storage so those verifications pass. Actual Wi-Fi
//! networking is unemulated — this emulator's focus is 2D/3D video and debugging, not
//! wireless — so the registers hold values but drive no radio.

/// The Wi-Fi I/O register window, 4 KB of 16-bit registers at `0x4808000`.
pub struct Wifi {
    regs: Box<[u16]>,
}

impl Default for Wifi {
    fn default() -> Self {
        Wifi::new()
    }
}

impl Wifi {
    /// The window's base address and length (`0x4808000..0x4809000`).
    pub const BASE: u32 = 0x0480_8000;
    pub const LEN: u32 = 0x1000;

    pub fn new() -> Self {
        Wifi {
            regs: vec![0u16; (Self::LEN / 2) as usize].into_boxed_slice(),
        }
    }

    /// Read `bytes` (1/2/4) from a register at `offset` into the window.
    pub fn read(&self, offset: u32, bytes: u32) -> u32 {
        let idx = (offset >> 1) as usize;
        let lo = self.regs.get(idx).copied().unwrap_or(0) as u32;
        match bytes {
            1 => (lo >> ((offset & 1) * 8)) & 0xFF,
            2 => lo,
            _ => lo | ((self.regs.get(idx + 1).copied().unwrap_or(0) as u32) << 16),
        }
    }

    /// Write `bytes` (1/2/4) of `value` to a register at `offset` into the window.
    pub fn write(&mut self, offset: u32, value: u32, bytes: u32) {
        let idx = (offset >> 1) as usize;
        match bytes {
            1 => {
                if let Some(r) = self.regs.get_mut(idx) {
                    let shift = (offset & 1) * 8;
                    *r = (*r & !(0xFF << shift)) | ((value & 0xFF) << shift) as u16;
                }
            }
            2 => {
                if let Some(r) = self.regs.get_mut(idx) {
                    *r = value as u16;
                }
            }
            _ => {
                if let Some(r) = self.regs.get_mut(idx) {
                    *r = value as u16;
                }
                if let Some(r) = self.regs.get_mut(idx + 1) {
                    *r = (value >> 16) as u16;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_returns_written_values() {
        let mut w = Wifi::new();
        w.write(0x006, 0x003F, 2); // the boot write-verify loop's first register
        assert_eq!(w.read(0x006, 2), 0x003F);
    }

    #[test]
    fn byte_and_word_access_agree() {
        let mut w = Wifi::new();
        w.write(0x010, 0xABCD, 2);
        assert_eq!(w.read(0x010, 1), 0xCD);
        assert_eq!(w.read(0x011, 1), 0xAB);
        w.write(0x020, 0x1234_5678, 4);
        assert_eq!(w.read(0x020, 2), 0x5678);
        assert_eq!(w.read(0x022, 2), 0x1234);
    }
}
