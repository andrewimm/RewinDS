//! Serial EEPROM save chip (4 Kbit / 64 Kbit).
//!
//! Unlike SRAM/Flash, EEPROM does not live in the `0x0E000000` save region. It is
//! a bit-serial device wired into the *upper GamePak window* (`0x0D000000..`), and
//! the game talks to it exclusively through DMA3: each 16-bit transfer moves a
//! single bit (only D0 matters). A command is a burst of *write* transfers,
//! followed by *read* transfers:
//!
//! * Read:  `1 1` + address + `0`, then 68 reads (4 don't-care bits + 64 data).
//! * Write: `1 0` + address + 64 data bits + `0`, then reads poll the ready bit.
//!
//! The address is 6 bits on a 512-byte chip and 14 bits on an 8-KiB chip; the chip
//! is organised as 8-byte blocks. The width is not encoded anywhere — hardware
//! infers it from the number of bits the game clocks in, so we detect it from the
//! command-burst length (9/73 bits ⇒ 6-bit, 17/81 bits ⇒ 14-bit).

const SIZE_512: usize = 0x200;
const SIZE_8K: usize = 0x2000;

#[derive(Clone, Debug)]
pub struct Eeprom {
    data: Vec<u8>,
    /// Address bits per command: 6 or 14. `0` until the first command reveals it.
    addr_bits: u8,
    /// Bits clocked in since the current command started (MSB first).
    input: u128,
    input_len: u16,
    /// Bits queued to clock out (MSB first), and how many remain.
    output: u128,
    output_len: u16,
    dirty: bool,
}

impl Default for Eeprom {
    fn default() -> Self {
        // Start sized for the smaller chip; a 14-bit command upgrades it to 8 KiB.
        Eeprom { data: vec![0xFF; SIZE_512], addr_bits: 0, input: 0, input_len: 0, output: 0, output_len: 0, dirty: false }
    }
}

impl Eeprom {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// The chip size in bytes (512 or 8192).
    pub fn size(&self) -> usize {
        self.data.len()
    }

