//! The ARM7 real-time clock (`0x4000138`), a 3-wire serial bus to the S-35180 RTC.
//!
//! Games read the date and time here during boot (and gate day/night content on it),
//! so a stub that always returns 0 can stall the ARM7's serial read loop. This models
//! the bus: chip-select, clock, and a bidirectional data line, transferring one bit
//! per clock (LSB first). The first byte after chip-select is a command whose bits
//! 4-6 select a register; the following bytes carry that register's data, which we
//! serve from a fixed, plausible clock (GBATEK "DS Real-Time Clock").

/// The RTC serial device behind the `0x4000138` port.
#[derive(Default)]
pub struct Rtc {
    /// Last value written to the port (upper control bits echo back on reads).
    last: u8,
    /// Chip select currently asserted (a transfer is in progress).
    selected: bool,
    /// Previous clock level, for falling-edge detection.
    prev_clock: bool,
    /// The command byte being shifted in, and how many of its bits so far.
    command: u8,
    command_bits: u8,
    /// The register's data bytes to shift out once the command is decoded.
    output: Vec<u8>,
    out_byte: usize,
    out_bit: u8,
    /// The data bit currently presented on the serial line.
    data_out: u8,
}

impl Rtc {
    pub fn new() -> Self {
        Rtc::default()
    }

    /// Read the port: the control bits last written, with the serial data line (bit
    /// 0) reflecting the RTC's current output bit.
    pub fn read(&self) -> u32 {
        ((self.last & !0x01) | self.data_out) as u32
    }

    /// Write the port. Bit 0 = data, bit 1 = clock, bit 2 = chip-select. A bit
    /// transfers on each clock falling edge while chip-select is high.
    pub fn write(&mut self, value: u32) {
        let value = value as u8;
        let data = value & 0x01;
        let clock = value & 0x02 != 0;
        let select = value & 0x04 != 0;

        if !select {
            // Chip-select released: the transfer ends.
            self.selected = false;
            self.command_bits = 0;
            self.command = 0;
            self.output.clear();
        } else {
            if !self.selected {
                // Chip-select asserted: begin a fresh command.
                self.selected = true;
                self.command_bits = 0;
                self.command = 0;
                self.output.clear();
            }
            if self.prev_clock && !clock {
                self.transfer_bit(data);
            }
        }
        self.prev_clock = clock;
        self.last = value;
    }

    /// Shift one bit (LSB first): first eight bits form the command; after that, data
    /// bytes are shifted out to the reading CPU.
    fn transfer_bit(&mut self, data_in: u8) {
        if self.command_bits < 8 {
            self.command |= data_in << self.command_bits;
            self.command_bits += 1;
            if self.command_bits == 8 {
                self.decode_command();
            }
            return;
        }
        // Parameter phase: present the next output bit (a read).
        self.data_out = self
            .output
            .get(self.out_byte)
            .map_or(0, |b| (b >> self.out_bit) & 1);
        self.out_bit += 1;
        if self.out_bit == 8 {
            self.out_bit = 0;
            self.out_byte += 1;
        }
    }

    /// Decode the command byte and queue the selected register's data (GBATEK: the
    /// low nibble is a fixed `0110`, bits 4-6 select the register).
    fn decode_command(&mut self) {
        self.out_byte = 0;
        self.out_bit = 0;
        self.output = match (self.command >> 4) & 0x7 {
            0 => vec![0x40],                                     // status register 1 (24-hour)
            4 => vec![0x00],                                     // status register 2
            2 => vec![0x24, 0x01, 0x01, 0x02, 0x12, 0x00, 0x00], // date+time (BCD)
            6 => vec![0x12, 0x00, 0x00],                         // time (BCD)
            _ => vec![0x00],
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the serial bus like the ARM7 does — clock in a "read date+time" command,
    /// then clock out the seven BCD bytes — and confirm we get the fixed clock back.
    #[test]
    fn reads_the_date_and_time() {
        let mut rtc = Rtc::new();
        // Assert chip-select.
        rtc.write(0b100);

        let shift_out = |rtc: &mut Rtc, bit: u8| {
            // clock low with the data bit, then clock high (falling edge already
            // consumed on the low transition via prev_clock tracking).
            rtc.write(0b100 | (bit as u32)); // clock low, cs high
            rtc.write(0b110 | (bit as u32)); // clock high
            rtc.write(0b100 | (bit as u32)); // clock low → falling edge transfers
        };

        // Command byte 0x26 (fixed 0110 + register 2 = date+time) LSB first.
        for i in 0..8 {
            shift_out(&mut rtc, (0x26 >> i) & 1);
        }
        // Read 7 bytes back, LSB first.
        let mut bytes = Vec::new();
        for _ in 0..7 {
            let mut b = 0u8;
            for i in 0..8 {
                rtc.write(0b100); // clock low
                rtc.write(0b110); // clock high
                rtc.write(0b100); // clock low → falling edge presents next bit
                b |= (rtc.read() as u8 & 1) << i;
            }
            bytes.push(b);
        }
        assert_eq!(bytes, vec![0x24, 0x01, 0x01, 0x02, 0x12, 0x00, 0x00]);
    }
}
