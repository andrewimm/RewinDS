//! The ARM7 real-time clock (`0x4000138`), an S-35180/S-3511 3-wire serial RTC.
//!
//! Games read the date and time here during boot (and gate day/night content and
//! their RNG seed on it), so a stub that always returns 0 can stall the ARM7's serial
//! read loop. This models the bus — chip-select, clock, and a bidirectional data line,
//! one bit per clock, LSB first — over a real register file (status 1/2, date+time,
//! clock-adjust, free), following GBATEK "DS Real-Time Clock".
//!
//! The first byte after chip-select is a command: the low nibble is a fixed `0110`,
//! bits 4-6 select the register, and bit 7 is the direction (0 = write, 1 = read).
//! Reads shift the selected register's bytes out; writes shift the incoming bytes in
//! and apply them when chip-select is released.
//!
//! Time advances off the emulated master clock (a 1 Hz [`crate::NdsEvent::RtcTick`]),
//! not the host wall-clock, so the timeline is a pure function of emulated cycles and
//! stays identical across a rewind or replay — the property the debugger depends on.
//! The base is a fixed timestamp by default (deterministic for tests and trace diffs);
//! a frontend can seed the real current time with [`Rtc::set_datetime`].

use emu_core::Timestamp;

/// Master ticks per real-time second (the 33.513982 MHz ARM9/system clock).
pub const CYCLES_PER_SECOND: Timestamp = 33_513_982;

/// The register-select field of a command byte (GBATEK "Fwd" values, `(cmd >> 4) & 7`).
mod reg {
    pub const STAT1: u8 = 0;
    pub const STAT2: u8 = 4;
    pub const DATETIME: u8 = 2;
    pub const TIME: u8 = 6;
    pub const CLKADJUST: u8 = 3;
    pub const FREE: u8 = 7;
}

/// The RTC serial device behind the `0x4000138` port.
pub struct Rtc {
    // --- serial bus state ---
    /// Last value written to the port (upper control bits echo back on reads).
    last: u8,
    /// Chip select currently asserted (a transfer is in progress).
    selected: bool,
    /// Previous clock level, for falling-edge detection.
    prev_clock: bool,
    /// The command byte being shifted in, and how many of its bits so far.
    command: u8,
    command_bits: u8,
    /// Direction of the current transfer's parameter phase (from command bit 7).
    reading: bool,
    /// The register the current command selected (`(command >> 4) & 7`).
    selected_reg: u8,

    // --- parameter phase (read) ---
    /// The register's data bytes to shift out once the command is decoded.
    output: Vec<u8>,
    out_byte: usize,
    out_bit: u8,
    /// The data bit currently presented on the serial line.
    data_out: u8,

    // --- parameter phase (write) ---
    /// Bytes shifted in during a write, applied when chip-select is released.
    input: Vec<u8>,
    in_bits: u32,

    // --- register file ---
    /// Status register 1: bit0 reset (W), bit1 24-hour mode, bits2-3 general purpose,
    /// bits4-7 read-only flags (INT1/INT2/power-low/power-off), auto-cleared on read.
    stat1: u8,
    /// Status register 2: INT1/INT2 mode and test bits.
    stat2: u8,
    /// Date & time, BCD: `[year, month, day, day_of_week, hour, minute, second]`.
    /// Year is `00..99` (2000-based); `hour` carries the AM/PM flag in bit 6 (12h mode).
    datetime: [u8; 7],
    /// Clock-adjustment register (oscillator trim; cosmetic here).
    clkadjust: u8,
    /// Free general-purpose register.
    free: u8,
}

impl Default for Rtc {
    fn default() -> Self {
        Rtc::new()
    }
}

impl Rtc {
    /// A fresh RTC at a fixed, deterministic base time (2024-01-01 00:00:00, Monday),
    /// 24-hour mode. Deterministic so tests and trace diffs are reproducible; a frontend
    /// can install the host's current time with [`Rtc::set_datetime`].
    pub fn new() -> Self {
        Rtc {
            last: 0,
            selected: false,
            prev_clock: false,
            command: 0,
            command_bits: 0,
            reading: false,
            selected_reg: 0,
            output: Vec::new(),
            out_byte: 0,
            out_bit: 0,
            data_out: 0,
            input: Vec::new(),
            in_bits: 0,
            stat1: 0x02, // 24-hour mode, no pending flags
            stat2: 0x00,
            datetime: [0x24, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00],
            clkadjust: 0x00,
            free: 0x00,
        }
    }

