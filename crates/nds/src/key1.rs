//! KEY1 cartridge encryption (Blowfish), used to decrypt the ROM secure area.
//!
//! KEY1 is Bruce Schneier's Blowfish keyed only by the cartridge gamecode; its one
//! external input is the 0x1048-byte key table found in the ARM7 BIOS at
//! `0x30..0x1078`. It appears in three places (GBATEK "DS Cartridges, Encryption,
//! Firmware"): decrypting the ROM secure area, encrypting the KEY1 gamecart
//! commands, and firmware decryption. Only the first is needed for direct boot —
//! the ARM9 boot code of a commercial ROM lives inside the secure area, whose first
//! 2 KB ship KEY1-encrypted.
//!
//! The secondary random-seed stream cipher (KEY2) is a separate, bus-transport
//! layer that the ROM *image* is never subject to, so it lives with the future
//! cartridge command controller rather than here.

/// Byte offset of the key table within the ARM7 BIOS image.
pub const KEYTABLE_BIOS_OFFSET: usize = 0x30;
/// Length of the KEY1 key table (Blowfish P-array + four S-boxes).
pub const KEYTABLE_LEN: usize = 0x1048;
/// The key buffer as 32-bit words: 18 P-array entries + 4 * 256 S-box entries.
const KEYBUF_WORDS: usize = KEYTABLE_LEN / 4; // 0x412

/// ROM offset of the secure area (`4000h..7FFFh`).
pub const SECURE_AREA_START: usize = 0x4000;
/// Length of the KEY1-encrypted first block of the secure area (first 2 KB).
pub const SECURE_AREA_ENC_LEN: usize = 0x800;

/// Raw secure-area ID before encryption / after decryption (GBATEK).
const ENCRYOBJ: [u8; 8] = *b"encryObj";
/// The value the BIOS writes over the ID after a successful verify (`E7FFDEFFh`
/// twice, little-endian).
const DESTROYED: [u8; 8] = [0xFF, 0xDE, 0xFF, 0xE7, 0xFF, 0xDE, 0xFF, 0xE7];

/// A KEY1 Blowfish context: the expanded key buffer plus the 12-byte keycode.
pub struct Key1 {
    keybuf: Box<[u32; KEYBUF_WORDS]>,
    keycode: [u32; 3],
}

impl Key1 {
    /// Build a context from the BIOS key table and a gamecode, applying the keycode
    /// `level` times (`init_keycode`, GBATEK). `modulo` is 8 for gamecart use.
    pub fn new(keytable: &[u8], idcode: u32, level: u8, modulo: usize) -> Self {
        assert!(keytable.len() >= KEYTABLE_LEN);
        let mut keybuf = Box::new([0u32; KEYBUF_WORDS]);
        for (w, chunk) in keytable[..KEYTABLE_LEN].as_chunks::<4>().0.iter().enumerate() {
            keybuf[w] = u32::from_le_bytes(*chunk);
        }
        let mut key1 = Key1 {
            keybuf,
            keycode: [idcode, idcode / 2, idcode.wrapping_mul(2)],
        };
        if level >= 1 {
            key1.apply_keycode(modulo);
        }
        if level >= 2 {
            key1.apply_keycode(modulo);
        }
        key1.keycode[1] = key1.keycode[1].wrapping_mul(2);
        key1.keycode[2] /= 2;
        if level >= 3 {
            key1.apply_keycode(modulo);
        }
        key1
    }

