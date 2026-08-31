//! Cartridge GPIO port and its Seiko S3511 real-time clock.
//!
//! Some carts (Pokémon Ruby/Sapphire/Emerald, Boktai…) wire a 4-bit general-purpose
//! I/O port into the ROM bus at `0x080000C4` (data), `0x080000C6` (direction) and
//! `0x080000C8` (control). The RTC hangs off three of those pins:
//!
//! * GPIO0 = SCK (serial clock, driven by the GBA)
//! * GPIO1 = SIO (serial data, bidirectional)
//! * GPIO2 = CS  (chip select, driven by the GBA)
//!
//! A command is framed by CS: raise CS, clock in an 8-bit command MSB-first
//! (`0110` header, then a 3-bit register select and a read/write bit), then clock
//! the register's bytes LSB-first — out of the chip for a read, into it for a
//! write. We serve reads from the host wall clock, so the in-game clock tracks real
//! time; writes to the time are accepted but do not override it (the control
//! register, e.g. the 24-hour flag, is kept).

use std::time::{SystemTime, UNIX_EPOCH};

/// Register-select values in the S3511 command byte.
const REG_RESET: u8 = 0;
const REG_CONTROL: u8 = 1;
const REG_DATETIME: u8 = 2;
const REG_TIME: u8 = 3;

/// Control-register flags. Bit 6 selects 24-hour mode; `WRITABLE` masks the bits a
/// write command may change (24-hour plus the three interrupt enables) — the
/// power-failure flag (bit 7) is not one of them.
const CONTROL_24HOUR: u8 = 0x40;
const CONTROL_WRITABLE: u8 = 0x6A;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Command,
    Read,
    Write,
}

#[derive(Clone, Debug)]
pub struct Rtc {
    // GPIO port.
    data: u8,        // last value the GBA wrote to the 4 pins
    direction: u8,   // 1 = pin driven by the GBA, 0 = driven by the device
    read_enable: bool,

    // Serial line state.
    prev_sck: bool,
    cs: bool,
    phase: Phase,

    // Bit/byte assembly for the active transfer.
    shift: u8,       // command byte (MSB-first) or an inbound data byte (LSB-first)
    shift_bits: u8,
    reg: u8,
    bytes_left: u8,
    in_byte_bits: u8,

    // Queued output bits for a read (LSB-first per byte), and the live SIO bit.
    out: Vec<bool>,
    out_index: usize,
    sio_out: bool,

    control: u8,
}

impl Default for Rtc {
    fn default() -> Self {
        Rtc {
            data: 0,
            direction: 0,
            read_enable: false,
            prev_sck: false,
            cs: false,
            phase: Phase::Idle,
            shift: 0,
            shift_bits: 0,
            reg: 0,
            bytes_left: 0,
            in_byte_bits: 0,
            out: Vec::new(),
            out_index: 0,
            sio_out: false,
            // A healthy, initialised clock: 24-hour mode set, no power-failure flag.
            // Games (e.g. Pokémon) treat a clear 24-hour bit as an error and show
            // "the internal battery has run dry", so a real-time RTC must start here.
            control: CONTROL_24HOUR,
        }
    }
}

