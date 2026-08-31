//! The ARM946E-S CP15 system-control coprocessor (NDS9).
//!
//! CP15 is what makes the ARM9 more than a faster ARM7: it owns the tightly
//! coupled memories (ITCM/DTCM), the cache, and the protection unit, all driven
//! through `MCR`/`MRC` with coprocessor number 15 and `opcode1` always 0. The
//! `arm` crate decodes those transfers and forwards the five selector fields
//! here through its `Bus` hook; this type is the device half of that bridge.
//!
//! This is the *minimal* model the DS bring-up needs (see the DS roadmap):
//!
//! - **Real:** the TCM base/size registers and the control register, because
//!   they shape the ARM9 memory map — the firmware relocates code and stack into
//!   ITCM/DTCM and reads the ID registers to identify the core.
//! - **Stored, not enforced:** the protection-unit regions and their
//!   cachability/permission bits. They are held so reads round-trip, but no
//!   access is actually checked against them yet.
//! - **No-op:** the cache maintenance commands (`C7`) and cache lockdown. There
//!   is no cache to flush in the interpreter, so the writes are simply accepted.
//!
//! All register layouts and reset/ID values are taken verbatim from GBATEK's
//! "ARM CP15 System Control Coprocessor" section for the NDS9.

/// The ARM946E-S system-control coprocessor.
#[derive(Clone, Debug)]
pub struct Cp15 {
    /// C1,C0,0 — control register (only the R/W bits are stored; see
    /// [`CONTROL_WRITABLE`]).
    control: u32,
    /// C9,C1,0 — DTCM base/size (base in bits 12..31, size N in bits 1..5).
    dtcm: u32,
    /// C9,C1,1 — ITCM size (base is fixed at 0 on the NDS, so only the size
    /// field is retained).
    itcm: u32,

    // Protection unit — stored but not enforced.
    pu_data_cachable: u32,   // C2,C0,0
    pu_inst_cachable: u32,   // C2,C0,1
    pu_write_buffer: u32,    // C3,C0,0
    pu_data_access: u32,     // C5,C0,0
    pu_inst_access: u32,     // C5,C0,1
    pu_data_access_ext: u32, // C5,C0,2
    pu_inst_access_ext: u32, // C5,C0,3
    /// C6,C0..C7 — the eight protection regions (unified on the NDS: the data
    /// and instruction region registers are mirrors of each other).
    pu_region: [u32; 8],
    dcache_lockdown: u32, // C9,C0,0
    icache_lockdown: u32, // C9,C0,1
}

/// C0,C0,0 Main ID — ARMv5TE, ARM946, revision 1 (GBATEK: NDS9 = 41059461h).
const MAIN_ID: u32 = 0x4105_9461;
/// C0,C0,1 Cache Type — 8KB code / 4KB data cache, 32-byte lines (0F0D2112h).
const CACHE_TYPE: u32 = 0x0F0D_2112;
/// C0,C0,2 TCM Physical Size — ITCM 32KB, DTCM 16KB (00140180h).
const TCM_PHYSICAL_SIZE: u32 = 0x0014_0180;

/// Control-register bits that are read/write on the NDS ARM9: bit 0 (PU enable),
/// bit 2 (data cache), bit 7 (endian), and bits 12..19 (icache, vectors, cache
/// replacement, pre-ARMv5 mode, and the DTCM/ITCM enable + load-mode pairs).
const CONTROL_WRITABLE: u32 = (1 << 0) | (1 << 2) | (1 << 7) | (0xFF << 12);
/// Control-register bits that always read as one on the NDS ARM9 (bits 3..6:
/// write buffer, 32-bit exception handling, 32-bit address faults, late abort).
const CONTROL_ALWAYS_SET: u32 = 0b111_1000;

impl Default for Cp15 {
    fn default() -> Self {
        Self::new()
    }
}

