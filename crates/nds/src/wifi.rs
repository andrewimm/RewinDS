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
//!
//! Beyond those read-backs, the Wireless Manager's full bring-up needs a *live* radio: the
//! power-on handshake (`W_POWERSTATE`), the running microsecond counter (`W_US_COUNT`), and
//! the beacon interrupt it drives (`W_IF` bit 14 → ARM7 IRQ 24). Those are modelled here so
//! WM init completes instead of timing out — no actual networking, just a radio that says
//! it is alive. See [`crate::interrupt::IrqSource::Wifi`].

use emu_core::Timestamp;

/// The Wi-Fi I/O register window, 4 KB of 16-bit registers at `0x4808000`.
pub struct Wifi {
    regs: Box<[u16]>,
    /// The 8 KB Wi-Fi RAM (`0x4804000..0x4806000`, GBATEK "Wifi RAM" / MAC memory — the
    /// transmit/receive buffer area). Plain read/write storage: the Wireless Manager's
    /// bring-up runs a pattern read-back *self-test* across this whole region, and treats
    /// any mismatch as a dead radio (`WM_Initialize` fails → "communication error"). Backing
    /// it as real RAM lets that test pass. No networking is modelled; it just holds bytes.
    ram: Box<[u8]>,
    /// Baseband chip registers (Mitsumi MM3155), indices `0x00..=0x68`. R/W indices hold
    /// what firmware/game wrote; read-only indices are overridden on read (see `bb_read`).
    bb: [u8; 0x69],
    /// Latched result of the last BB read transfer, returned by `W_BB_READ` (`0x15C`).
    bb_read: u8,
    /// RF chip registers (RF9008 / Type2), indices `0x00..=0x1F`, 18-bit values.
    rf: [u32; 0x20],
    /// Latched result of the last RF read transfer, returned by `W_RF_DATA1` (`0x17E`).
    rf_read: u32,
    /// `W_US_COUNT` (`0x0F8`): the 1 MHz microsecond counter, running once `W_US_COUNTCNT`
    /// (`0x0E8`) bit 0 is set. Derived from the scheduler clock so reads advance smoothly:
    /// the value is `us_base_count + (now - us_base_cycle)` converted µs. The Wireless
    /// Manager's radio bring-up relies on this advancing and on the beacon IRQ it drives.
    us_enabled: bool,
    us_base_count: u64,
    us_base_cycle: Timestamp,
    /// `W_RANDOM` (`0x044`): an 11-bit LFSR the hardware advances on every read. A game that
    /// seeds from it during radio bring-up rejects a stuck value, so model a changing one.
    random: u16,
}

impl Default for Wifi {
    fn default() -> Self {
        Wifi::new()
    }
}

/// What the glue must schedule after a Wi-Fi register write (returned by [`Wifi::write`]).
#[derive(Default, Clone, Copy)]
pub struct WifiAfterWrite {
    /// The µs counter was just started — schedule the beacon / scan-dwell ticks.
    pub start_beacon: bool,
    /// The radio was just powered on — schedule the RF-wakeup IRQ.
    pub power_on: bool,
    /// Enabling a cause in `W_IE` left an already-pending `W_IF` flag enabled — assert the
    /// ARM7 Wi-Fi IRQ now (the line is level-driven by `W_IF & W_IE`, and the WM enables the
    /// cause *after* the flag is latched, then spins waiting for the interrupt).
    pub raise_irq: bool,
}

// Register offsets within the window (relative to `BASE`).
const W_ID: u32 = 0x000; // chip id (read-only): 0x1440 phat NDS, 0xC340 NDS-Lite
const W_IE: u32 = 0x010; // interrupt enable
const W_IF: u32 = 0x012; // interrupt flags (write-1-to-clear)
const W_POWER_US: u32 = 0x036; // radio power enable (the WM writes this to power on)
const W_POWERSTATE: u32 = 0x03C; // power request (bits 0-1) + status (bits 8-9)
const W_POWERFORCE: u32 = 0x040; // force power state now (bit 15 = force, bit 0 = off/on)
const W_RANDOM: u32 = 0x044; // 11-bit LFSR, advances on each read
const W_US_COUNTCNT: u32 = 0x0E8; // microsecond-counter enable (bit 0)
const W_US_COUNT0: u32 = 0x0F8; // µs counter, 4 × u16 at 0x0F8..0x0FE (64-bit)
const W_BB_CNT: u32 = 0x158; // BB access start: index (0-7) + direction (12-15: 5=write, 6=read)
const W_BB_WRITE: u32 = 0x15A; // BB data byte to send
const W_BB_READ: u32 = 0x15C; // BB data byte received
const W_BB_BUSY: u32 = 0x15E; // BB transfer busy (bit0)
const W_RF_DATA2: u32 = 0x17C; // RF transfer start: index/command (Type2 layout)
const W_RF_DATA1: u32 = 0x17E; // RF lower-16 data (write source / read result)
const W_RF_BUSY: u32 = 0x180; // RF transfer busy (bit0)
const W_RF_PINS: u32 = 0x19C; // transceiver pin status
const W_RF_STATUS: u32 = 0x214; // transceiver state machine status