    /// Restore from a `.sav`: the file length picks the chip size (and thus the
    /// address width), so a save round-trips even without the `.meta` sidecar.
    pub fn load(&mut self, data: &[u8]) {
        let size = if data.len() > SIZE_512 { SIZE_8K } else { SIZE_512 };
        self.addr_bits = if size == SIZE_8K { 14 } else { 6 };
        self.data = vec![0xFF; size];
        let len = data.len().min(size);
        self.data[..len].copy_from_slice(&data[..len]);
        self.input = 0;
        self.input_len = 0;
        self.output = 0;
        self.output_len = 0;
        self.dirty = false;
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// Clock one bit in (from a DMA write to the EEPROM window; only D0 is used).
    pub fn write_bit(&mut self, bit: bool) {
        // A write after an output phase begins the next command.
        if self.output_len != 0 {
            self.output = 0;
            self.output_len = 0;
        }
        self.input = (self.input << 1) | bit as u128;
        self.input_len += 1;
    }

    /// Clock one bit out (to a DMA read from the EEPROM window; the bit is in D0).
    /// A read that follows a command burst first executes the command. When no
    /// data is queued the chip reads back ready (`1`).
    pub fn read_bit(&mut self) -> u16 {
        if self.input_len != 0 {
            self.execute();
            self.input = 0;
            self.input_len = 0;
        }
        if self.output_len == 0 {
            return 1; // ready / idle
        }
        self.output_len -= 1;
        ((self.output >> self.output_len) & 1) as u16
    }

    /// Interpret the accumulated command burst.
    fn execute(&mut self) {
        let len = self.input_len;
        // Detect address width from the burst length the game clocked in.
        let bits = if len == 17 || len == 81 { 14 } else { 6 };
        if self.addr_bits == 0 {
            self.set_width(bits);
        }
        let aw = self.addr_bits as u16;
        if len < 2 + aw {
            return; // malformed / partial burst
        }
        let command = (self.input >> (len - 2)) & 0b11;
        let addr = ((self.input >> (len - 2 - aw)) & ((1u128 << aw) - 1)) as usize;
        let block = (addr & (self.data.len() / 8 - 1)) * 8;

        match command {
            0b11 => {
                // Read: 4 leading don't-care bits (left as 0) then 64 data bits.
                let mut out: u128 = 0;
                for i in 0..8 {
                    out = (out << 8) | self.data[block + i] as u128;
                }
                self.output = out; // top 4 of the 68 are 0 (data64 < 2^64)
                self.output_len = 68;
            }
            0b10 => {
                // Write: 64 data bits sit above the single trailing stop bit.
                if len > 2 + aw + 64 {
                    let data64 = ((self.input >> 1) & u64::MAX as u128) as u64;
                    for i in 0..8 {
                        let byte = (data64 >> (56 - i * 8)) as u8;
                        if self.data[block + i] != byte {
                            self.data[block + i] = byte;
                            self.dirty = true;
                        }
                    }
                }
                self.output = 0;
                self.output_len = 0; // subsequent reads poll ready (1)
            }
            _ => {}
        }
    }

    fn set_width(&mut self, addr_bits: u8) {
        self.addr_bits = addr_bits;
        let size = if addr_bits == 14 { SIZE_8K } else { SIZE_512 };
        if self.data.len() != size {
            let mut grown = vec![0xFF; size];
            let keep = self.data.len().min(size);
            grown[..keep].copy_from_slice(&self.data[..keep]);
            self.data = grown;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Clock a big-endian bit sequence of `n` bits into the chip.
    fn write_bits(e: &mut Eeprom, value: u128, n: u32) {
        for i in (0..n).rev() {
            e.write_bit((value >> i) & 1 != 0);
        }
    }

    /// A 6-bit write to block 5, then a read back of the same block.
    #[test]
    fn write_then_read_roundtrip_6bit() {
        let mut e = Eeprom::new();
        let payload: u64 = 0x0123_4567_89AB_CDEF;
        // Write command: 1 0 | addr(6)=5 | data(64) | stop(0)  => 73 bits.
        let mut cmd: u128 = 0b10;
        cmd = (cmd << 6) | 5;
        cmd = (cmd << 64) | payload as u128;
        cmd <<= 1;
        write_bits(&mut e, cmd, 73);
        // The program takes effect on the first read (the game's ready poll).
        assert_eq!(e.read_bit(), 1); // ready
        assert_eq!(e.size(), SIZE_512);

        // Read command: 1 1 | addr(6)=5 | stop(0) => 9 bits.
        let mut rd: u128 = 0b11;
        rd = (rd << 6) | 5;
        rd <<= 1;
        write_bits(&mut e, rd, 9);
        // 4 don't-care bits, then 64 data bits MSB-first.
        for _ in 0..4 {
            e.read_bit();
        }
        let mut got: u64 = 0;
        for _ in 0..64 {
            got = (got << 1) | e.read_bit() as u64;
        }
        assert_eq!(got, payload);
    }

    /// A 14-bit command upgrades the chip to 8 KiB.
    #[test]
    fn fourteen_bit_command_grows_to_8k() {
        let mut e = Eeprom::new();
        // Read command with a 14-bit address (17 bits total) to block 300.
        let mut rd: u128 = 0b11;
        rd = (rd << 14) | 300;
        rd <<= 1;
        write_bits(&mut e, rd, 17);
        e.read_bit(); // executes -> detects 14-bit width
        assert_eq!(e.size(), SIZE_8K);
    }

    #[test]
    fn load_sets_width_from_size() {
        let mut e = Eeprom::new();
        e.load(&vec![0u8; SIZE_8K]);
        assert_eq!(e.size(), SIZE_8K);
        assert_eq!(e.addr_bits, 14);
    }
}
