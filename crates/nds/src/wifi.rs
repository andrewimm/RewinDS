//! A minimal DS Wi-Fi register block (`0x4808000`-`0x4808FFF`, ARM7).
//!
//! At boot the ARM7 copies the firmware's Wi-Fi calibration into these registers and
//! verifies each write with an immediate read-back (a retry loop that gives up, and
//! deadlocks the inter-core boot handshake, if the value doesn't stick). We back the
//! region with plain read/write storage so those verifications pass. Actual Wi-Fi
//! networking is unemulated — this emulator's focus is 2D/3D video and debugging, not
//! wireless — so the registers hold values but drive no radio.
//!
//! Beyond plain storage we model the **indirect baseband (BB) and RF register access**
//! (`W_BB_*`/`W_RF_*`). A game (notably Pokémon HeartGold/SoulSilver on *continue*, which
//! brings up the always-on wireless features) verifies the radio by reading back BB/RF
//! registers through these ports — most importantly the BB chip-ID at index `0x00`, which
//! must read `0x6Dh` (GBATEK "DS Wifi Baseband Chip"). Without this the reads return 0,
//! the game decides the radio is dead, and shows "A communication error has occurred".

/// The Wi-Fi I/O register window, 4 KB of 16-bit registers at `0x4808000`.
pub struct Wifi {
    regs: Box<[u16]>,
    /// Baseband chip registers (Mitsumi MM3155), indices `0x00..=0x68`. R/W indices hold
    /// what firmware/game wrote; read-only indices are overridden on read (see `bb_read`).
    bb: [u8; 0x69],
    /// Latched result of the last BB read transfer, returned by `W_BB_READ` (`0x15C`).
    bb_read: u8,
    /// RF chip registers (RF9008 / Type2), indices `0x00..=0x1F`, 18-bit values.
    rf: [u32; 0x20],
    /// Latched result of the last RF read transfer, returned by `W_RF_DATA1` (`0x17E`).
    rf_read: u32,
}

impl Default for Wifi {
    fn default() -> Self {
        Wifi::new()
    }
}

// Register offsets within the window (relative to `BASE`).
const W_BB_CNT: u32 = 0x158; // BB access start: index (0-7) + direction (12-15: 5=write, 6=read)
const W_BB_WRITE: u32 = 0x15A; // BB data byte to send
const W_BB_READ: u32 = 0x15C; // BB data byte received
const W_BB_BUSY: u32 = 0x15E; // BB transfer busy (bit0)
const W_RF_DATA2: u32 = 0x17C; // RF transfer start: index/command (Type2 layout)
const W_RF_DATA1: u32 = 0x17E; // RF lower-16 data (write source / read result)
const W_RF_BUSY: u32 = 0x180; // RF transfer busy (bit0)

impl Wifi {
    /// The window's base address and length (`0x4808000..0x4809000`).
    pub const BASE: u32 = 0x0480_8000;
    pub const LEN: u32 = 0x1000;

    pub fn new() -> Self {
        let mut bb = [0u8; 0x69];
        // On hardware the ARM7 firmware bootcode programs the baseband registers with the
        // firmware's Wi-Fi calibration. We direct-boot (skipping that bootcode), so seed the
        // R/W registers with GBATEK's documented defaults — otherwise a game that reads the
        // channel calibration back (e.g. BB[1Eh] gain) sees zeros and rejects the radio.
        bb[0x01] = 0x9E; // init/reset control (bit 7)
        bb[0x1E] = 0xBB; // per-channel external gain (GBATEK "change channels")
        bb[0x35] = 0x1F; // energy-detection threshold
        Wifi {
            regs: vec![0u16; (Self::LEN / 2) as usize].into_boxed_slice(),
            bb,
            bb_read: 0,
            rf: [0; 0x20],
            rf_read: 0,
        }
    }

    /// The value the BB chip returns for register `index`. Read-only indices report their
    /// fixed hardware values (per GBATEK's MM3155 table); every other index reads back
    /// what was written to it.
    fn bb_read(&self, index: usize) -> u8 {
        match index {
            0x00 => 0x6D,                                     // chip ID (read-only)
            0x5D => 0x01,                                     // read-only constant
            0x64 => 0x3F,                                     // read-only (0xFF/0x3F)
            0x0D..=0x12 | 0x16..=0x1A | 0x27 | 0x4D | 0x5E..=0x61 | 0x66 => 0x00, // always-zero
            i if i <= 0x68 => self.bb[i],                     // R/W registers
            _ => 0x00,                                        // 0x69.. always zero
        }
    }

    /// Start the BB transfer requested by a `W_BB_CNT` write: direction 5 writes
    /// `W_BB_WRITE` into the addressed register, direction 6 latches the register into
    /// `W_BB_READ`. Busy is reported as always-ready.
    fn bb_transfer(&mut self, cnt: u16) {
        let index = (cnt & 0xFF) as usize;
        let direction = (cnt >> 12) & 0xF;
        match direction {
            5 => {
                if index <= 0x68 {
                    self.bb[index] = self.regs[(W_BB_WRITE >> 1) as usize] as u8;
                }
            }
            6 => self.bb_read = self.bb_read(index),
            _ => {}
        }
    }

