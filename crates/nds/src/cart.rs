//! The gamecard (cartridge slot) command controller.
//!
//! After boot, a game reads the rest of its ROM — code overlays, the NitroROM
//! filesystem, graphics — by sending 8-byte commands to the cartridge and
//! streaming the reply through the gamecard data port. This models that bus
//! (GBATEK "DS Cartridge I/O Ports" / "DS Cartridge Protocol"):
//!
//!   - `AUXSPICNT` (`40001A0h`) — slot enable and the transfer-complete IRQ enable.
//!   - `ROMCTRL`   (`40001A4h`) — block size, the `Start/Busy` bit, and the `DRQ`
//!     data-ready status.
//!   - Command out (`40001A8h`, 8 bytes, MSB first) — the command.
//!   - Data in     (`4100010h`, 4 bytes) — the streamed reply, word by word.
//!
//! KEY2 (the random-seed stream cipher) is deliberately not implemented: it is a
//! transparent *hardware* layer — the cartridge stores KEY2-encrypted data and the
//! gamecard controller decrypts it inline, so game software only ever sees
//! plaintext. Our ROM image is already plaintext, so serving it verbatim is
//! byte-correct. KEY2 matters only as a boot-time integrity check, which direct
//! boot bypasses. The seed ports are accepted and ignored.

/// ROMCTRL bit 23: a data word is ready to read (`DRQ`).
const ROMCTRL_DRQ: u32 = 1 << 23;
/// ROMCTRL bit 31: block transfer start / busy.
const ROMCTRL_START: u32 = 1 << 31;

/// What the current transfer streams from the data port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reply {
    /// Sequential little-endian words from ROM starting at this byte address.
    Rom(u32),
    /// A fixed 4-byte word repeated for the whole block (chip ID, HIGH-Z dummy).
    Fixed(u32),
}

/// The gamecard slot: the inserted ROM plus the command/transfer registers.
#[derive(Clone)]
pub struct Cart {
    rom: Box<[u8]>,
    chip_id: u32,
    /// `AUXSPICNT` (`40001A0h`).
    auxspicnt: u16,
    /// `ROMCTRL` (`40001A4h`), minus the volatile `DRQ`/`Start` status bits, which
    /// are derived from [`Cart::words_left`] on read.
    romctrl: u32,
    /// The 8-byte command buffer (`40001A8h`), MSB at index 0.
    command: [u8; 8],
    /// KEY2 seed ports (`40001B0h`..`40001BAh`); stored, never used (see module doc).
    seed: [u32; 2],
    /// The reply source for the in-flight transfer.
    reply: Reply,
    /// Words still to stream this block; zero means idle/complete.
    words_left: u32,
    /// Set when the final word of a block is consumed; the caller then raises the
    /// transfer-complete IRQ and clears it via [`Cart::take_completion`].
    completed: bool,
}

impl Default for Cart {
    fn default() -> Self {
        Cart {
            rom: Box::new([]),
            chip_id: 0,
            auxspicnt: 0,
            romctrl: 0,
            command: [0; 8],
            seed: [0; 2],
            reply: Reply::Fixed(0xFFFF_FFFF),
            words_left: 0,
            completed: false,
        }
    }
}

impl Cart {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a cartridge image and derive its chip ID from the ROM size, the way
    /// the manufacturer/size bytes are encoded (GBATEK "1st Get ROM Chip ID"):
    /// Macronix `C2h`, size byte `(MB-1)`, and the 1T-ROM protocol flag for large
    /// carts (games read it to choose their transfer strategy).
    pub fn insert(&mut self, rom: &[u8]) {
        self.rom = rom.to_vec().into_boxed_slice();
        let size_mb = (rom.len() >> 20).max(1) as u32;
        let size_byte = size_mb.saturating_sub(1).min(0x7F);
        let flags = if size_mb >= 128 { 0x80 } else { 0x00 }; // bit31: new 1T-ROM protocol
        self.chip_id = 0xC2 | (size_byte << 8) | (flags << 24);
    }