    /// Seed the calendar (24-hour clock). Fields are plain integers (`year` is the full
    /// year, e.g. 2024); they are stored in the BCD layout the bus serves. `day_of_week`
    /// is `0..6`. A frontend calls this to install the host's real current time.
    #[allow(clippy::too_many_arguments)]
    pub fn set_datetime(
        &mut self,
        year: u16,
        month: u8,
        day: u8,
        day_of_week: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) {
        self.datetime = [
            to_bcd((year % 100) as u8),
            to_bcd(month),
            to_bcd(day),
            day_of_week & 0x07,
            to_bcd(hour) | if hour >= 12 { 0x40 } else { 0 },
            to_bcd(minute),
            to_bcd(second),
        ];
    }

    /// Advance the clock by one second, carrying through minutes, hours, and the
    /// calendar. Driven by the scheduler's 1 Hz [`crate::NdsEvent::RtcTick`].
    pub fn tick_second(&mut self) {
        let [y, mo, d, dow, h, mi, s] = self.datetime;
        let second = bcd_inc(s, 60);
        if second != 0 {
            self.datetime[6] = second;
            return;
        }
        self.datetime[6] = 0;
        let minute = bcd_inc(mi, 60);
        if minute != 0 {
            self.datetime[5] = minute;
            return;
        }
        self.datetime[5] = 0;
        // Hour carries the AM/PM flag (bit 6) in 12-hour mode; recompute it from the
        // 24-hour count so the flag stays correct across the noon/midnight rollover.
        let hour24 = (from_bcd(h & 0x3F) + 1) % 24;
        let ampm = if hour24 >= 12 { 0x40 } else { 0 };
        let hour_field = if self.stat1 & 0x02 != 0 {
            to_bcd(hour24) | ampm // 24-hour mode: 00..23, AM/PM read-only
        } else {
            to_bcd(hour24 % 12) | ampm // 12-hour mode: 00..11 + flag
        };
        self.datetime[4] = hour_field;
        if hour24 != 0 {
            return;
        }
        // Day rollover.
        self.datetime[3] = (dow + 1) % 7;
        let (year, month, day) = (from_bcd(y), from_bcd(mo), from_bcd(d));
        let (mut year, mut month, mut day) = (year, month, day + 1);
        if day > days_in_month(month, year) {
            day = 1;
            month += 1;
            if month > 12 {
                month = 1;
                year = (year + 1) % 100;
            }
        }
        self.datetime[0] = to_bcd(year);
        self.datetime[1] = to_bcd(month);
        self.datetime[2] = to_bcd(day);
    }

    /// Read the port: the control bits last written, with the serial data line (bit 0)
    /// reflecting the RTC's current output bit.
    pub fn read(&self) -> u32 {
        ((self.last & !0x01) | self.data_out) as u32
    }

    /// Write the port. Bit 0 = data, bit 1 = clock, bit 2 = chip-select. A bit transfers
    /// on each clock falling edge while chip-select is high; releasing chip-select ends
    /// the transfer (and applies a pending write).
    pub fn write(&mut self, value: u32) {
        let value = value as u8;
        let data = value & 0x01;
        let clock = value & 0x02 != 0;
        let select = value & 0x04 != 0;

        if !select {
            // Chip-select released: apply any pending write, then end the transfer.
            if self.selected && !self.reading && self.command_bits == 8 {
                self.apply_write();
            }
            self.reset_transfer();
            self.selected = false;
        } else {
            if !self.selected {
                // Chip-select asserted: begin a fresh command.
                self.selected = true;
                self.reset_transfer();
            }
            if self.prev_clock && !clock {
                self.transfer_bit(data);
            }
        }
        self.prev_clock = clock;
        self.last = value;
    }

    /// Clear the per-transfer command/parameter state (not the register file).
    fn reset_transfer(&mut self) {
        self.command = 0;
        self.command_bits = 0;
        self.reading = false;
        self.selected_reg = 0;
        self.output.clear();
        self.out_byte = 0;
        self.out_bit = 0;
        self.input.clear();
        self.in_bits = 0;
    }