/// `W_IF` bit 11: RF wakeup — raised when the radio finishes powering on. The Wireless
/// Manager's `WM_PowerOn` blocks on this before it starts a scan; without it the whole
/// bring-up times out.
const W_IF_RF_WAKEUP: u16 = 1 << 11;
/// `W_IF` bit 14: beacon / µs-compare timeslot. Once the WM powers on and starts the µs
/// counter, this is the per-channel scan-dwell tick that lets the scan finish.
const W_IF_BEACON: u16 = 1 << 14;
/// The system clock rate (shared with the RTC). 1 µs = this / 1_000_000 master cycles.
const CYCLES_PER_SECOND: u64 = 33_513_982;
/// The beacon-IRQ cadence while the µs-counter runs: 1024 µs (the compare's 0x400-µs
/// granularity) in master cycles (~34318). The WM expects periodic beacon interrupts.
pub const BEACON_CYCLES: Timestamp = CYCLES_PER_SECOND * 0x400 / 1_000_000;
/// Delay from a power-on request to the RF-wakeup IRQ: one µs timer tick (8 µs ≈ 268
/// cycles). melonDS raises it effectively immediately; a tick keeps it off the write path.
pub const POWER_ON_CYCLES: Timestamp = CYCLES_PER_SECOND * 8 / 1_000_000;

/// Whether `REWINDS_WIFI_TRACE` is set — gates one stderr line per Wi-Fi register access,
/// for diagnosing the Wireless-Manager bring-up handshake. Read once and cached.
fn wifi_trace_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("REWINDS_WIFI_TRACE").is_some())
}

impl Wifi {
    /// The window's base address and length (`0x4808000..0x4809000`).
    pub const BASE: u32 = 0x0480_8000;
    pub const LEN: u32 = 0x1000;
    /// The Wi-Fi RAM window (`0x4804000..0x4806000`, 8 KB).
    pub const RAM_BASE: u32 = 0x0480_4000;
    pub const RAM_LEN: u32 = 0x2000;

    /// Read `bytes` (1/2/4) from the Wi-Fi RAM at `offset` (relative to [`Self::RAM_BASE`]).
    pub fn ram_read(&self, offset: u32, bytes: u32) -> u32 {
        let mut v = 0u32;
        for i in 0..bytes {
            let idx = (offset + i) as usize & (self.ram.len() - 1);
            v |= (self.ram[idx] as u32) << (i * 8);
        }
        v
    }