    /// Start the RF transfer requested by a `W_RF_DATA2` write (Type2 layout): bit 7 is
    /// the command (0 = write, 1 = read), bits 2-6 the index, bits 0-1 the upper 2 bits of
    /// the 18-bit datum (lower 16 come from `W_RF_DATA1`). A write stores the datum; a read
    /// latches it back into `W_RF_DATA1`.
    fn rf_transfer(&mut self, data2: u16) {
        let index = ((data2 >> 2) & 0x1F) as usize;
        let read = data2 & (1 << 7) != 0;
        if read {
            self.rf_read = self.rf[index];
        } else {
            let lower = self.regs[(W_RF_DATA1 >> 1) as usize] as u32;
            self.rf[index] = ((data2 as u32 & 0x3) << 16) | lower;
        }
    }

    /// Read `bytes` (1/2/4) from a register at `offset` into the window.
    pub fn read(&self, offset: u32, bytes: u32) -> u32 {
        let word = |o: u32| -> u32 {
            match o {
                W_BB_READ => self.bb_read as u32,
                W_BB_BUSY | W_RF_BUSY => 0, // transfers complete instantly → never busy
                W_RF_DATA1 => self.rf_read & 0xFFFF,
                _ => self.regs.get((o >> 1) as usize).copied().unwrap_or(0) as u32,
            }
        };
        let lo = word(offset & !1);
        match bytes {
            1 => (lo >> ((offset & 1) * 8)) & 0xFF,
            2 => lo,
            _ => lo | (word((offset & !1) + 2) << 16),
        }
    }

    /// Write `bytes` (1/2/4) of `value` to a register at `offset` into the window.
    pub fn write(&mut self, offset: u32, value: u32, bytes: u32) {
        // Store the raw value first (so W_BB_WRITE / W_RF_DATA1 are in place for a transfer),
        // then act on the control registers that start a BB/RF transfer.
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
        // Writing a control register starts its transfer. The halfword(s) just stored
        // are the control word; a 32-bit write also covers the following register.
        let base = offset & !1;
        for o in [base, base + 2] {
            if o != base && bytes != 4 {
                break; // a 16-bit write touches only the first halfword
            }
            if o == W_BB_CNT {
                self.bb_transfer(self.regs[(W_BB_CNT >> 1) as usize]);
            } else if o == W_RF_DATA2 {
                self.rf_transfer(self.regs[(W_RF_DATA2 >> 1) as usize]);
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

    #[test]
    fn baseband_chip_id_reads_back() {
        // The decisive register: HGSS reads BB index 0x00 and expects the MM3155 chip id
        // 0x6D. Drive the indirect read protocol: W_BB_CNT = direction 6 (read) | index 0.
        let mut w = Wifi::new();
        w.write(W_BB_CNT, 0x6000, 2); // read BB[0x00]
        assert_eq!(w.read(W_BB_BUSY, 2) & 1, 0, "transfer completes instantly");
        assert_eq!(w.read(W_BB_READ, 2) & 0xFF, 0x6D, "BB chip id");
        // A read-only "always 01h" register, and an always-zero one.
        w.write(W_BB_CNT, 0x605D, 2);
        assert_eq!(w.read(W_BB_READ, 2) & 0xFF, 0x01);
        w.write(W_BB_CNT, 0x600D, 2);
        assert_eq!(w.read(W_BB_READ, 2) & 0xFF, 0x00);
    }

    #[test]
    fn baseband_rw_register_round_trips() {
        let mut w = Wifi::new();
        // Write BB[0x01] = 0x9E, then read it back through the indirect ports.
        w.write(W_BB_WRITE, 0x9E, 2);
        w.write(W_BB_CNT, 0x5001, 2); // direction 5 (write) | index 0x01
        w.write(W_BB_CNT, 0x6001, 2); // direction 6 (read) | index 0x01
        assert_eq!(w.read(W_BB_READ, 2) & 0xFF, 0x9E);
    }

    #[test]
    fn rf_register_round_trips() {
        let mut w = Wifi::new();
        // Type2 write: W_RF_DATA1 = lower16, W_RF_DATA2 = index<<2 (command bit7=0 = write).
        w.write(W_RF_DATA1, 0xC008, 2);
        w.write(W_RF_DATA2, (0x05 << 2) | 0x1, 2); // index 5, upper bits = 1 → datum 0x1C008
        assert_eq!(w.read(W_RF_BUSY, 2) & 1, 0);
        // Read it back: command bit7 set.
        w.write(W_RF_DATA2, (0x05 << 2) | (1 << 7), 2);
        assert_eq!(w.read(W_RF_DATA1, 2), 0xC008, "lower 16 bits of the RF datum");
    }
}
