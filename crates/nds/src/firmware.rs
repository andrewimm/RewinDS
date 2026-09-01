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