impl Cp15 {
    /// A CP15 in its reset state: TCM disabled, PU disabled, control register at
    /// its fixed-bits-only value. The NDS firmware programs the TCM and control
    /// registers during boot.
    pub fn new() -> Self {
        Cp15 {
            control: 0,
            dtcm: 0,
            itcm: 0,
            pu_data_cachable: 0,
            pu_inst_cachable: 0,
            pu_write_buffer: 0,
            pu_data_access: 0,
            pu_inst_access: 0,
            pu_data_access_ext: 0,
            pu_inst_access_ext: 0,
            pu_region: [0; 8],
            dcache_lockdown: 0,
            icache_lockdown: 0,
        }
    }

    /// `MRC p15, 0, Rd, Cn, Cm, opcode2` — read a CP15 register. Returns `None`
    /// for a selector CP15 does not define (the caller then raises the Undefined
    /// Instruction trap), or when `opcode1` is non-zero (all NDS CP15 registers
    /// use `opcode1` 0).
    pub fn read(&self, opcode1: u8, crn: u8, crm: u8, opcode2: u8) -> Option<u32> {
        if opcode1 != 0 {
            return None;
        }
        Some(match (crn, crm, opcode2) {
            // C0 — read-only ID registers. C0,C0,3..7 mirror the Main ID.
            (0, 0, 0) => MAIN_ID,
            (0, 0, 1) => CACHE_TYPE,
            (0, 0, 2) => TCM_PHYSICAL_SIZE,
            (0, 0, _) => MAIN_ID,
            // C1 — control register.
            (1, 0, 0) => self.control_read(),
            // C2/C3/C5 — protection-unit cachability & permission bits.
            (2, 0, 0) => self.pu_data_cachable,
            (2, 0, 1) => self.pu_inst_cachable,
            (3, 0, 0) => self.pu_write_buffer,
            (5, 0, 0) => self.pu_data_access,
            (5, 0, 1) => self.pu_inst_access,
            (5, 0, 2) => self.pu_data_access_ext,
            (5, 0, 3) => self.pu_inst_access_ext,
            // C6 — the eight protection regions (unified: op2 0 or 1 alias).
            (6, region, 0 | 1) if region < 8 => self.pu_region[region as usize],
            // C9 — cache lockdown and TCM base/size.
            (9, 0, 0) => self.dcache_lockdown,
            (9, 0, 1) => self.icache_lockdown,
            (9, 1, 0) => self.dtcm,
            (9, 1, 1) => self.itcm,
            _ => return None,
        })
    }

    /// `MCR p15, 0, Rd, Cn, Cm, opcode2` — write a CP15 register. Returns `false`
    /// for a selector CP15 does not define (→ Undefined Instruction trap) or when
    /// `opcode1` is non-zero. Read-only registers and the cache commands accept
    /// the write and take no state (they return `true`).
    pub fn write(&mut self, opcode1: u8, crn: u8, crm: u8, opcode2: u8, value: u32) -> bool {
        if opcode1 != 0 {
            return false;
        }
        match (crn, crm, opcode2) {
            // C0 — ID registers are read-only; ignore the write but accept it.
            (0, 0, _) => {}
            (1, 0, 0) => self.control = value & CONTROL_WRITABLE,
            (2, 0, 0) => self.pu_data_cachable = value,
            (2, 0, 1) => self.pu_inst_cachable = value,
            (3, 0, 0) => self.pu_write_buffer = value,
            (5, 0, 0) => self.pu_data_access = value,
            (5, 0, 1) => self.pu_inst_access = value,
            (5, 0, 2) => self.pu_data_access_ext = value,
            (5, 0, 3) => self.pu_inst_access_ext = value,
            (6, region, 0 | 1) if region < 8 => self.pu_region[region as usize] = value,
            // C7 — cache maintenance commands; no cache to act on, so no-op.
            (7, _, _) => {}
            (9, 0, 0) => self.dcache_lockdown = value,
            (9, 0, 1) => self.icache_lockdown = value,
            (9, 1, 0) => self.dtcm = value,
            // ITCM base is fixed at 0 on the NDS; keep only the low fields (the
            // size and reserved bits), clearing the base.
            (9, 1, 1) => self.itcm = value & 0x0000_0FFF,
            _ => return false,
        }
        true
    }