    /// Shift one bit (LSB first): the first eight bits form the command, then parameter
    /// bytes are shifted out (read) or in (write).
    fn transfer_bit(&mut self, data_in: u8) {
        if self.command_bits < 8 {
            self.command |= data_in << self.command_bits;
            self.command_bits += 1;
            if self.command_bits == 8 {
                self.decode_command();
            }
            return;
        }
        if self.reading {
            // Present the next output bit to the reading CPU.
            self.data_out = self
                .output
                .get(self.out_byte)
                .map_or(0, |b| (b >> self.out_bit) & 1);
            self.out_bit += 1;
            if self.out_bit == 8 {
                self.out_bit = 0;
                self.out_byte += 1;
            }
        } else {
            // Accumulate the incoming bit into the write buffer.
            let byte = (self.in_bits >> 3) as usize;
            if byte >= self.input.len() {
                self.input.push(0);
            }
            self.input[byte] |= data_in << (self.in_bits & 7);
            self.in_bits += 1;
        }
    }

    /// Decode the command byte: register (`bits 4-6`), direction (`bit 7`, 1 = read).
    /// On a read, queue the selected register's bytes; on a write, prepare to collect.
    fn decode_command(&mut self) {
        self.selected_reg = (self.command >> 4) & 0x7;
        self.reading = self.command & 0x80 != 0;
        self.out_byte = 0;
        self.out_bit = 0;
        self.input.clear();
        self.in_bits = 0;
        if self.reading {
            self.output = self.load_register(self.selected_reg);
        }
    }

    /// The bytes a read command serves for `reg`. Reading status register 1 also
    /// auto-clears its read-only flag bits (4-7), per the S-3511 datasheet.
    fn load_register(&mut self, reg: u8) -> Vec<u8> {
        match reg {
            reg::STAT1 => {
                let v = self.stat1;
                self.stat1 &= 0x0F; // flags (bits 4-7) auto-clear on read
                vec![v]
            }
            reg::STAT2 => vec![self.stat2],
            reg::DATETIME => self.datetime.to_vec(),
            reg::TIME => self.datetime[4..7].to_vec(),
            reg::CLKADJUST => vec![self.clkadjust],
            reg::FREE => vec![self.free],
            _ => vec![0x00], // int1/int2 alarms: unused, read as zero
        }
    }

    /// Apply a completed write to the selected register. The date/time registers are
    /// writable on hardware (a game can set the clock); we honor status, clock-adjust,
    /// and free writes, and let date/time writes install a new calendar.
    fn apply_write(&mut self) {
        let Some(&first) = self.input.first() else {
            return;
        };
        match self.selected_reg {
            reg::STAT1 => {
                if first & 0x01 != 0 {
                    // Reset: registers return to their power-on state.
                    self.stat1 = 0x00;
                    self.stat2 = 0x00;
                    self.datetime = [0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00];
                } else {
                    // Only bits 0-3 are writable; the flag bits stay under our control.
                    self.stat1 = (self.stat1 & 0xF0) | (first & 0x0E);
                }
            }
            reg::STAT2 => self.stat2 = first,
            reg::DATETIME if self.input.len() >= 7 => {
                self.datetime.copy_from_slice(&self.input[..7]);
            }
            reg::TIME if self.input.len() >= 3 => {
                self.datetime[4..7].copy_from_slice(&self.input[..3]);
            }
            reg::CLKADJUST => self.clkadjust = first,
            reg::FREE => self.free = first,
            _ => {}
        }
    }
}

/// Encode `v` (0..99) as two BCD nibbles.
fn to_bcd(v: u8) -> u8 {
    ((v / 10) << 4) | (v % 10)
}

/// Decode a two-nibble BCD byte to 0..99.
fn from_bcd(v: u8) -> u8 {
    (v >> 4) * 10 + (v & 0x0F)
}

/// Increment a BCD value, wrapping at `modulo`. Returns the new BCD value (0 on wrap).
fn bcd_inc(v: u8, modulo: u8) -> u8 {
    let n = from_bcd(v) + 1;
    to_bcd(if n >= modulo { 0 } else { n })
}

