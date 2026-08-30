//! Battery-backed SRAM (32 KiB) — the simplest GBA save backup.
//!
//! SRAM is a plain 8-bit static RAM: reads and writes hit bytes directly, with no
//! command protocol. The chip is 32 KiB; the 64 KiB save region mirrors it.

/// GBA SRAM is 32 KiB.
pub const SRAM_SIZE: usize = 0x8000;

#[derive(Clone, Debug)]
pub struct Sram {
    data: Box<[u8]>,
    dirty: bool,
}

impl Default for Sram {
    fn default() -> Self {
        // An erased/blank chip reads as all-ones.
        Sram { data: vec![0xFF; SRAM_SIZE].into_boxed_slice(), dirty: false }
    }
}

impl Sram {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    fn index(addr: u32) -> usize {
        addr as usize & (SRAM_SIZE - 1)
    }

    pub fn read8(&self, addr: u32) -> u8 {
        self.data[Self::index(addr)]
    }

    pub fn write8(&mut self, addr: u32, value: u8) {
        let i = Self::index(addr);
        if self.data[i] != value {
            self.data[i] = value;
            self.dirty = true;
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// Overwrite the contents from a loaded save (e.g. a `.sav` file), copying up
    /// to the chip size and leaving any remainder blank.
    pub fn load(&mut self, data: &[u8]) {
        let len = data.len().min(SRAM_SIZE);
        self.data[..len].copy_from_slice(&data[..len]);
        for b in self.data[len..].iter_mut() {
            *b = 0xFF;
        }
        self.dirty = false;
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write_roundtrip_and_dirty() {
        let mut s = Sram::new();
        assert_eq!(s.read8(0), 0xFF); // blank
        assert!(!s.dirty());
        s.write8(0x10, 0xAB);
        assert_eq!(s.read8(0x10), 0xAB);
        assert!(s.dirty());
    }

    #[test]
    fn thirty_two_kib_mirrors_across_the_region() {
        let mut s = Sram::new();
        s.write8(0x0000, 0x5A);
        // 0x8000 mirrors 0x0000.
        assert_eq!(s.read8(0x8000), 0x5A);
    }

    #[test]
    fn load_replaces_contents_and_clears_dirty() {
        let mut s = Sram::new();
        s.write8(0, 0x11);
        s.load(&[0x22, 0x33]);
        assert_eq!(s.read8(0), 0x22);
        assert_eq!(s.read8(1), 0x33);
        assert_eq!(s.read8(2), 0xFF);
        assert!(!s.dirty());
    }
}
