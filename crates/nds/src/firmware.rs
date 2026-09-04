//! Direct-boot firmware state: the RAM the real firmware/BIOS establishes before
//! it jumps to a cartridge, and which retail games expect to already be present.
//!
//! On a real DS the boot ROM copies the cartridge header to `0x27FFE00`, mirrors
//! the gamecard chip ID and header CRCs into a footer at `0x27FF800`/`0x27FFC00`,
//! sets the inter-core boot handshake words and the "boot indicator", and copies
//! the user's firmware settings (touch calibration, language, clock) to
//! `0x27FFC80`. Direct boot skips that sequence, so games read zeroes and either
//! stall in early startup or reject the (missing) settings. This module rebuilds
//! that state (GBATEK "DS Firmware User Settings" and the `27FFxxx` footer tables).

/// Touch-screen calibration: two `(ADC, screen-pixel)` reference points spanning the
/// panel. The TSC reports raw 12-bit ADC values; games convert them to pixels with
/// these points (linear interpolation, GBATEK). The same constants drive both the
/// settings block games read and [`touch_adc`], which inverts them — so a host touch
/// at pixel `(x, y)` produces the ADC that the game converts back to `(x, y)`.
pub const TOUCH_ADC_X1: u16 = 0x0200;
pub const TOUCH_ADC_Y1: u16 = 0x0200;
pub const TOUCH_SCR_X1: u8 = 0x20;
pub const TOUCH_SCR_Y1: u8 = 0x20;
pub const TOUCH_ADC_X2: u16 = 0x0E00;
pub const TOUCH_ADC_Y2: u16 = 0x0E00;
pub const TOUCH_SCR_X2: u8 = 0xE0;
pub const TOUCH_SCR_Y2: u8 = 0x90;

/// The raw 12-bit ADC pair `(x, y)` a touchscreen controller reports for a touch at
/// screen pixel `(x, y)` — the inverse of the calibration games apply. Clamped to the
/// 12-bit ADC range; coordinates outside the reference span extrapolate linearly.
pub fn touch_adc(x: i32, y: i32) -> (u16, u16) {
    let map = |v: i32, adc1: u16, adc2: u16, scr1: u8, scr2: u8| -> u16 {
        let adc = (v - scr1 as i32) * (adc2 as i32 - adc1 as i32) / (scr2 as i32 - scr1 as i32)
            + adc1 as i32;
        adc.clamp(0, 0xFFF) as u16
    };
    (
        map(x, TOUCH_ADC_X1, TOUCH_ADC_X2, TOUCH_SCR_X1, TOUCH_SCR_X2),
        map(y, TOUCH_ADC_Y1, TOUCH_ADC_Y2, TOUCH_SCR_Y1, TOUCH_SCR_Y2),
    )
}

/// The 0x70-byte "Current Settings" block the firmware copies to RAM `0x27FFC80`
/// (GBATEK "DS Firmware User Settings"). The RAM copy holds only the 0x70 data
/// bytes — the update counter and CRC16 live in flash, not here — so games trust
/// it without re-checksumming. Values are a plausible, already-configured profile
/// (English, valid touch calibration, "settings okay" so no setup prompt).
pub fn user_settings() -> [u8; 0x70] {
    let mut s = [0u8; 0x70];
    let put16 = |s: &mut [u8; 0x70], off: usize, v: u16| {
        s[off..off + 2].copy_from_slice(&v.to_le_bytes());
    };

    put16(&mut s, 0x00, 5); // Version (always 5)
    s[0x02] = 0; // Favourite colour (grey)
    s[0x03] = 1; // Birthday month
    s[0x04] = 1; // Birthday day

    // Nickname "Player" (UTF-16), length 6.
    for (i, ch) in "Player".chars().enumerate() {
        put16(&mut s, 0x06 + i * 2, ch as u16);
    }
    put16(&mut s, 0x1A, 6); // Nickname length

    // Touch-screen calibration: two ADC/screen reference points spanning the panel
    // so ADC values map linearly to pixels (GBATEK conversion formula). The same
    // constants feed `touch_adc`, which inverts this map for injected touches.
    put16(&mut s, 0x58, TOUCH_ADC_X1);
    put16(&mut s, 0x5A, TOUCH_ADC_Y1);
    s[0x5C] = TOUCH_SCR_X1;
    s[0x5D] = TOUCH_SCR_Y1;
    put16(&mut s, 0x5E, TOUCH_ADC_X2);
    put16(&mut s, 0x60, TOUCH_ADC_Y2);
    s[0x62] = TOUCH_SCR_X2;
    s[0x63] = TOUCH_SCR_Y2;

    // Language and flags: English (1), plus the "settings okay" bits (10,11,13,
    // 14,15) with "settings lost" (bit 9) clear, so no user-info / calibration /
    // language prompt and the health-and-safety screen is skipped.
    let settings_ok = (1 << 10) | (1 << 11) | (1 << 13) | (1 << 14) | (1 << 15);
    put16(&mut s, 0x64, 1 | settings_ok);
    s[0x66] = 24; // Year (2024)
                  // 0x68 RTC offset = 0.
    s
}

