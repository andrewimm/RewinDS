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

    // Touch-screen calibration: two ADC/screen reference points spanning the
    // panel so ADC values map linearly to pixels (GBATEK conversion formula).
    put16(&mut s, 0x58, 0x0200); // adc.x1
    put16(&mut s, 0x5A, 0x0200); // adc.y1
    s[0x5C] = 0x20; // scr.x1
    s[0x5D] = 0x20; // scr.y1
    put16(&mut s, 0x5E, 0x0E00); // adc.x2
    put16(&mut s, 0x60, 0x0E00); // adc.y2
    s[0x62] = 0xE0; // scr.x2 (near 224)
    s[0x63] = 0x90; // scr.y2 (near 144)

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
