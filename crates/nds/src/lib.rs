//! Nintendo DS hardware semantics.
//!
//! This crate owns the DS-specific machine: the ARM9 (ARMv5TE) and ARM7 (ARMv4T)
//! cores, their shared scheduler, the DS memory maps, and the specialised
//! coprocessor and 2D/3D hardware. It builds on the device-agnostic `arm`
//! interpreter, turning on the ARMv5TE delta for the ARM9.
//!
//! The bring-up starts with the CP15 system-control coprocessor ([`cp15`]),
//! which the ARM9 drives through `MCR`/`MRC` to place the tightly coupled
//! memories in the memory map.

pub mod cp15;

pub use cp15::Cp15;