    /// The Blowfish round function over a 64-bit value held as `(word0, word1)`
    /// = `([ptr+0], [ptr+4])`. Encryption runs the P-array forwards (P0..P15 then
    /// P16/P17 whitening); decryption runs it in reverse (P17..P2 then P1/P0).
    fn crypt(&self, mut y: u32, mut x: u32, decrypt: bool) -> (u32, u32) {
        let kb = &self.keybuf;
        let rounds: [usize; 16] = if decrypt {
            [
                0x11, 0x10, 0xF, 0xE, 0xD, 0xC, 0xB, 0xA, 0x9, 0x8, 0x7, 0x6, 0x5, 0x4, 0x3, 0x2,
            ]
        } else {
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0xA, 0xB, 0xC, 0xD, 0xE, 0xF]
        };
        for &i in &rounds {
            let z = kb[i] ^ x;
            let mut xx = kb[0x012 + ((z >> 24) & 0xFF) as usize];
            xx = kb[0x112 + ((z >> 16) & 0xFF) as usize].wrapping_add(xx);
            xx ^= kb[0x212 + ((z >> 8) & 0xFF) as usize];
            xx = kb[0x312 + (z & 0xFF) as usize].wrapping_add(xx);
            x = y ^ xx;
            y = z;
        }
        if decrypt {
            (x ^ kb[0x1], y ^ kb[0x0])
        } else {
            (x ^ kb[0x10], y ^ kb[0x11])
        }
    }

    /// Fold the keycode into the key buffer (`apply_keycode`, GBATEK): encrypt the
    /// keycode, XOR the byte-reversed keycode across the P-array, then re-key every
    /// word by encrypting a running zero block.
    fn apply_keycode(&mut self, modulo: usize) {
        let (a, b) = self.crypt(self.keycode[1], self.keycode[2], false);
        self.keycode[1] = a;
        self.keycode[2] = b;
        let (a, b) = self.crypt(self.keycode[0], self.keycode[1], false);
        self.keycode[0] = a;
        self.keycode[1] = b;

        for w in 0..=0x11usize {
            // Byte offset (w*4) mod `modulo` selects a keycode word; both are
            // multiples of 4 here, so this stays word-aligned.
            self.keybuf[w] ^= self.keycode[(w * 4) % modulo / 4].swap_bytes();
        }

        let (mut s0, mut s1) = (0u32, 0u32);
        let mut w = 0usize;
        while w < KEYBUF_WORDS {
            let (rx, ry) = self.crypt(s0, s1, false);
            s0 = rx;
            s1 = ry;
            // The upper word is written first (GBATEK), so the pair is stored swapped.
            self.keybuf[w] = s1;
            self.keybuf[w + 1] = s0;
            w += 2;
        }
    }

    /// Decrypt an 8-byte block in place.
    pub fn decrypt_block(&self, block: &mut [u8]) {
        self.crypt_block(block, true);
    }

    /// Encrypt an 8-byte block in place (mirror of [`Key1::decrypt_block`]).
    pub fn encrypt_block(&self, block: &mut [u8]) {
        self.crypt_block(block, false);
    }

    fn crypt_block(&self, block: &mut [u8], decrypt: bool) {
        let y = u32::from_le_bytes(block[0..4].try_into().unwrap());
        let x = u32::from_le_bytes(block[4..8].try_into().unwrap());
        let (w0, w1) = self.crypt(y, x, decrypt);
        block[0..4].copy_from_slice(&w0.to_le_bytes());
        block[4..8].copy_from_slice(&w1.to_le_bytes());
    }
}

/// Whether a cartridge's ARM9 source address places its boot code in the secure
/// area (`4000h..7FFFh`); only then does the secure area need loading/decrypting.
pub fn secure_area_present(arm9_rom_offset: u32) -> bool {
    (0x4000..0x8000).contains(&arm9_rom_offset)
}

/// Cheap pre-check on the 8-byte secure-area ID: a ROM whose ID is already the
/// destroyed value is ready to boot as-is and needs no key table (and no copy).
pub fn secure_area_needs_work(id: &[u8]) -> bool {
    id != DESTROYED
}

/// What [`process_secure_area`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecureAreaState {
    /// Already in the post-boot state (`E7FFDEFF` ID); left untouched.
    Ready,
    /// Shipped decrypted (`"encryObj"` ID); the ID was destroyed as the BIOS does.
    WasDecrypted,
    /// KEY1-decrypted here; the ID verified and was then destroyed.
    Decrypted,
    /// Decryption ran but the ID did not verify (e.g. a devkit zero-filled secure
    /// area); the whole 2 KB was destroyed, matching the BIOS.
    IdMismatch,
    /// The secure area is encrypted but no key table is available (BIOS not loaded).
    NoKeytable,
}