/// Days in `month` (1-12) for the 2000-based `year` (0-99), honoring leap years.
fn days_in_month(month: u8, year: u8) -> u8 {
    match month {
        2 => {
            // Every 2000-based year divisible by 4 is a leap year (2000..2099).
            if year.is_multiple_of(4) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the serial bus: assert CS, clock a command byte in (LSB first), then clock
    /// the parameter bytes in/out. Returns the bytes read back.
    fn run_command(rtc: &mut Rtc, command: u8, params: usize, write: Option<&[u8]>) -> Vec<u8> {
        rtc.write(0b100); // CS high
        let shift = |rtc: &mut Rtc, bit: u8| {
            rtc.write(0b100 | bit as u32); // clock low, data
            rtc.write(0b110 | bit as u32); // clock high
            rtc.write(0b100 | bit as u32); // clock low → falling edge transfers
        };
        for i in 0..8 {
            shift(rtc, (command >> i) & 1);
        }
        let mut out = Vec::new();
        for byte in 0..params {
            let mut b = 0u8;
            for i in 0..8 {
                let bit = write.map_or(0, |w| (w[byte] >> i) & 1);
                rtc.write(0b100 | bit as u32);
                rtc.write(0b110 | bit as u32);
                rtc.write(0b100 | bit as u32);
                b |= (rtc.read() as u8 & 1) << i;
            }
            out.push(b);
        }
        rtc.write(0b000); // CS low → end (applies a write)
        out
    }

    /// Command byte for `reg` (GBATEK Fwd) with the fixed `0110` low nibble; `read` sets
    /// bit 7.
    fn cmd(reg: u8, read: bool) -> u8 {
        0x06 | (reg << 4) | if read { 0x80 } else { 0 }
    }

    #[test]
    fn reads_the_date_and_time() {
        let mut rtc = Rtc::new();
        let bytes = run_command(&mut rtc, cmd(reg::DATETIME, true), 7, None);
        assert_eq!(bytes, vec![0x24, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn status_register_1_reports_24h_mode_without_stray_flags() {
        // The boot code masks bits 6-7 and branches on them; only bit 1 (24h) may be set.
        let mut rtc = Rtc::new();
        let bytes = run_command(&mut rtc, cmd(reg::STAT1, true), 1, None);
        assert_eq!(bytes, vec![0x02]);
    }

    #[test]
    fn tick_carries_seconds_into_minutes_and_hours() {
        let mut rtc = Rtc::new();
        rtc.set_datetime(2024, 1, 1, 0, 1, 59, 59);
        rtc.tick_second();
        // 01:59:59 -> 02:00:00
        assert_eq!(rtc.datetime[4], 0x02); // hour (24h, no PM flag)
        assert_eq!(rtc.datetime[5], 0x00); // minute
        assert_eq!(rtc.datetime[6], 0x00); // second
    }

    #[test]
    fn tick_rolls_over_end_of_month_and_updates_day_of_week() {
        let mut rtc = Rtc::new();
        rtc.set_datetime(2024, 1, 31, 3, 23, 59, 59); // Wed 2024-01-31 23:59:59
        rtc.tick_second();
        assert_eq!(rtc.datetime[0], 0x24); // year 2024
        assert_eq!(rtc.datetime[1], 0x02); // February
        assert_eq!(rtc.datetime[2], 0x01); // day 1
        assert_eq!(rtc.datetime[3], 4); // day of week advanced Wed(3) -> Thu(4)
        assert_eq!(rtc.datetime[4], 0x00); // hour
    }

    #[test]
    fn leap_day_is_honored() {
        let mut rtc = Rtc::new();
        rtc.set_datetime(2024, 2, 28, 0, 23, 59, 59); // 2024 is a leap year
        rtc.tick_second();
        assert_eq!(rtc.datetime[1], 0x02); // still February
        assert_eq!(rtc.datetime[2], 0x29); // Feb 29
    }

    #[test]
    fn writing_status_register_1_sets_12_hour_mode() {
        let mut rtc = Rtc::new();
        run_command(&mut rtc, cmd(reg::STAT1, false), 1, Some(&[0x00])); // clear 24h bit
        let bytes = run_command(&mut rtc, cmd(reg::STAT1, true), 1, None);
        assert_eq!(bytes, vec![0x00]);
    }

    #[test]
    fn writing_the_clock_is_read_back() {
        let mut rtc = Rtc::new();
        let set = [0x25, 0x06, 0x15, 0x00, 0x09, 0x30, 0x00]; // 2025-06-15 09:30:00
        run_command(&mut rtc, cmd(reg::DATETIME, false), 7, Some(&set));
        let bytes = run_command(&mut rtc, cmd(reg::DATETIME, true), 7, None);
        assert_eq!(bytes, set);
    }
}