    /// Write `bytes` (1/2/4) of `value` to the Wi-Fi RAM at `offset`.
    pub fn ram_write(&mut self, offset: u32, value: u32, bytes: u32) {
        for i in 0..bytes {
            let idx = (offset + i) as usize & (self.ram.len() - 1);
            self.ram[idx] = (value >> (i * 8)) as u8;
        }
    }

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
            ram: vec![0u8; Self::RAM_LEN as usize].into_boxed_slice(),
            bb,
            bb_read: 0,
            rf: [0; 0x20],
            rf_read: 0,
            us_enabled: false,
            us_base_count: 0,
            us_base_cycle: 0,
            random: 1,
        }
    }

    /// Advance and return the `W_RANDOM` LFSR (11-bit Galois, GBATEK's documented taps).
    fn next_random(&mut self) -> u16 {
        let r = self.random;
        self.random = ((r << 1) | (((r >> 10) ^ (r >> 2)) & 1)) & 0x7FF;
        r
    }

    /// The microsecond counter's current value: once enabled it advances at 1 MHz, derived
    /// from the elapsed master cycles so consecutive reads differ (as the WM expects).
    fn us_count(&self, now: Timestamp) -> u64 {
        if !self.us_enabled {
            return self.us_base_count;
        }
        let elapsed = now.saturating_sub(self.us_base_cycle);
        self.us_base_count + elapsed * 1_000_000 / CYCLES_PER_SECOND
    }

    /// A scheduled beacon tick fired: flag the beacon cause in `W_IF` and report whether the
    /// counter is still running (reschedule the next tick) and whether `W_IE` enables the
    /// beacon IRQ (assert ARM7 IRQ 24). Called from the glue's `WifiBeacon` event.
    pub fn beacon_tick(&mut self) -> (bool, bool) {
        if !self.us_enabled {
            return (false, false);
        }
        self.regs[(W_IF >> 1) as usize] |= W_IF_BEACON;
        let enabled = self.regs[(W_IE >> 1) as usize] & W_IF_BEACON != 0;
        (true, enabled)
    }

    /// A scheduled RF-wakeup fired (shortly after a power-on request): flag the RF-wakeup
    /// cause in `W_IF` and report whether `W_IE` enables it (so the glue asserts ARM7 IRQ
    /// 24). This is what lets the WM's `WM_PowerOn` proceed to the scan.
    pub fn power_on_irq(&mut self) -> bool {
        self.regs[(W_IF >> 1) as usize] |= W_IF_RF_WAKEUP;
        self.regs[(W_IE >> 1) as usize] & W_IF_RF_WAKEUP != 0
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

    /// Read `bytes` (1/2/4) from a register at `offset` into the window. `now` is the
    /// scheduler time, used to derive the live microsecond counter.
    ///
    /// Takes `&mut self` because the RF-wakeup flag (`W_IF` bit 11) is a momentary event,
    /// not a write-1-clear flag: the WM's interrupt dispatcher loops `while (W_IE & W_IF)`
    /// and its bit-11 sub-handler only re-arms `W_IE` (it never writes `W_IF` to ack), so
    /// the bit must auto-clear once the CPU has observed it or the dispatcher spins forever.
    pub fn read(&mut self, offset: u32, bytes: u32, now: Timestamp) -> u32 {
        // W_RANDOM advances on every read, so serve it before the (immutable) word reader.
        if offset & !1 == W_RANDOM {
            let result = self.next_random() as u32;
            if wifi_trace_enabled() {
                eprintln!("[wifi] R {:03X} ({}) -> {:04X} (random)", offset, bytes, result);
            }
            return result;
        }
        let count = self.us_count(now);
        let word = |o: u32| -> u32 {
            match o {
                W_ID => 0x1440, // chip id (read-only): a phat NDS; WM identifies the MAC/BB
                W_BB_READ => self.bb_read as u32,
                W_BB_BUSY | W_RF_BUSY => 0, // transfers complete instantly → never busy
                W_RF_DATA1 => self.rf_read & 0xFFFF,
                // Transceiver reports an idle-but-alive radio, so a status spin-wait ends.
                W_RF_PINS => 0x0004,
                W_RF_STATUS => 9,
                // The 64-bit µs counter spread over four halfwords at 0x0F8..0x0FE.
                W_US_COUNT0 => count as u32 & 0xFFFF,
                o if o == W_US_COUNT0 + 2 => (count >> 16) as u32 & 0xFFFF,
                o if o == W_US_COUNT0 + 4 => (count >> 32) as u32 & 0xFFFF,
                o if o == W_US_COUNT0 + 6 => (count >> 48) as u32 & 0xFFFF,
                _ => self.regs.get((o >> 1) as usize).copied().unwrap_or(0) as u32,
            }
        };
        let lo = word(offset & !1);
        let result = match bytes {
            1 => (lo >> ((offset & 1) * 8)) & 0xFF,
            2 => lo,
            _ => lo | (word((offset & !1) + 2) << 16),
        };
        if wifi_trace_enabled() {
            eprintln!("[wifi] R {:03X} ({}) -> {:04X}", offset, bytes, result);
        }
        // Reading W_IF latches-and-clears the momentary RF-wakeup (bit 11) and beacon /
        // µs-compare (bit 14) flags: the WM's interrupt dispatcher loops `while (W_IE & W_IF)`
        // and neither sub-handler writes W_IF to ack (the beacon handler only re-arms W_IE and
        // updates its timers), so the value returned above still shows the cause — handled
        // this pass — but the next read sees it gone and the dispatch loop exits.
        let base = offset & !1;
        if base == W_IF || (bytes == 4 && base == W_IE) {
            self.regs[(W_IF >> 1) as usize] &= !(W_IF_RF_WAKEUP | W_IF_BEACON);
        }
        result
    }

    /// Write `bytes` (1/2/4) of `value` to a register at `offset` into the window. `now` is
    /// the scheduler time. Returns which Wi-Fi events the glue should now schedule (the µs
    /// beacon ticks and/or the RF-wakeup IRQ).
    #[must_use]
    pub fn write(&mut self, offset: u32, value: u32, bytes: u32, now: Timestamp) -> WifiAfterWrite {
        if wifi_trace_enabled() {
            eprintln!("[wifi] W {:03X} ({}) = {:04X}", offset, bytes, value);
        }
        let mut after = WifiAfterWrite::default();
        // `W_IF` (0x012) is write-1-to-clear: clear the flagged causes rather than storing.
        if offset == W_IF && bytes != 4 {
            self.regs[(W_IF >> 1) as usize] &= !(value as u16);
            return after;
        }
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
        // Power-on handshake: the request in W_POWERSTATE bits 0-1 is acked in bits 8-9
        // (bit1 = power up → bit8 "on", bit9 clear; bit0 = power down → bit9 "down"), so a
        // firmware spin-wait on the status bits completes.
        if base == W_POWERSTATE {
            let req = self.regs[(W_POWERSTATE >> 1) as usize];
            self.regs[(W_POWERSTATE >> 1) as usize] = match () {
                _ if req & 0x2 != 0 => 0x0100,
                _ if req & 0x1 != 0 => 0x0200,
                _ => req,
            };
        }
        // `W_POWERFORCE` (0x040): with bit 15 set, immediately forces the radio power state
        // instead of going through the requested/ack handshake. The Wireless Manager's scan
        // drives this to power the radio down between channels (bit 0 = 1) and back up (bit 0
        // = 0), spin-waiting on `W_POWERSTATE` bits 8-9 for the forced state to take effect.
        if base == W_POWERFORCE {
            let v = self.regs[(W_POWERFORCE >> 1) as usize];
            if v & 0x8000 != 0 {
                self.regs[(W_POWERSTATE >> 1) as usize] = if v & 1 != 0 { 0x0200 } else { 0x0100 };
            }
        }
        // Writing W_POWER_US powers the radio on; the WM waits for the RF-wakeup IRQ (W_IF
        // bit 11) that the glue schedules in response. Only that bit — the WM's power-on
        // wait expects a clean W_IF, so the µs-counter/beacon (bit 14) must NOT be running
        // yet; the WM enables W_US_COUNTCNT itself once it reaches the scan.
        if base == W_POWER_US {
            after.power_on = true;
        }
        // A direct write to the µs counter rebases it from `now`.
        if (W_US_COUNT0..W_US_COUNT0 + 8).contains(&base) {
            let i = (W_US_COUNT0 >> 1) as usize;
            self.us_base_count = (0..4).fold(0u64, |a, k| a | (self.regs[i + k] as u64) << (16 * k));
            self.us_base_cycle = now;
        }
        // W_US_COUNTCNT bit 0 starts/stops the 1 MHz counter.
        if base == W_US_COUNTCNT {
            let enable = self.regs[(W_US_COUNTCNT >> 1) as usize] & 1 != 0;
            if enable && !self.us_enabled {
                self.us_base_cycle = now; // continue from the current frozen count
                self.us_enabled = true;
                after.start_beacon = true; // the glue schedules the first beacon tick
            } else if !enable && self.us_enabled {
                self.us_base_count = self.us_count(now); // freeze at the current value
                self.us_enabled = false;
            }
        }
        // The Wi-Fi → ARM7 interrupt line is level-driven by `W_IF & W_IE`. Enabling a cause
        // in `W_IE` while its flag is already latched must assert the line now — the WM
        // latches the RF-wakeup flag, *then* sets `W_IE` bit 11 and spins on the IRQ.
        if base == W_IE && self.regs[(W_IF >> 1) as usize] & self.regs[(W_IE >> 1) as usize] != 0 {
            after.raise_irq = true;
        }
        after
    }

    /// Test-only: write at clock 0, discarding the "schedule beacon" flag.
    #[cfg(test)]
    fn tw(&mut self, offset: u32, value: u32, bytes: u32) {
        let _ = self.write(offset, value, bytes, 0);
    }
    /// Test-only: read at clock 0.
    #[cfg(test)]
    fn tr(&mut self, offset: u32, bytes: u32) -> u32 {
        self.read(offset, bytes, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_returns_written_values() {
        let mut w = Wifi::new();
        w.tw(0x006, 0x003F, 2); // the boot write-verify loop's first register
        assert_eq!(w.tr(0x006, 2), 0x003F);
    }

    #[test]
    fn byte_and_word_access_agree() {
        let mut w = Wifi::new();
        w.tw(0x010, 0xABCD, 2);
        assert_eq!(w.tr(0x010, 1), 0xCD);
        assert_eq!(w.tr(0x011, 1), 0xAB);
        w.tw(0x020, 0x1234_5678, 4);
        assert_eq!(w.tr(0x020, 2), 0x5678);
        assert_eq!(w.tr(0x022, 2), 0x1234);
    }

    #[test]
    fn baseband_chip_id_reads_back() {
        // The decisive register: HGSS reads BB index 0x00 and expects the MM3155 chip id
        // 0x6D. Drive the indirect read protocol: W_BB_CNT = direction 6 (read) | index 0.
        let mut w = Wifi::new();
        w.tw(W_BB_CNT, 0x6000, 2); // read BB[0x00]
        assert_eq!(w.tr(W_BB_BUSY, 2) & 1, 0, "transfer completes instantly");
        assert_eq!(w.tr(W_BB_READ, 2) & 0xFF, 0x6D, "BB chip id");
        // A read-only "always 01h" register, and an always-zero one.
        w.tw(W_BB_CNT, 0x605D, 2);
        assert_eq!(w.tr(W_BB_READ, 2) & 0xFF, 0x01);
        w.tw(W_BB_CNT, 0x600D, 2);
        assert_eq!(w.tr(W_BB_READ, 2) & 0xFF, 0x00);
    }

    #[test]
    fn baseband_rw_register_round_trips() {
        let mut w = Wifi::new();
        // Write BB[0x01] = 0x9E, then read it back through the indirect ports.
        w.tw(W_BB_WRITE, 0x9E, 2);
        w.tw(W_BB_CNT, 0x5001, 2); // direction 5 (write) | index 0x01
        w.tw(W_BB_CNT, 0x6001, 2); // direction 6 (read) | index 0x01
        assert_eq!(w.tr(W_BB_READ, 2) & 0xFF, 0x9E);
    }

    #[test]
    fn rf_register_round_trips() {
        let mut w = Wifi::new();
        // Type2 write: W_RF_DATA1 = lower16, W_RF_DATA2 = index<<2 (command bit7=0 = write).
        w.tw(W_RF_DATA1, 0xC008, 2);
        w.tw(W_RF_DATA2, (0x05 << 2) | 0x1, 2); // index 5, upper bits = 1 → datum 0x1C008
        assert_eq!(w.tr(W_RF_BUSY, 2) & 1, 0);
        // Read it back: command bit7 set.
        w.tw(W_RF_DATA2, (0x05 << 2) | (1 << 7), 2);
        assert_eq!(w.tr(W_RF_DATA1, 2), 0xC008, "lower 16 bits of the RF datum");
    }

    #[test]
    fn radio_reports_alive_to_the_wireless_manager() {
        let mut w = Wifi::new();
        // Chip id and transceiver status read as a live, idle radio (not zeros).
        assert_eq!(w.tr(W_ID, 2), 0x1440, "MAC/BB chip id");
        assert_eq!(w.tr(W_RF_PINS, 2), 0x0004, "transceiver idle pins");
        assert_eq!(w.tr(W_RF_STATUS, 2), 9, "transceiver idle status");
        // Power-on handshake: request (bit1) is acked in the status bits (bit8 set).
        w.tw(W_POWERSTATE, 0x0002, 2);
        assert_eq!(w.tr(W_POWERSTATE, 2) & 0x0300, 0x0100, "powered-on, not down");
    }

    #[test]
    fn us_counter_advances_and_drives_the_beacon_irq() {
        let mut w = Wifi::new();
        assert_eq!(w.read(W_US_COUNT0, 2, 1_000_000), 0, "counter idle until enabled");
        // Enable the µs counter at t=0; it should request the first beacon tick.
        assert!(w.write(W_US_COUNTCNT, 1, 2, 0).start_beacon, "enabling the counter schedules a beacon");
        // One second of master cycles later, the counter reads ~1,000,000 µs.
        let secs_later = CYCLES_PER_SECOND;
        let count = w.read(W_US_COUNT0, 2, secs_later) as u64
            | (w.read(W_US_COUNT0 + 2, 2, secs_later) as u64) << 16;
        assert_eq!(count, 1_000_000, "1 MHz counter after one second");
        // With the beacon IRQ enabled in W_IE, a beacon tick flags it and asks to assert.
        w.tw(W_IE, W_IF_BEACON as u32, 2);
        let (active, raise) = w.beacon_tick();
        assert!(active && raise, "beacon ticks while enabled and raises when W_IE allows");
        assert_eq!(w.tr(W_IF, 2) & W_IF_BEACON as u32, W_IF_BEACON as u32, "W_IF beacon flag set");
        // W_IF is write-1-to-clear.
        w.tw(W_IF, W_IF_BEACON as u32, 2);
        assert_eq!(w.tr(W_IF, 2) & W_IF_BEACON as u32, 0, "beacon flag cleared");
    }
}
