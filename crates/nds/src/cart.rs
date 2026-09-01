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

/// Serial-FLASH backup size. Retail games use up to 8 Mbit; NSMB streams two save
/// banks at `0x000000` and `0x100000`, so the chip must be ≥ 2 MB. A power of two, so
/// an address wraps with a mask.
const BACKUP_SIZE: usize = 2 * 1024 * 1024;
/// The `RDID` (0x9F) reply: a plausible 4 Mbit flash JEDEC id. Retail games that hard-
/// code their save type never read it; it's here for the ones that probe.
const JEDEC_ID: [u8; 3] = [0x20, 0x40, 0x12];

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
    /// Backup-SPI (`AUXSPIDATA`, `40001A2h`) state machine over a serial FLASH chip:
    /// the in-flight command, the address it accumulates over three bytes, how many
    /// bytes into the transfer we are, the write-enable latch, and the byte last
    /// clocked back. `backup` is the chip's contents (erased state `0xFF`); it is the
    /// game's save data, persisted by the host.
    backup: Vec<u8>,
    backup_command: u8,
    backup_addr: u32,
    backup_phase: u32,
    backup_wel: bool,
    backup_in_transfer: bool,
    backup_dirty: bool,
    backup_data: u8,
    /// How many address bytes the backup command stream carries. The DS header has no
    /// save-type field, so we auto-detect the chip class from its protocol: EEPROMs
    /// (2-byte addressing) are the common case, while a serial FLASH is identified by
    /// its `RDID` (`0x9F`) probe — which every FLASH driver issues to read the JEDEC id
    /// before any save access — and uses 3-byte addressing. We default to 2 and latch
    /// to 3 on the first `RDID`. Getting this wrong shifts the first data byte into the
    /// address, so a written record can never be read back (NSMB: "could not erase
    /// data"). (A rare 512-byte EEPROM uses 1 address byte; not yet auto-detected.)
    backup_addr_bytes: u32,
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
            backup: vec![0xFF; BACKUP_SIZE],
            backup_command: 0,
            backup_addr: 0,
            backup_phase: 0,
            backup_wel: false,
            backup_in_transfer: false,
            backup_dirty: false,
            backup_data: 0,
            backup_addr_bytes: 2,
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

    /// The cartridge's ROM chip ID (the boot-info footer mirrors it into RAM).
    pub fn chip_id(&self) -> u32 {
        self.chip_id
    }

    // --- register access ----------------------------------------------------

    pub fn read_auxspicnt(&self) -> u16 {
        self.auxspicnt
    }
    pub fn write_auxspicnt(&mut self, value: u16) {
        self.auxspicnt = value;
    }

    /// The byte last clocked back from the backup chip (`AUXSPIDATA` read).
    pub fn read_auxspidata(&self) -> u8 {
        self.backup_data
    }

    /// Clock a byte to the backup FLASH chip and latch the byte clocked back. The
    /// first byte of a transfer selects the command; the rest carry the address
    /// (three bytes) and then data. `WREN`/`WRDI` (`06`/`04`) toggle the write-enable
    /// latch; `RDSR` (`05`) reports it; `READ` (`03`) streams stored bytes; `PP`/`PW`
    /// (`02`/`0A`) store bytes when write-enabled; `RDID` (`9F`) reports the chip id.
    /// Chip-select hold is `AUXSPICNT` bit 6; releasing it ends the transfer (and
    /// clears the latch after a write, as the hardware does).
    pub fn write_auxspidata(&mut self, value: u8) {
        if !self.backup_in_transfer {
            self.backup_command = value;
            self.backup_phase = 0;
            self.backup_addr = 0;
            self.backup_in_transfer = true;
            self.backup_data = 0;
            match value {
                0x06 => self.backup_wel = true,  // WREN
                0x04 => self.backup_wel = false, // WRDI
                // A FLASH driver reads the JEDEC id before any save access; that probe
                // identifies the chip as 3-byte-addressed serial FLASH.
                0x9F | 0x9E => self.backup_addr_bytes = 3,
                _ => {}
            }
        } else {
            self.backup_phase += 1;
            let phase = self.backup_phase;
            self.backup_data = match self.backup_command {
                0x05 => (self.backup_wel as u8) << 1, // RDSR: WIP=0 (ready), WEL in bit 1
                0x9F | 0x9E => JEDEC_ID[((phase - 1) % 3) as usize],
                0x03 => {
                    if phase <= self.backup_addr_bytes {
                        self.backup_addr = (self.backup_addr << 8) | value as u32;
                        0
                    } else {
                        let a = self.backup_addr as usize & (BACKUP_SIZE - 1);
                        self.backup_addr = self.backup_addr.wrapping_add(1);
                        self.backup[a]
                    }
                }
                0x02 | 0x0A => {
                    if phase <= self.backup_addr_bytes {
                        self.backup_addr = (self.backup_addr << 8) | value as u32;
                    } else if self.backup_wel {
                        let a = self.backup_addr as usize & (BACKUP_SIZE - 1);
                        self.backup[a] = value;
                        self.backup_addr = self.backup_addr.wrapping_add(1);
                        self.backup_dirty = true;
                    }
                    0
                }
                _ => 0,
            };
        }
        if self.auxspicnt & (1 << 6) == 0 {
            if matches!(self.backup_command, 0x02 | 0x0A) {
                self.backup_wel = false; // a program completes and disarms the latch
            }
            self.backup_in_transfer = false;
            self.backup_command = 0;
        }
        #[cfg(feature = "cyctrace")]
        {
            let pc = crate::system::cyctrace::CUR_PC[1]
                .load(std::sync::atomic::Ordering::Relaxed);
            crate::system::cyctrace::aux_log(
                pc,
                value,
                self.backup_data,
                self.backup_in_transfer as u8,
            );
        }
    }

    /// The backup (save) contents, for the host to persist.
    pub fn backup_bytes(&self) -> &[u8] {
        &self.backup
    }

    /// Restore previously saved backup contents (truncated/padded to the chip size).
    pub fn load_backup(&mut self, data: &[u8]) {
        let n = data.len().min(BACKUP_SIZE);
        self.backup[..n].copy_from_slice(&data[..n]);
        self.backup_dirty = false;
    }

    /// Whether the backup has been written since the last [`Self::clear_backup_dirty`].
    pub fn backup_dirty(&self) -> bool {
        self.backup_dirty
    }
    pub fn clear_backup_dirty(&mut self) {
        self.backup_dirty = false;
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

    /// Clock a full backup-SPI transfer: hold chip-select for every byte but the
    /// last. Returns the byte clocked back for each byte sent.
    fn spi(cart: &mut Cart, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, &b) in bytes.iter().enumerate() {
            let hold = if i + 1 == bytes.len() { 0 } else { 1 << 6 };
            cart.write_auxspicnt(hold);
            cart.write_auxspidata(b);
            out.push(cart.read_auxspidata());
        }
        out
    }

    /// A programmed backup byte reads back through the FLASH (3-byte address) machine.
    #[test]
    fn backup_flash_write_then_read_is_coherent() {
        let mut cart = Cart::new();
        // RDID identifies a serial FLASH → 3-byte addressing.
        spi(&mut cart, &[0x9F, 0, 0, 0]);
        spi(&mut cart, &[0x06]); // WREN
        // PP at 0x00110040, three data bytes 0xAA 0xBB 0xCC.
        spi(&mut cart, &[0x02, 0x11, 0x00, 0x40, 0xAA, 0xBB, 0xCC]);
        // READ at 0x00110040 (command + 3 address + 3 dummy read bytes).
        let r = spi(&mut cart, &[0x03, 0x11, 0x00, 0x40, 0, 0, 0]);
        assert_eq!(&r[4..], &[0xAA, 0xBB, 0xCC], "written bytes must read back");
        // Without WREN, a program is ignored (write-enable latch clears after PP).
        spi(&mut cart, &[0x02, 0x11, 0x00, 0x40, 0x11]);
        let r = spi(&mut cart, &[0x03, 0x11, 0x00, 0x40, 0]);
        assert_eq!(r[4], 0xAA, "a program without WREN must not take effect");
    }

    /// An EEPROM cart (no RDID probe) uses 2-byte addressing: the byte after two
    /// address bytes is data, not a third address byte. This is the NSMB case — a
    /// 3-byte decode would swallow the first data byte and fail read-back.
    #[test]
    fn backup_eeprom_uses_two_byte_addressing() {
        let mut cart = Cart::new();
        spi(&mut cart, &[0x06]); // WREN
        // WRITE at 0x1100: header byte 0xCD then a signature, exactly like NSMB.
        spi(&mut cart, &[0x02, 0x11, 0x00, 0xCD, b'M', b'a', b'r', b'i', b'o']);
        // The game verifies by reading address 0x1100 back.
        let r = spi(&mut cart, &[0x03, 0x11, 0x00, 0, 0, 0]);
        assert_eq!(
            &r[3..],
            &[0xCD, b'M', b'a'],
            "2-byte-addressed data must read back at the same address"
        );
    }

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