/// DS CRC16 (GBATEK SWI 0Eh `GetCRC16`, polynomial `0xA001`). Used to checksum the
/// firmware user-settings blocks so the ARM7's firmware read accepts them.
pub fn crc16(initial: u16, data: &[u8]) -> u16 {
    let mut crc = initial;
    for &byte in data {
        crc ^= byte as u16;
        for _ in 0..8 {
            let carry = crc & 1 != 0;
            crc >>= 1;
            if carry {
                crc ^= 0xA001;
            }
        }
    }
    crc
}

/// A minimal 256 KB firmware flash image, as read over SPI by the ARM7. Direct boot
/// skips the firmware, but games still read the touchscreen calibration and other
/// user settings from flash via SPI; a game that finds no valid settings there (an
/// all-zero calibration) skips its touchscreen init, which deadlocks the inter-core
/// boot handshake. The header's User-Settings pointer (`[0x20]` = offset ÷ 8, GBATEK
/// "DS Firmware Header") points at two settings areas at `0x3FE00`/`0x3FF00`, each a
/// `[Self::user_settings]` copy plus an update counter and a CRC16 the game verifies.
pub fn firmware_flash() -> Vec<u8> {
    const SIZE: usize = 0x40000; // 256 KB
    const AREA1: usize = 0x3FE00;
    const AREA2: usize = 0x3FF00;
    let mut flash = vec![0xFF; SIZE];

    // Header: User-Settings offset (÷ 8) → area 1.
    let ptr = (AREA1 / 8) as u16;
    flash[0x20..0x22].copy_from_slice(&ptr.to_le_bytes());

    // Wi-Fi calibration block (0x2A..0x1FF), which the ARM7's Wi-Fi init reads at boot.
    write_wifi_calibration(&mut flash);

    // Wi-Fi Connection (WFC) access-point slots, just below the user-settings area.
    write_wifi_access_points(&mut flash, AREA1);

    let data = user_settings();
    // Two copies for wear-levelling; the game picks the valid one with the higher
    // update counter, so area 1 (counter 1) wins over area 2 (counter 0).
    for (base, counter) in [(AREA1, 1u16), (AREA2, 0u16)] {
        flash[base..base + 0x70].copy_from_slice(&data);
        flash[base + 0x70..base + 0x72].copy_from_slice(&counter.to_le_bytes());
        let crc = crc16(0xFFFF, &data);
        flash[base + 0x72..base + 0x74].copy_from_slice(&crc.to_le_bytes());
    }
    flash
}

/// Write the three Wi-Fi Connection (WFC) access-point slots (GBATEK "DS Firmware
/// Wifi Internet Access Points") as valid but *unconfigured* entries. The slots sit
/// at `user_settings - 0x400/0x300/0x200` (the `0x3FA00/0x3FB00/0x3FC00` region a
/// game copies out of flash). Games that support online read these, checksum each
/// slot's `0xFE` bytes against the CRC16 at offset `0xFE`, and — if the CRC fails —
/// report the Wi-Fi user information as corrupt/erased. Erased (`0xFF`-filled) flash
/// fails that check, so we emit zero-filled slots with `Status = FFh` ("connection
/// not configured") and a matching CRC: a clean "no connections set up" state rather
/// than a corrupt one.
fn write_wifi_access_points(flash: &mut [u8], user_settings_ptr: usize) {
    const SLOT_LEN: usize = 0x100;
    const CRC_LEN: usize = 0xFE; // CRC16 covers bytes 0x00..0xFD.
    const STATUS: usize = 0xE7; // 00h=Normal, 01h=AOSS, FFh=not configured.
    for delta in [0x400, 0x300, 0x200] {
        let base = user_settings_ptr - delta;
        let slot = &mut flash[base..base + SLOT_LEN];
        slot.fill(0x00);
        slot[STATUS] = 0xFF; // connection not configured
        let crc = crc16(0x0000, &slot[..CRC_LEN]);
        slot[CRC_LEN..CRC_LEN + 2].copy_from_slice(&crc.to_le_bytes());
    }
}

