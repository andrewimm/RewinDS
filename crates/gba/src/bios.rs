//! The built-in GBA BIOS replacement.
//!
//! RewinDS ships its own freely redistributable BIOS (assembled from
//! `bios/gba/bios.s`) so that no copyrighted Nintendo image is required to boot.
//! It runs on the same CPU path as a real BIOS — reset at `0x0`, SWI at `0x8`,
//! IRQ at `0x18` — and callers may still supply a real image when byte-exact
//! fidelity is wanted.

/// The built-in 16 KiB BIOS image, baked in at build time.
///
/// Rebuild it with `bios/gba/build.sh` after editing the assembly source.
pub fn default_bios() -> &'static [u8] {
    include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../bios/gba/gba_bios.bin"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_is_one_bios_worth() {
        assert_eq!(default_bios().len(), 0x4000);
    }

    #[test]
    fn reset_vector_is_an_arm_branch() {
        // The first word must be an unconditional ARM branch (0xEA......) so the
        // reset vector jumps into the boot code rather than executing data.
        let first = u32::from_le_bytes(default_bios()[0..4].try_into().unwrap());
        assert_eq!(first >> 24, 0xEA, "reset vector should be `b <reset>`");
    }
}