    // --- register access ----------------------------------------------------

    pub fn read_auxspicnt(&self) -> u16 {
        self.auxspicnt
    }
    pub fn write_auxspicnt(&mut self, value: u16) {
        self.auxspicnt = value;
    }

    /// `ROMCTRL` with the live `DRQ`/`Start` status bits reflecting the transfer.
    pub fn read_romctrl(&self) -> u32 {
        let mut v = self.romctrl;
        if self.words_left > 0 {
            v |= ROMCTRL_DRQ | ROMCTRL_START;
        }
        v
    }

    /// The stored ROMCTRL configuration, without the momentary/status bits, for
    /// read-modify-write of partial-width register writes.
    pub fn romctrl_config(&self) -> u32 {
        self.romctrl
    }

    /// Write `ROMCTRL`. Setting the `Start` bit (bit 31) launches the transfer for
    /// the current command; returns `true` when a transfer was launched so the
    /// caller can drive any cart-mode DMA and raise the completion IRQ. The start
    /// bit is momentary and is not retained in the stored config.
    pub fn write_romctrl(&mut self, value: u32) -> bool {
        // Bit 15 (apply KEY2 seed), the start bit, and the DRQ status are momentary.
        self.romctrl = value & !((1 << 15) | ROMCTRL_START | ROMCTRL_DRQ);
        if value & ROMCTRL_START == 0 {
            return false;
        }
        self.begin_transfer();
        true
    }

    /// One byte of the command buffer (`40001A8h`+`index`).
    pub fn write_command_byte(&mut self, index: usize, byte: u8) {
        if index < 8 {
            self.command[index] = byte;
        }
    }

    pub fn write_seed(&mut self, index: usize, value: u32) {
        if index < 2 {
            self.seed[index] = value;
        }
    }

    // --- transfer -----------------------------------------------------------

    /// The block length ROMCTRL bits 24-26 select: `0`=none, `1..6`=`100h<<n`,
    /// `7`=4 bytes.
    fn block_bytes(&self) -> u32 {
        match (self.romctrl >> 24) & 7 {
            0 => 0,
            7 => 4,
            n => 0x100 << n,
        }
    }

    /// Decode the command and arm the reply stream.
    fn begin_transfer(&mut self) {
        let block = self.block_bytes();
        // The command address parameter (bytes 1..5, MSB first).
        let addr = u32::from_be_bytes([
            self.command[1],
            self.command[2],
            self.command[3],
            self.command[4],
        ]);
        self.reply = match self.command[0] {
            0xB7 | 0x00 => Reply::Rom(addr), // encrypted data read / get header
            0x90 | 0xB8 => Reply::Fixed(self.chip_id), // get ROM chip ID
            // Mode switches (KEY1/KEY2 activate, enter main data) and unknown
            // commands stream HIGH-Z (FFh); a directly-booted game rarely issues
            // them, but they must not hang.
            _ => Reply::Fixed(0xFFFF_FFFF),
        };
        self.words_left = block / 4;
        // A zero-length block completes immediately (mode-switch commands).
        self.completed = self.words_left == 0;
    }

    /// Read one 32-bit word from the data port (`4100010h`), advancing the stream.
    /// The final word of a block sets the completion flag.
    pub fn read_data(&mut self) -> u32 {
        if self.words_left == 0 {
            return 0;
        }
        let word = match self.reply {
            Reply::Rom(addr) => {
                let a = addr as usize;
                let w = self
                    .rom
                    .get(a..a + 4)
                    .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .unwrap_or(0xFFFF_FFFF);
                self.reply = Reply::Rom(addr.wrapping_add(4)); // advance the cursor
                w
            }
            Reply::Fixed(w) => w,
        };
        self.words_left -= 1;
        if self.words_left == 0 {
            self.completed = true;
        }
        word
    }

    /// Take the pending block-completion signal (raise the IRQ when it returns
    /// `true`, gated by `AUXSPICNT` bit 14).
    pub fn take_completion(&mut self) -> bool {
        core::mem::take(&mut self.completed)
    }