    /// The control register as read back: the stored R/W bits with the
    /// always-one bits merged in.
    fn control_read(&self) -> u32 {
        (self.control & CONTROL_WRITABLE) | CONTROL_ALWAYS_SET
    }

    // --- Derived view for the DS memory map (consumed in M2) -----------------

    /// Whether the protection unit is enabled (control bit 0). Stored only — no
    /// access is actually checked against the regions yet.
    pub fn protection_unit_enabled(&self) -> bool {
        self.control & (1 << 0) != 0
    }

    /// Whether exception vectors are relocated high, to `FFFF0000h` (control
    /// bit 13); otherwise they are at `00000000h`.
    pub fn high_exception_vectors(&self) -> bool {
        self.control & (1 << 13) != 0
    }

    /// Whether DTCM is enabled (control bit 16).
    pub fn dtcm_enabled(&self) -> bool {
        self.control & (1 << 16) != 0
    }

    /// Whether DTCM is in load (write-only) mode (control bit 17).
    pub fn dtcm_load_mode(&self) -> bool {
        self.control & (1 << 17) != 0
    }

    /// Whether ITCM is enabled (control bit 18).
    pub fn itcm_enabled(&self) -> bool {
        self.control & (1 << 18) != 0
    }

    /// Whether ITCM is in load (write-only) mode (control bit 19).
    pub fn itcm_load_mode(&self) -> bool {
        self.control & (1 << 19) != 0
    }

    /// DTCM base address (bits 12..31 of C9,C1,0, i.e. `X << 12`).
    pub fn dtcm_base(&self) -> u32 {
        self.dtcm & 0xFFFF_F000
    }

    /// DTCM virtual size in bytes (`512 << N`, from bits 1..5 of C9,C1,0).
    pub fn dtcm_size(&self) -> u32 {
        tcm_virtual_size(self.dtcm)
    }

    /// ITCM base address — always 0 on the NDS.
    pub fn itcm_base(&self) -> u32 {
        0
    }

    /// ITCM virtual size in bytes (`512 << N`, from bits 1..5 of C9,C1,1). ITCM
    /// mirrors fill the region above its physical 32KB.
    pub fn itcm_size(&self) -> u32 {
        tcm_virtual_size(self.itcm)
    }
}

