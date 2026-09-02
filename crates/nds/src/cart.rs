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

/// Whether a backup command programs or erases the chip — the ops that clear the
/// write-enable latch on completion (`WRITE`/`WRHI`, page/sector/chip erase, `WRSR`).
fn is_write_or_erase(command: u8) -> bool {
    matches!(command, 0x02 | 0x0A | 0xDB | 0xD8 | 0xC7 | 0x62 | 0x01)
}

/// Whether `REWINDS_BACKUP_TRACE` is set — gates one stderr line per backup-SPI
/// transaction (command, address, byte count, detected address width) for diagnosing
/// save-detection failures. Read once and cached.
fn backup_trace_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("REWINDS_BACKUP_TRACE").is_some())
}

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
    /// save-type field, so we auto-detect the chip class from its protocol (GBATEK "DS
    /// Cartridge Backup"). We default to 2 (the common 8K/64K EEPROM) and latch:
    ///
    /// - to **3** on the first `RDID` (`0x9F`) — the JEDEC-id probe every serial-FLASH
    ///   driver issues before any save access (and which EEPROMs never use);
    /// - to **1** on `RDHI` (`0x0B`) — a read command that exists only on the 0.5K
    ///   EEPROM, whose 9th address bit (A8) is carried in the command opcode.
    ///
    /// Getting the width wrong shifts the first data byte into the address, so a written
    /// record can never be read back (NSMB: "could not erase data"). A 0.5K EEPROM whose
    /// very first access is a *low*-page op (never touching the high page) can't be told
    /// from a 2-byte chip without tracing the program counter, and stays at the default.
    backup_addr_bytes: u32,
    /// This cart shares its SPI bus between the backup chip and an infrared transceiver
    /// (GBATEK "DS Cart Infrared Cartridge SPI Commands"; e.g. Pokémon HG/SS, B/W). On
    /// such carts every savedata command is prefixed with a `00h` byte, and other leading
    /// bytes are infrared operations that must not reach the backup chip. Latched the
    /// first time a `00h` prefix is seen; never cleared.
    backup_ir: bool,
    /// A `00h` prefix was just consumed — the next byte is the real savedata command.
    backup_await_command: bool,
    /// The current transfer is an infrared operation (non-`00h` prefix on an IR cart):
    /// its bytes are stubbed so they can neither read nor corrupt the backup chip.
    backup_ir_op: bool,
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
            backup_ir: false,
            backup_await_command: false,
            backup_ir_op: false,
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

    /// Clock a byte to the backup chip and latch the byte clocked back. The first byte
    /// of a transfer selects the command; the rest carry the address (1-3 bytes,
    /// auto-detected — see [`Cart::backup_addr_bytes`]) and then data. `WREN`/`WRDI`
    /// (`06`/`04`) toggle the write-enable latch; `RDSR` (`05`) reports it; `READ`
    /// (`03`, plus `0B` = high page on the 0.5K EEPROM) streams stored bytes; `WRITE`
    /// (`02`, plus `0A` = high page / FLASH write+erase) stores bytes when write-enabled;
    /// FLASH erases (`DB` page, `D8` sector, `C7`/`62` chip) reset a region to `FFh`;
    /// `RDID` (`9F`) reports the chip id. Chip-select hold is `AUXSPICNT` bit 6; releasing
    /// it ends the transfer (and clears the latch after a write/erase, as hardware does).
    pub fn write_auxspidata(&mut self, value: u8) {
        if !self.backup_in_transfer {
            self.backup_in_transfer = true;
            self.backup_data = 0;
            if value == 0x00 {
                // IR-cart savedata prefix: the real backup command is the next byte.
                // The first prefix also identifies the cart as IR-type. Every known IR
                // cart (Pokémon HG/SS, B/W; Walk with Me) uses serial FLASH with 24-bit
                // addressing, and its flash driver skips the RDID probe that would
                // otherwise reveal the width — so default to 3 bytes here rather than
                // mangling every command by one byte against the 2-byte EEPROM default.
                if !self.backup_ir {
                    self.backup_ir = true;
                    self.backup_addr_bytes = 3;
                }
                self.backup_await_command = true;
            } else if self.backup_ir {
                // A non-00h leading byte on an IR cart is an infrared operation (RX/TX/
                // status), not savedata — stub it so it can never touch the backup chip.
                self.backup_ir_op = true;
            } else {
                self.start_backup_command(value);
            }
        } else if self.backup_await_command {
            // The byte after a 00h prefix is the real savedata command.
            self.backup_await_command = false;
            self.start_backup_command(value);
        } else if self.backup_ir_op {
            self.backup_data = 0; // infrared payload: ignored
        } else {
            self.backup_phase += 1;
            let phase = self.backup_phase;
            self.backup_data = match self.backup_command {
                0x05 => self.status_register(), // RDSR
                0x9F | 0x9E => JEDEC_ID[((phase - 1) % 3) as usize], // RDID
                // Reads: RDLO (03h) low page, RDHI (0Bh) high page (0.5K EEPROM only).
                0x03 | 0x0B => {
                    if phase <= self.backup_addr_bytes {
                        self.backup_addr = (self.backup_addr << 8) | value as u32;
                        if phase == self.backup_addr_bytes {
                            self.latch_high_page();
                        }
                        0
                    } else {
                        let a = self.backup_addr as usize & (BACKUP_SIZE - 1);
                        self.backup_addr = self.backup_addr.wrapping_add(1);
                        self.backup[a]
                    }
                }
                // Writes: WRLO (02h)/PP low page, WRHI (0Ah)/PW high page. For FLASH, 0Ah
                // is write+erase and 02h a plain program; both just store bytes here — an
                // overwrite is always valid, so the erase-before-write rule isn't modeled.
                0x02 | 0x0A => {
                    if phase <= self.backup_addr_bytes {
                        self.backup_addr = (self.backup_addr << 8) | value as u32;
                        if phase == self.backup_addr_bytes {
                            self.latch_high_page();
                        }
                    } else if self.backup_wel {
                        let a = self.backup_addr as usize & (BACKUP_SIZE - 1);
                        self.backup[a] = value;
                        self.backup_addr = self.backup_addr.wrapping_add(1);
                        self.backup_dirty = true;
                    }
                    0
                }
                // FLASH erase: page (DBh, 256 B) / sector (D8h, 64 KB). Once the address
                // is clocked in, the region is reset to the erased state (FFh).
                0xDB | 0xD8 => {
                    self.backup_addr = (self.backup_addr << 8) | value as u32;
                    if phase == self.backup_addr_bytes {
                        self.erase_region(self.backup_command);
                    }
                    0
                }
                _ => 0,
            };
        }
        if self.auxspicnt & (1 << 6) == 0 {
            if is_write_or_erase(self.backup_command) {
                self.backup_wel = false; // a program/erase completes and disarms the latch
            }
            if backup_trace_enabled() {
                eprintln!(
                    "[backup] cmd={:02X} addr={:05X} phase={} addr_bytes={} ir={} irop={} wel={}",
                    self.backup_command,
                    self.backup_addr & (BACKUP_SIZE as u32 - 1),
                    self.backup_phase,
                    self.backup_addr_bytes,
                    self.backup_ir as u8,
                    self.backup_ir_op as u8,
                    self.backup_wel as u8,
                );
            }
            self.backup_in_transfer = false;
            self.backup_await_command = false;
            self.backup_ir_op = false;
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

    /// Begin a backup command: latch it, reset the address/phase, and apply the
    /// command-time effects (the write-enable latch, address-width auto-detection, and
    /// the address-less chip erase). Reached for a plain command or, on an IR cart, for
    /// the command byte that follows a `00h` savedata prefix.
    fn start_backup_command(&mut self, value: u8) {
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
            // RDHI exists only on the 0.5K EEPROM (address bit A8 in the opcode); seeing
            // it identifies a 1-byte-addressed chip. Never downgrade FLASH.
            0x0B if self.backup_addr_bytes != 3 => self.backup_addr_bytes = 1,
            // Chip/bulk erase carries no address, so it runs at command time.
            0xC7 | 0x62 => self.erase_chip(),
            _ => {}
        }
    }

    /// The RDSR (`05h`) reply: WIP = 0 (always ready) and WEL in bit 1. A 0.5K EEPROM
    /// (1-byte address) additionally reads bits 4-7 as 1 — the `F0h` signature that
    /// identifies its narrow address bus (GBATEK "Detection by examining responses").
    fn status_register(&self) -> u8 {
        let mut s = (self.backup_wel as u8) << 1;
        if self.backup_addr_bytes == 1 {
            s |= 0xF0;
        }
        s
    }

    /// Fold in the 0.5K EEPROM's 9th address bit once the 1-byte address is latched: the
    /// high-page opcodes RDHI/WRHI (`0B`/`0A`) select `100h-1FFh`, RDLO/WRLO (`03`/`02`)
    /// the low page. A no-op for wider chips (the address already holds every bit).
    fn latch_high_page(&mut self) {
        if self.backup_addr_bytes == 1 && matches!(self.backup_command, 0x0B | 0x0A) {
            self.backup_addr |= 0x100;
        }
    }

    /// Reset a FLASH page (`DBh`, 256 B) or sector (`D8h`, 64 KB) around the latched
    /// address to the erased state (`FFh`), if write-enabled.
    fn erase_region(&mut self, command: u8) {
        if !self.backup_wel {
            return;
        }
        let size = if command == 0xDB { 0x100 } else { 0x1_0000 };
        let start = (self.backup_addr as usize & (BACKUP_SIZE - 1)) & !(size - 1);
        let end = (start + size).min(BACKUP_SIZE);
        self.backup[start..end].fill(0xFF);
        self.backup_dirty = true;
    }

    /// Reset the whole chip to the erased state (`FFh`) — FLASH bulk/chip erase
    /// (`C7h`/`62h`), which carries no address — if write-enabled.
    fn erase_chip(&mut self) {
        if !self.backup_wel {
            return;
        }
        self.backup.fill(0xFF);
        self.backup_dirty = true;
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

    /// A 0.5K EEPROM (1-byte address) is identified by its high-page read command
    /// (RDHI, 0Bh), reports RDSR=F0h, and carries the 9th address bit in the opcode so
    /// the low page (03h/02h) and high page (0Bh/0Ah) don't collide.
    #[test]
    fn backup_small_eeprom_detects_and_addresses_high_page() {
        let mut cart = Cart::new();
        // A high-page read identifies the 1-byte-addressed 0.5K EEPROM.
        spi(&mut cart, &[0x0B, 0x00, 0]);
        // RDSR now carries the F0h signature (bits 4-7 set).
        assert_eq!(spi(&mut cart, &[0x05, 0])[1] & 0xF0, 0xF0);

        spi(&mut cart, &[0x06]); // WREN
        // WRLO 0x00 = 0xAA (low page), WRHI 0x00 = 0xBB (high page, addr 0x100).
        spi(&mut cart, &[0x02, 0x00, 0xAA]);
        spi(&mut cart, &[0x06]); // WREN again (a write cleared the latch)
        spi(&mut cart, &[0x0A, 0x00, 0xBB]);
        // The two 0x00 offsets must not collide: low reads 0xAA, high reads 0xBB.
        assert_eq!(spi(&mut cart, &[0x03, 0x00, 0])[2], 0xAA, "low page");
        assert_eq!(spi(&mut cart, &[0x0B, 0x00, 0])[2], 0xBB, "high page (A8 in opcode)");
    }

    /// A FLASH sector erase resets its 64 KB region to FFh; a program then writes into it.
    #[test]
    fn backup_flash_erase_resets_region_to_ff() {
        let mut cart = Cart::new();
        spi(&mut cart, &[0x9F, 0, 0, 0]); // RDID → FLASH, 3-byte address
        spi(&mut cart, &[0x06]);
        spi(&mut cart, &[0x02, 0x01, 0x00, 0x00, 0x42]); // program 0x42 at 0x010000
        assert_eq!(spi(&mut cart, &[0x03, 0x01, 0x00, 0x00, 0])[4], 0x42);
        // Sector-erase the 64 KB region containing 0x010000.
        spi(&mut cart, &[0x06]);
        spi(&mut cart, &[0xD8, 0x01, 0x00, 0x00]);
        assert_eq!(spi(&mut cart, &[0x03, 0x01, 0x00, 0x00, 0])[4], 0xFF, "erased to FFh");
        // A neighbouring sector is untouched.
        spi(&mut cart, &[0x06]);
        spi(&mut cart, &[0x02, 0x02, 0x00, 0x00, 0x99]); // 0x020000, different sector
        spi(&mut cart, &[0x06]);
        spi(&mut cart, &[0xD8, 0x01, 0x00, 0x00]); // erase sector 0x01xxxx again
        assert_eq!(spi(&mut cart, &[0x03, 0x02, 0x00, 0x00, 0])[4], 0x99, "other sector kept");
    }

    /// An IR cart (Pokémon HG/SS, B/W) shares the SPI bus between an infrared chip and
    /// the save flash: every savedata command is prefixed with a 00h byte, and other
    /// leading bytes are infrared ops that must not touch the flash.
    #[test]
    fn backup_ir_cart_passes_through_00_prefixed_savedata_only() {
        let mut cart = Cart::new();
        // 00h-prefixed savedata: RDID (→FLASH, 3-byte), WREN, program 0x77 at 0x010000.
        spi(&mut cart, &[0x00, 0x9F, 0, 0, 0]);
        spi(&mut cart, &[0x00, 0x06]);
        spi(&mut cart, &[0x00, 0x02, 0x01, 0x00, 0x00, 0x77]);
        let r = spi(&mut cart, &[0x00, 0x03, 0x01, 0x00, 0x00, 0]);
        assert_eq!(r[5], 0x77, "00h-prefixed savedata must reach the flash");

        // An infrared op (non-00h prefix) must not corrupt the flash, even mid write-enable.
        spi(&mut cart, &[0x00, 0x06]); // WREN
        spi(&mut cart, &[0x02, 0xDE, 0xAD, 0xBE]); // IR TX-shaped op, not savedata
        let r = spi(&mut cart, &[0x00, 0x03, 0x01, 0x00, 0x00, 0]);
        assert_eq!(r[5], 0x77, "an infrared operation must not write the flash");
    }

    /// IR carts use 24-bit FLASH addressing but their drivers skip the RDID probe, so
    /// merely detecting the cart (the 00h prefix) must default the width to 3 bytes —
    /// otherwise every command is mangled by one byte (Pokémon Black: "can't write save").
    #[test]
    fn backup_ir_cart_defaults_to_three_byte_flash_addressing() {
        let mut cart = Cart::new();
        // No RDID anywhere — just 00h-prefixed savedata, as Pokémon Black does.
        spi(&mut cart, &[0x00, 0x06]); // WREN
        spi(&mut cart, &[0x00, 0x02, 0x01, 0x23, 0x45, 0xA5]); // program 0xA5 at 0x012345
        let r = spi(&mut cart, &[0x00, 0x03, 0x01, 0x23, 0x45, 0]); // read back, 3-byte addr
        assert_eq!(r[5], 0xA5, "IR cart must use 3-byte addressing without an RDID probe");
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