/// Fill the firmware's Wi-Fi calibration/settings block (GBATEK "DS Firmware Wifi
/// Calibration Data"), located directly after the header at `0x2A`. The ARM7's Wi-Fi
/// init reads this at boot: it range-checks the `config_length` at `0x2C`, verifies the
/// CRC16 at `0x2A` over the config region, then copies the RF/BB values into the Wi-Fi
/// hardware. A missing block (all `0xFF`) fails the length/CRC check and deadlocks the
/// boot handshake. We emit a plausible, self-consistent block; the exact RF/BB constants
/// only feed (unemulated) Wi-Fi hardware registers, so structural fields and a matching
/// CRC are what matter.
fn write_wifi_calibration(flash: &mut [u8]) {
    // Config region: 0x0138 bytes at 0x2C..0x164 (the usual length), zero-initialized so
    // unused fields read as 00h. 0x163 stays 0xFF per GBATEK (still inside the CRC).
    const CFG: usize = 0x2C;
    const CFG_LEN: usize = 0x0138;
    for b in &mut flash[CFG..CFG + CFG_LEN] {
        *b = 0x00;
    }
    let put16 = |flash: &mut [u8], off: usize, v: u16| {
        flash[off..off + 2].copy_from_slice(&v.to_le_bytes());
    };

    put16(flash, 0x2C, CFG_LEN as u16); // config_length
    flash[0x2F] = 0x00; // version (v1)
    // 48-bit MAC address (Nintendo OUI 00:09:BF + an arbitrary but fixed suffix).
    flash[0x36..0x3C].copy_from_slice(&[0x00, 0x09, 0xBF, 0x12, 0x34, 0x56]);
    put16(flash, 0x3C, 0x3FFE); // enabled channels: 1..13
    put16(flash, 0x3E, 0xFFFF); // flags
    flash[0x40] = 0x02; // RF chip type (NDS: Type2, Mitsumi MM3155)
    flash[0x41] = 0x18; // RF bits per entry at 0xCE (24-bit)
    flash[0x42] = 0x0C; // RF number of entries at 0xCE
    flash[0x43] = 0x01;
    // Channel 1..14 BB[1Eh] values (usually ~0xB1..0xB7); RF[9] bits (usually 0x10).
    for (i, b) in flash[0x146..0x154].iter_mut().enumerate() {
        *b = 0xB1 + (i as u8 % 7);
    }
    for b in &mut flash[0x154..0x162] {
        *b = 0x10;
    }
    flash[0x162] = 0x1C; // unknown per-console calibration byte (usual 0x19..0x1C)
    flash[0x163] = 0xFF; // GBATEK: 0xFF, inside the CRC region

    // CRC16 (initial value 0) over the config region [0x2C .. 0x2C+config_length).
    let crc = crc16(0x0000, &flash[CFG..CFG + CFG_LEN]);
    put16(flash, 0x2A, crc);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wifi_access_point_slots_are_valid_but_unconfigured() {
        // A game reads each 0x100-byte WFC slot and checksums bytes 0x00..0xFD
        // against the CRC16 at 0xFE (initial 0x0000). A stale/erased slot fails the
        // check and triggers "Your Wi-Fi User Information may have been erased." All
        // three slots must validate, and none may read as a configured connection.
        let flash = firmware_flash();
        let user_settings_ptr = 0x3FE00usize; // Header [0x20]*8; matches AREA1.
        for delta in [0x400usize, 0x300, 0x200] {
            let base = user_settings_ptr - delta;
            let slot = &flash[base..base + 0x100];
            let stored = u16::from_le_bytes([slot[0xFE], slot[0xFF]]);
            assert_eq!(crc16(0x0000, &slot[..0xFE]), stored, "slot @{base:#x} CRC");
            assert_eq!(slot[0xE7], 0xFF, "slot @{base:#x} status = not configured");
            assert_eq!(slot[0xEF], 0x00, "slot @{base:#x} no connection configured");
        }
    }
}