/// Bring the secure area of `rom` into its boot-ready (decrypted, ID-destroyed)
/// form, in place, following GBATEK's `gamecart_decryption`.
///
/// Handles the three dump conventions: already-destroyed (no-op), shipped-decrypted
/// (destroy the ID), and encrypted (decrypt then verify/destroy). `keytable` is the
/// 0x1048-byte KEY1 table from the ARM7 BIOS; `gamecode` is header `[0Ch]`.
pub fn process_secure_area(rom: &mut [u8], gamecode: u32, keytable: &[u8]) -> SecureAreaState {
    let sa = SECURE_AREA_START;
    debug_assert!(rom.len() >= sa + SECURE_AREA_ENC_LEN);
    let id = &rom[sa..sa + 8];
    if id == DESTROYED {
        return SecureAreaState::Ready;
    }
    if id == ENCRYOBJ {
        rom[sa..sa + 8].copy_from_slice(&DESTROYED);
        return SecureAreaState::WasDecrypted;
    }
    if keytable.len() < KEYTABLE_LEN || keytable[..KEYTABLE_LEN].iter().all(|&b| b == 0) {
        return SecureAreaState::NoKeytable;
    }

    // The first 8 bytes are doubly encrypted: level 2 over the ID, then level 3
    // over the whole 2 KB. Undo in reverse order.
    let level2 = Key1::new(keytable, gamecode, 2, 8);
    level2.decrypt_block(&mut rom[sa..sa + 8]);
    let level3 = Key1::new(keytable, gamecode, 3, 8);
    for off in (0..SECURE_AREA_ENC_LEN).step_by(8) {
        level3.decrypt_block(&mut rom[sa + off..sa + off + 8]);
    }

    if rom[sa..sa + 8] == ENCRYOBJ {
        rom[sa..sa + 8].copy_from_slice(&DESTROYED);
        SecureAreaState::Decrypted
    } else {
        // The BIOS destroys the whole 2 KB when the ID does not verify.
        for chunk in rom[sa..sa + SECURE_AREA_ENC_LEN].as_chunks_mut::<4>().0 {
            *chunk = 0xE7FF_DEFFu32.to_le_bytes();
        }
        SecureAreaState::IdMismatch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic but structurally valid key table. The Blowfish round-trip is an
    /// identity for *any* key table, so real BIOS values are not needed to prove the
    /// primitives; a deterministic pattern keeps the test self-contained.
    fn synthetic_keytable() -> Vec<u8> {
        (0..KEYTABLE_LEN as u32)
            .flat_map(|i| (i.wrapping_mul(0x9E37_79B1) ^ 0x5F20_D599).to_le_bytes())
            .take(KEYTABLE_LEN)
            .collect()
    }

    #[test]
    fn encrypt_decrypt_round_trips() {
        let kt = synthetic_keytable();
        let key = Key1::new(&kt, 0x2323_2323, 2, 8);
        let original = [0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        let mut block = original;
        key.encrypt_block(&mut block);
        assert_ne!(block, original, "encryption must change the block");
        key.decrypt_block(&mut block);
        assert_eq!(block, original, "decrypt(encrypt(x)) must be identity");
    }

    #[test]
    fn levels_produce_distinct_key_schedules() {
        let kt = synthetic_keytable();
        let mut b2 = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut b3 = b2;
        Key1::new(&kt, 0x4550_4B49, 2, 8).encrypt_block(&mut b2);
        Key1::new(&kt, 0x4550_4B49, 3, 8).encrypt_block(&mut b3);
        assert_ne!(b2, b3, "level 2 and level 3 must key differently");
    }

    /// Build an encrypted secure area from a plaintext one (the inverse of
    /// `process_secure_area`) so the decrypt path can be validated end to end.
    fn encrypt_secure_area(rom: &mut [u8], gamecode: u32, keytable: &[u8]) {
        let sa = SECURE_AREA_START;
        let level3 = Key1::new(keytable, gamecode, 3, 8);
        for off in (0..SECURE_AREA_ENC_LEN).step_by(8) {
            level3.encrypt_block(&mut rom[sa + off..sa + off + 8]);
        }
        let level2 = Key1::new(keytable, gamecode, 2, 8);
        level2.encrypt_block(&mut rom[sa..sa + 8]);
    }

    #[test]
    fn decrypts_encrypted_secure_area_and_destroys_id() {
        let kt = synthetic_keytable();
        let gamecode = 0x4550_4B49; // "EKPI"
        let mut rom = vec![0u8; SECURE_AREA_START + SECURE_AREA_ENC_LEN];
        // Plaintext secure-area layout: "encryObj" ID, then a marker and payload.
        rom[SECURE_AREA_START..SECURE_AREA_START + 8].copy_from_slice(&ENCRYOBJ);
        for (i, b) in rom[SECURE_AREA_START + 8..SECURE_AREA_START + SECURE_AREA_ENC_LEN]
            .iter_mut()
            .enumerate()
        {
            *b = (i as u8).wrapping_mul(7).wrapping_add(3);
        }
        let plaintext_payload =
            rom[SECURE_AREA_START + 8..SECURE_AREA_START + SECURE_AREA_ENC_LEN].to_vec();

        encrypt_secure_area(&mut rom, gamecode, &kt);
        assert_ne!(&rom[SECURE_AREA_START..SECURE_AREA_START + 8], &ENCRYOBJ);

        let state = process_secure_area(&mut rom, gamecode, &kt);
        assert_eq!(state, SecureAreaState::Decrypted);
        // ID destroyed, payload recovered.
        assert_eq!(&rom[SECURE_AREA_START..SECURE_AREA_START + 8], &DESTROYED);
        assert_eq!(
            &rom[SECURE_AREA_START + 8..SECURE_AREA_START + SECURE_AREA_ENC_LEN],
            &plaintext_payload[..]
        );
    }

    #[test]
    fn already_decrypted_dump_only_destroys_id() {
        let mut rom = vec![0u8; SECURE_AREA_START + SECURE_AREA_ENC_LEN];
        rom[SECURE_AREA_START..SECURE_AREA_START + 8].copy_from_slice(&ENCRYOBJ);
        rom[SECURE_AREA_START + 100] = 0xAB;
        let state = process_secure_area(&mut rom, 0x2323_2323, &[]);
        assert_eq!(state, SecureAreaState::WasDecrypted);
        assert_eq!(&rom[SECURE_AREA_START..SECURE_AREA_START + 8], &DESTROYED);
        assert_eq!(rom[SECURE_AREA_START + 100], 0xAB, "payload untouched");
    }

    #[test]
    fn already_destroyed_dump_is_ready_without_keytable() {
        let mut rom = vec![0u8; SECURE_AREA_START + SECURE_AREA_ENC_LEN];
        rom[SECURE_AREA_START..SECURE_AREA_START + 8].copy_from_slice(&DESTROYED);
        assert!(!secure_area_needs_work(
            &rom[SECURE_AREA_START..SECURE_AREA_START + 8]
        ));
        let state = process_secure_area(&mut rom, 0x2323_2323, &[]);
        assert_eq!(state, SecureAreaState::Ready);
    }

    #[test]
    fn encrypted_without_keytable_reports_missing() {
        let mut rom = vec![0u8; SECURE_AREA_START + SECURE_AREA_ENC_LEN];
        // Ciphertext-looking ID (neither marker).
        rom[SECURE_AREA_START..SECURE_AREA_START + 8]
            .copy_from_slice(&[0x68, 0xF9, 0x08, 0x23, 0x1A, 0x42, 0x04, 0xD0]);
        let state = process_secure_area(&mut rom, 0x2323_2323, &[]);
        assert_eq!(state, SecureAreaState::NoKeytable);
    }

    #[test]
    fn secure_area_present_matches_gbatek_range() {
        assert!(!secure_area_present(0x0200)); // armwrestler: no secure area
        assert!(secure_area_present(0x4000)); // commercial: at the start
        assert!(secure_area_present(0x7FFF));
        assert!(!secure_area_present(0x8000)); // src >= 8000h: no secure area
    }
}