/// Decode a TCM base/size register's virtual size: `512 << N`, where `N` is the
/// 5-bit size field in bits 1..5. Saturates rather than overflowing at the top
/// of the field (`N` up to 23 is the architectural max of 4GB).
fn tcm_virtual_size(reg: u32) -> u32 {
    let n = (reg >> 1) & 0x1F;
    512u32.checked_shl(n).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_registers_report_the_nds9_core() {
        let cp = Cp15::new();
        assert_eq!(cp.read(0, 0, 0, 0), Some(0x4105_9461)); // ARMv5TE ARM946 rev1
        assert_eq!(cp.read(0, 0, 0, 1), Some(0x0F0D_2112)); // cache type
        assert_eq!(cp.read(0, 0, 0, 2), Some(0x0014_0180)); // TCM physical size
        // C0,C0,3..7 mirror the Main ID.
        assert_eq!(cp.read(0, 0, 0, 5), Some(0x4105_9461));
        // ID registers are read-only: a write is accepted but changes nothing.
        let mut cp = cp;
        assert!(cp.write(0, 0, 0, 0, 0xDEAD_BEEF));
        assert_eq!(cp.read(0, 0, 0, 0), Some(0x4105_9461));
    }

    #[test]
    fn control_register_masks_to_writable_and_fixed_bits() {
        let mut cp = Cp15::new();
        // Reset: only the always-one bits (3..6) read back.
        assert_eq!(cp.read(0, 1, 0, 0), Some(0x0000_0078));
        // Write all ones: only the R/W bits stick, and the fixed bits stay set.
        assert!(cp.write(0, 1, 0, 0, 0xFFFF_FFFF));
        assert_eq!(cp.read(0, 1, 0, 0), Some(CONTROL_WRITABLE | CONTROL_ALWAYS_SET));
        // A reserved bit (e.g. bit 20) never sticks.
        assert!(cp.write(0, 1, 0, 0, 1 << 20));
        assert_eq!(cp.read(0, 1, 0, 0), Some(CONTROL_ALWAYS_SET));
    }

    #[test]
    fn control_bits_drive_the_derived_tcm_view() {
        let mut cp = Cp15::new();
        assert!(!cp.dtcm_enabled() && !cp.itcm_enabled());
        // Enable DTCM (bit16) + load mode (bit17), ITCM (bit18), high vectors (13).
        cp.write(0, 1, 0, 0, (1 << 16) | (1 << 17) | (1 << 18) | (1 << 13));
        assert!(cp.dtcm_enabled() && cp.dtcm_load_mode());
        assert!(cp.itcm_enabled() && !cp.itcm_load_mode());
        assert!(cp.high_exception_vectors());
    }

    #[test]
    fn dtcm_base_and_size_decode_like_the_firmware_setting() {
        let mut cp = Cp15::new();
        // The value the NDS firmware programs: base 027C0000h, virtual size 16KB
        // (512 << 5). Size field N=5 sits in bits 1..5, so N<<1 = 0x0A.
        cp.write(0, 9, 1, 0, 0x027C_0000 | 0x0A);
        assert_eq!(cp.dtcm_base(), 0x027C_0000);
        assert_eq!(cp.dtcm_size(), 16 * 1024);
        assert_eq!(cp.read(0, 9, 1, 0), Some(0x027C_000A));
    }

    #[test]
    fn itcm_base_is_fixed_at_zero_but_size_is_writable() {
        let mut cp = Cp15::new();
        // Firmware programs a large virtual size to mirror ITCM upward; the base
        // field must be ignored (fixed 0). N=6 → 512<<6 = 32KB.
        cp.write(0, 9, 1, 1, 0x0100_0000 | 0x0C);
        assert_eq!(cp.itcm_base(), 0);
        assert_eq!(cp.itcm_size(), 32 * 1024);
        // The stored register reads back with the base cleared.
        assert_eq!(cp.read(0, 9, 1, 1), Some(0x0000_000C));
    }

    #[test]
    fn protection_regions_round_trip_but_are_not_enforced() {
        let mut cp = Cp15::new();
        cp.write(0, 6, 3, 0, 0x0400_0029); // region 3
        assert_eq!(cp.read(0, 6, 3, 0), Some(0x0400_0029));
        // Unified: the instruction-side alias (op2=1) mirrors the same region.
        assert_eq!(cp.read(0, 6, 3, 1), Some(0x0400_0029));
        cp.write(0, 6, 3, 1, 0x0200_0025);
        assert_eq!(cp.read(0, 6, 3, 0), Some(0x0200_0025));
    }

    #[test]
    fn cache_commands_are_accepted_as_no_ops() {
        let mut cp = Cp15::new();
        // C7,C5,0 = Invalidate Entire Instruction Cache; C7,C10,4 = Drain Write
        // Buffer. Both must be accepted (there is no cache to touch).
        assert!(cp.write(0, 7, 5, 0, 0));
        assert!(cp.write(0, 7, 10, 4, 0));
        // But C7 is write-only: a read is undefined.
        assert_eq!(cp.read(0, 7, 5, 0), None);
    }

    #[test]
    fn unknown_selectors_and_nonzero_opcode1_trap() {
        let mut cp = Cp15::new();
        assert_eq!(cp.read(0, 4, 0, 0), None); // C4 is unassigned
        assert!(!cp.write(0, 4, 0, 0, 0));
        // opcode1 is always 0 for NDS CP15 registers.
        assert_eq!(cp.read(1, 1, 0, 0), None);
        assert!(!cp.write(1, 1, 0, 0, 0));
    }
}