impl Rtc {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read one of the three GPIO registers (offset `0xC4`/`0xC6`/`0xC8` within the
    /// ROM region). Returns `None` for any other offset so the bus falls back to ROM.
    pub fn read(&self, offset: u32) -> Option<u16> {
        Some(match offset {
            0xC4 => {
                if !self.read_enable {
                    return Some(0);
                }
                // Output pins read back their latch; input pins read the device.
                let mut pins = self.data & self.direction;
                if self.direction & 0b0010 == 0 && self.sio_out {
                    pins |= 0b0010; // SIO is an input right now, driven by the RTC
                }
                u16::from(pins & 0x0F)
            }
            0xC6 => u16::from(self.direction),
            0xC8 => u16::from(self.read_enable as u8),
            _ => return None,
        })
    }

    /// Write one of the three GPIO registers. Returns `true` if it was a GPIO
    /// register (so the bus does not also treat it as a dropped ROM write).
    pub fn write(&mut self, offset: u32, value: u16) -> bool {
        match offset {
            0xC4 => {
                self.data = value as u8 & 0x0F;
                self.step();
                true
            }
            0xC6 => {
                self.direction = value as u8 & 0x0F;
                true
            }
            0xC8 => {
                self.read_enable = value & 1 != 0;
                true
            }
            _ => false,
        }
    }

    /// The current pin levels the GBA is driving (0 for pins the device owns).
    fn driven(&self) -> u8 {
        self.data & self.direction
    }

    /// Advance the serial state machine on a change to the driven pins.
    fn step(&mut self) {
        let pins = self.driven();
        let sck = pins & 0b0001 != 0;
        let sio = pins & 0b0010 != 0;
        let cs = pins & 0b0100 != 0;

        // CS frames a command: a rising edge starts one, a low level ends it.
        if cs && !self.cs {
            self.phase = Phase::Command;
            self.shift = 0;
            self.shift_bits = 0;
        } else if !cs {
            self.phase = Phase::Idle;
        }
        self.cs = cs;

        let rising = sck && !self.prev_sck;
        let falling = !sck && self.prev_sck;
        self.prev_sck = sck;

        match self.phase {
            // Inbound bits (command + write parameters) are sampled on the rising
            // edge, MSB-first — the game drives SIO while the clock is low.
            Phase::Command if rising => {
                self.shift = (self.shift << 1) | sio as u8;
                self.shift_bits += 1;
                if self.shift_bits == 8 {
                    self.decode_command();
                }
            }
            Phase::Write if rising => {
                // Parameter bytes are LSB-first (unlike the MSB-first command).
                self.shift |= (sio as u8) << self.in_byte_bits;
                self.in_byte_bits += 1;
                if self.in_byte_bits == 8 {
                    self.store_byte(self.shift);
                    self.shift = 0;
                    self.in_byte_bits = 0;
                    self.bytes_left = self.bytes_left.saturating_sub(1);
                    if self.bytes_left == 0 {
                        self.phase = Phase::Idle;
                    }
                }
            }
            // Outbound bits are presented on the FALLING edge; the game raises the
            // clock and then samples SIO, so it reads the bit driven here. Presenting
            // on the rising edge instead would skip the first bit of every byte.
            Phase::Read if falling => {
                self.sio_out = self.out.get(self.out_index).copied().unwrap_or(false);
                self.out_index += 1;
            }
            _ => {}
        }
    }

    fn decode_command(&mut self) {
        let cmd = self.shift;
        // Expect the fixed 0110 header in the high nibble; if it is in the low
        // nibble instead the cart wired the byte reversed, so flip it.
        let cmd = if cmd >> 4 == 0b0110 { cmd } else { cmd.reverse_bits() };
        self.reg = (cmd >> 1) & 0b111;
        let read = cmd & 1 != 0;

        match self.reg {
            REG_RESET => {
                self.control = 0;
                self.phase = Phase::Idle;
            }
            _ if read => {
                self.out = self.read_register(self.reg);
                // The first output bit is driven on the read phase's first falling
                // clock edge (see `step`), which the game issues before sampling.
                self.out_index = 0;
                self.sio_out = false;
                self.phase = if self.out.is_empty() { Phase::Idle } else { Phase::Read };
            }
            _ => {
                self.bytes_left = register_len(self.reg);
                self.shift = 0;
                self.in_byte_bits = 0;
                self.phase = if self.bytes_left == 0 { Phase::Idle } else { Phase::Write };
            }
        }
    }

    /// The bytes a read command clocks out, flattened LSB-first (bit 0 of each byte
    /// leaves the chip first — the parameter convention the game's driver expects).
    fn read_register(&self, reg: u8) -> Vec<bool> {
        let bytes: Vec<u8> = match reg {
            REG_CONTROL => vec![self.control],
            REG_DATETIME => datetime_bcd(self.control).to_vec(),
            REG_TIME => datetime_bcd(self.control)[4..7].to_vec(),
            _ => vec![],
        };
        let mut bits = Vec::with_capacity(bytes.len() * 8);
        for byte in bytes {
            for i in 0..8 {
                bits.push((byte >> i) & 1 != 0);
            }
        }
        bits
    }

    /// Apply one byte clocked into a write command.
    fn store_byte(&mut self, byte: u8) {
        // Only the control register is retained; time writes are accepted but the
        // clock stays anchored to real time.
        if self.reg == REG_CONTROL {
            self.control = byte & CONTROL_WRITABLE;
        }
    }
}

/// Byte count clocked for a register in a write command.
fn register_len(reg: u8) -> u8 {
    match reg {
        REG_CONTROL => 1,
        REG_DATETIME => 7,
        REG_TIME => 3,
        _ => 0,
    }
}

/// The 7-byte datetime register (year, month, day, weekday, hour, minute, second)
/// in packed BCD, sampled from the host wall clock (UTC). Bit 6 of `control`
/// selects 24-hour mode; otherwise the hour uses the 12-hour form with the PM flag.
fn datetime_bcd(control: u8) -> [u8; 7] {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour24, minute, second) = ((rem / 3600) as u8, ((rem % 3600) / 60) as u8, (rem % 60) as u8);
    let (year, month, day) = civil_from_days(days);
    let weekday = ((days + 4).rem_euclid(7)) as u8; // 1970-01-01 was a Thursday

    let hour = if control & 0x40 != 0 {
        bcd(hour24)
    } else {
        let pm = hour24 >= 12;
        let mut h = hour24 % 12;
        if h == 0 {
            h = 12;
        }
        bcd(h) | if pm { 0x80 } else { 0 }
    };
    [bcd((year % 100) as u8), bcd(month), bcd(day), weekday, hour, bcd(minute), bcd(second)]
}

fn bcd(v: u8) -> u8 {
    ((v / 10) << 4) | (v % 10)
}