    /// Whether `AUXSPICNT` enables the transfer-complete IRQ (bit 14).
    pub fn transfer_irq_enabled(&self) -> bool {
        self.auxspicnt & (1 << 14) != 0
    }
}

/// The gamecard data port (`4100010h`), a fixed DMA source for cart-mode DMA.
pub const DATA_PORT: u32 = 0x0410_0010;

#[cfg(test)]
mod tests {
    use super::*;

    /// A ROMCTRL value selecting a `bytes`-length block with the Start bit set.
    fn start_block(bytes: u32) -> u32 {
        let field = match bytes {
            4 => 7,
            _ => (bytes / 0x100).trailing_zeros(), // 0x200->1 .. 0x4000->6
        };
        ROMCTRL_START | (field << 24)
    }

    fn cart_with_rom(len: usize) -> Cart {
        let mut rom = vec![0u8; len];
        for (i, b) in rom.iter_mut().enumerate() {
            *b = i as u8;
        }
        let mut cart = Cart::new();
        cart.insert(&rom);
        cart
    }

    #[test]
    fn read_command_streams_rom_words_then_completes() {
        let mut cart = cart_with_rom(0x1_0000);
        // B7 read from 0x8000, 0x200-byte block.
        cart.command = [0xB7, 0x00, 0x00, 0x80, 0x00, 0, 0, 0];
        let started = cart.write_romctrl(start_block(0x200));
        assert!(started);
        assert_eq!(cart.read_romctrl() & ROMCTRL_START, ROMCTRL_START, "busy");
        // ROM is filled with `i as u8`, so ROM[0x8000..0x8004] = 00 01 02 03.
        let expect0 = u32::from_le_bytes([0x00, 0x01, 0x02, 0x03]);
        assert_eq!(cart.read_data(), expect0);
        // Drain the remaining 0x200/4 - 1 words.
        for _ in 0..(0x200 / 4 - 1) {
            assert!(!cart.completed);
            cart.read_data();
        }
        assert!(cart.take_completion(), "final word signals completion");
        assert_eq!(cart.read_romctrl() & ROMCTRL_START, 0, "no longer busy");
        assert_eq!(cart.read_data(), 0, "idle port reads 0");
    }

    #[test]
    fn chip_id_repeats_every_four_bytes() {
        let mut cart = cart_with_rom(16 << 20); // 16 MB -> size byte 0x0F
        assert_eq!(cart.chip_id & 0xFF, 0xC2);
        assert_eq!((cart.chip_id >> 8) & 0xFF, 0x0F);
        cart.command = [0xB8, 0, 0, 0, 0, 0, 0, 0];
        cart.write_romctrl(start_block(4)); // 4-byte block
        assert_eq!(cart.read_data(), cart.chip_id);
        assert!(cart.take_completion());
    }

    #[test]
    fn zero_length_mode_switch_completes_without_data() {
        let mut cart = cart_with_rom(0x1000);
        cart.command = [0x3C, 0, 0, 0, 0, 0, 0, 0]; // activate KEY1: no data
        let started = cart.write_romctrl(ROMCTRL_START); // block size field 0
        assert!(started);
        assert!(cart.take_completion());
        assert_eq!(cart.read_romctrl() & ROMCTRL_START, 0);
    }

    #[test]
    fn out_of_range_rom_reads_open_bus() {
        let mut cart = cart_with_rom(0x8000);
        cart.command = [0xB7, 0x00, 0x10, 0x00, 0x00, 0, 0, 0]; // addr 0x100000 > len
        cart.write_romctrl(start_block(0x200));
        assert_eq!(cart.read_data(), 0xFFFF_FFFF);
    }

    #[test]
    fn large_cart_sets_1t_rom_protocol_flag() {
        let cart = cart_with_rom(128 << 20);
        assert_eq!(cart.chip_id >> 31, 1, "128 MB cart flags new protocol");
    }
}