/// Gregorian year/month/day from a day count since 1970-01-01 (Howard Hinnant's
/// algorithm). Month and day are 1-based.
fn civil_from_days(z: i64) -> (i64, u8, u8) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    // These helpers mirror the real driver (e.g. Pokémon's `siirtc`): SIO on GPIO1,
    // SCK on GPIO0, CS on GPIO2. Bits are driven while the clock is low and sampled
    // by the chip on the rising edge; on a read the chip drives SIO on the falling
    // edge and the driver samples after raising the clock. The command byte is
    // MSB-first, parameter bytes are LSB-first.

    /// Clock one bit from the GBA into the chip (rising-edge sample).
    fn clock_in(rtc: &mut Rtc, bit: bool) {
        let sio = (bit as u16) << 1;
        rtc.write(0xC4, 0b0100 | sio); // CS high, SCK low, SIO=bit
        rtc.write(0xC4, 0b0100 | sio | 1); // rising edge
    }

    /// Clock one bit out of the chip: drop SCK (chip drives SIO), raise it, sample.
    fn clock_out(rtc: &mut Rtc) -> bool {
        rtc.write(0xC4, 0b0100); // SCK low — chip presents the bit
        rtc.write(0xC4, 0b0101); // SCK high
        rtc.read(0xC4).unwrap() & 0b0010 != 0
    }

    fn command(rtc: &mut Rtc, reg: u8, read: bool) {
        rtc.write(0xC6, 0b0111); // SCK/SIO/CS all outputs
        rtc.write(0xC4, 0b0000); // drop CS between commands
        rtc.write(0xC4, 0b0100); // raise CS -> start command
        let cmd = 0b0110_0000 | (reg << 1) | read as u8;
        for i in (0..8).rev() {
            clock_in(rtc, (cmd >> i) & 1 != 0); // command is MSB-first
        }
    }

    fn write_param(rtc: &mut Rtc, byte: u8) {
        for i in 0..8 {
            clock_in(rtc, (byte >> i) & 1 != 0); // parameters are LSB-first
        }
    }

    fn read_param(rtc: &mut Rtc) -> u8 {
        rtc.write(0xC6, 0b0101); // SIO -> input
        let mut byte = 0u8;
        for i in 0..8 {
            byte |= (clock_out(rtc) as u8) << i; // LSB-first
        }
        rtc.write(0xC6, 0b0111);
        byte
    }

    #[test]
    fn control_register_write_read_roundtrip() {
        let mut rtc = Rtc::new();
        rtc.write(0xC8, 1); // enable reads
        command(&mut rtc, REG_CONTROL, false);
        write_param(&mut rtc, 0x40); // 24-hour mode
        command(&mut rtc, REG_CONTROL, true);
        assert_eq!(read_param(&mut rtc), 0x40);
    }

    /// Reading the datetime over the wire must yield valid BCD — the exact check the
    /// game performs. Guards against the command/parameter bit-order and clock-edge
    /// bugs that made Pokémon report "the internal battery has run dry".
    #[test]
    fn datetime_reads_valid_bcd_over_the_wire() {
        let mut rtc = Rtc::new();
        rtc.write(0xC8, 1);
        command(&mut rtc, REG_DATETIME, true);
        let dt: Vec<u8> = (0..7).map(|_| read_param(&mut rtc)).collect();
        let bin = |b: u8| (b >> 4) * 10 + (b & 0xF);
        assert!(dt[0] & 0xF <= 9 && dt[0] >> 4 <= 9, "year {:02x}", dt[0]);
        assert!((1..=12).contains(&bin(dt[1])), "month {:02x}", dt[1]);
        assert!((1..=31).contains(&bin(dt[2])), "day {:02x}", dt[2]);
        assert!(bin(dt[4] & 0x7F) <= 23, "hour {:02x}", dt[4]);
        assert!(bin(dt[5]) <= 59, "min {:02x}", dt[5]);
        assert!(bin(dt[6]) <= 59, "sec {:02x}", dt[6]);
    }

    #[test]
    fn datetime_read_is_valid_bcd() {
        let dt = datetime_bcd(0x40); // 24-hour mode
        let month = (dt[1] >> 4) * 10 + (dt[1] & 0xF);
        let day = (dt[2] >> 4) * 10 + (dt[2] & 0xF);
        let hour = (dt[4] >> 4) * 10 + (dt[4] & 0xF);
        assert!((1..=12).contains(&month), "month {month}");
        assert!((1..=31).contains(&day), "day {day}");
        assert!(hour <= 23, "hour {hour}");
    }

    #[test]
    fn civil_epoch_is_1970() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(31), (1970, 2, 1));
        assert_eq!(civil_from_days(365), (1971, 1, 1));
    }

    #[test]
    fn reads_return_zero_until_enabled() {
        let mut rtc = Rtc::new();
        rtc.write(0xC6, 0b0111);
        assert_eq!(rtc.read(0xC4), Some(0)); // read-enable clear
        rtc.write(0xC8, 1);
        assert!(rtc.read(0xC4).is_some());
    }
}
