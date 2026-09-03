//! A structured debug interface over a running emulator.
//!
//! This is the richer, semantic layer an external debugger or agent drives — the
//! Rust side of what a debug socket exposes to a client. It borrows an
//! [`emulator::Emulator`] (console-agnostic) and presents namespaced accessors
//! (`video`, `cpu`, `memory`, `scheduler`, `interrupts`, `input`) that dispatch to
//! whichever machine is loaded:
//!
//! ```ignore
//! let mut dbg = Debugger::new(&mut emulator);
//! let pixel = dbg.video().explain_pixel(0, 131, 72)?; // engine 0 = GBA / DS main
//! let regs = dbg.cpu().registers(0);                   // core 0 = GBA / ARM9
//! ```
//!
//! The DS has two 2D engines (0 = A/main, 1 = B/sub) and two CPUs (0 = ARM9,
//! 1 = ARM7); those selectors are ignored on the GBA (one engine, one CPU). Both
//! consoles share `video2d`'s explanation types, so the video provenance API is
//! identical across them.
//!
//! Everything here reads (or re-derives) live state without perturbing the guest
//! timeline. Capabilities that require infrastructure not yet built — a memory
//! last-writer log, execution tracing, rewind — are called out in their namespaces.

use arm::cpu::Mode;
use emu_core::Timestamp;
use emulator::Emulator;
use gba::Access;
use video2d::debug::{ExplainError, PixelExplanation, ScanlineExplanation};

pub mod server;

/// Re-exported so debug clients can name GBA buttons without depending on `gba`.
pub use gba::Key as Button;

/// Map a core index to a DS core (0 = ARM9, 1 = ARM7); the GBA ignores it.
pub(crate) fn nds_core(core: usize) -> nds::Core {
    if core == 1 {
        nds::Core::Arm7
    } else {
        nds::Core::Arm9
    }
}

/// A borrowed, structured debug view of an emulator (GBA or DS).
pub struct Debugger<'a> {
    emu: &'a mut Emulator,
}

impl<'a> Debugger<'a> {
    pub fn new(emu: &'a mut Emulator) -> Self {
        Debugger { emu }
    }

    /// Escape hatch to the underlying emulator facade.
    pub fn emulator(&mut self) -> &mut Emulator {
        self.emu
    }

    /// Whether the loaded machine is a DS (two engines / two CPUs).
    pub fn is_nds(&self) -> bool {
        self.emu.as_nds().is_some()
    }

    pub fn video(&mut self) -> Video<'_> {
        Video { emu: self.emu }
    }
    pub fn cpu(&mut self) -> Cpu<'_> {
        Cpu { emu: self.emu }
    }
    pub fn memory(&mut self) -> Memory<'_> {
        Memory { emu: self.emu }
    }
    pub fn scheduler(&mut self) -> Scheduler<'_> {
        Scheduler { emu: self.emu }
    }
    pub fn interrupts(&mut self) -> Interrupts<'_> {
        Interrupts { emu: self.emu }
    }
    pub fn input(&mut self) -> Input<'_> {
        Input { emu: self.emu }
    }
}

/// `emu.video.*` — graphics inspection and pixel provenance. Methods take an `engine`
/// (0 = GBA / DS Engine A, 1 = DS Engine B); the GBA ignores anything but 0.
pub struct Video<'a> {
    emu: &'a mut Emulator,
}

impl Video<'_> {
    /// The active BG mode (`DISPCNT` bits 0-2) of the engine.
    pub fn current_mode(&mut self, engine: usize) -> u8 {
        if let Some(g) = self.emu.as_gba() {
            g.gba.bus.io.video.current_mode()
        } else if let Some(n) = self.emu.as_nds() {
            n.video_mode(engine)
        } else {
            0
        }
    }

    /// Explain how the pixel at `(x, y)` on `engine` came to be its color — the winning
    /// layer, every rejected candidate with its reason, and each one's tile/map/palette
    /// source.
    pub fn explain_pixel(&mut self, engine: usize, x: u16, y: u16) -> Result<PixelExplanation, ExplainError> {
        if let Some(g) = self.emu.as_gba_mut() {
            g.explain_pixel(x, y)
        } else if let Some(n) = self.emu.as_nds_mut() {
            n.explain_pixel(engine, x, y)
        } else {
            Err(ExplainError::PixelOutsideFramebuffer { x, y })
        }
    }

    /// A whole-scanline summary for `engine` (latched state, evaluated sprites, final line).
    pub fn scanline(&mut self, engine: usize, y: u16) -> ScanlineExplanation {
        if let Some(g) = self.emu.as_gba_mut() {
            g.inspect_scanline(y)
        } else if let Some(n) = self.emu.as_nds_mut() {
            n.inspect_scanline(engine, y)
        } else {
            unreachable!("an emulator is always GBA or NDS")
        }
    }
}

/// `emu.cpu.*` — processor state. `core` selects the DS CPU (0 = ARM9, 1 = ARM7);
/// the GBA ignores it.
pub struct Cpu<'a> {
    emu: &'a mut Emulator,
}

impl Cpu<'_> {
    fn with<R>(&self, f: impl FnOnce(&arm::cpu::Cpu) -> R, core: usize) -> R {
        if let Some(g) = self.emu.as_gba() {
            f(&g.cpu)
        } else if let Some(n) = self.emu.as_nds() {
            f(if core == 1 { &n.arm7 } else { &n.arm9 })
        } else {
            unreachable!()
        }
    }

    /// General-purpose register `index` (0-15).
    pub fn register(&self, core: usize, index: usize) -> u32 {
        self.with(|c| c.register(index), core)
    }

    /// The program counter (R15).
    pub fn pc(&self, core: usize) -> u32 {
        self.with(|c| c.register(15), core)
    }

    /// The current processor mode.
    pub fn mode(&self, core: usize) -> Option<Mode> {
        self.with(|c| c.mode(), core)
    }

    /// All sixteen registers plus the CPSR (as a raw word).
    pub fn registers(&self, core: usize) -> ([u32; 16], u32) {
        self.with(|c| (std::array::from_fn(|i| c.register(i)), c.cpsr().bits()), core)
    }
}

/// `emu.memory.*` — bus reads (writer tracking and watches are future work). `core`
/// selects the DS bus (0 = ARM9, 1 = ARM7); the GBA ignores it.
pub struct Memory<'a> {
    emu: &'a mut Emulator,
}

impl Memory<'_> {
    /// Read a byte via the bus (side-effect-free — does not advance the timeline).
    pub fn read8(&mut self, core: usize, address: u32) -> u8 {
        self.read(core, address, 1) as u8
    }
    pub fn read16(&mut self, core: usize, address: u32) -> u16 {
        self.read(core, address, 2) as u16
    }
    pub fn read32(&mut self, core: usize, address: u32) -> u32 {
        self.read(core, address, 4)
    }

    fn read(&mut self, core: usize, address: u32, bytes: u32) -> u32 {
        if let Some(g) = self.emu.as_gba_mut() {
            match bytes {
                1 => g.gba.bus.read8(address, Access::cpu_data(), &mut g.scheduler).value as u32,
                2 => g.gba.bus.read16(address, Access::cpu_data(), &mut g.scheduler).value as u32,
                _ => g.gba.bus.read32(address, Access::cpu_data(), &mut g.scheduler).value,
            }
        } else if let Some(n) = self.emu.as_nds_mut() {
            n.read(nds_core(core), address, bytes)
        } else {
            0
        }
    }
}

/// `emu.scheduler.*` — the event timeline.
pub struct Scheduler<'a> {
    emu: &'a mut Emulator,
}

impl Scheduler<'_> {
    /// The current guest timestamp.
    pub fn now(&self) -> Timestamp {
        if let Some(g) = self.emu.as_gba() {
            g.scheduler.now()
        } else if let Some(n) = self.emu.as_nds() {
            n.now()
        } else {
            Timestamp::default()
        }
    }

    /// The earliest pending event's time, if any.
    pub fn next_deadline(&self) -> Option<Timestamp> {
        if let Some(g) = self.emu.as_gba() {
            g.scheduler.next_deadline()
        } else if let Some(n) = self.emu.as_nds() {
            n.next_deadline()
        } else {
            None
        }
    }

    /// The number of pending events, stale ones included.
    pub fn pending_count(&self) -> usize {
        if let Some(g) = self.emu.as_gba() {
            g.scheduler.pending_count()
        } else if let Some(n) = self.emu.as_nds() {
            n.pending_count()
        } else {
            0
        }
    }

    /// A chronologically sorted snapshot of the pending events as `(time, label)` pairs
    /// — the event kind rendered with `Debug`, so the console-specific event enums serve
    /// a uniform protocol.
    pub fn pending_events(&self) -> Vec<(u64, String)> {
        if let Some(g) = self.emu.as_gba() {
            g.scheduler
                .pending_events()
                .iter()
                .map(|e| (e.at, format!("{:?}", e.kind)))
                .collect()
        } else if let Some(n) = self.emu.as_nds() {
            n.pending_events()
                .iter()
                .map(|e| (e.at, format!("{:?}", e.kind)))
                .collect()
        } else {
            Vec::new()
        }
    }
}

/// `emu.interrupts.*` — interrupt controller state. `core` selects the DS controller
/// (0 = ARM9, 1 = ARM7); the GBA ignores it.
pub struct Interrupts<'a> {
    emu: &'a mut Emulator,
}

impl Interrupts<'_> {
    /// `IE` — the enabled interrupt sources.
    pub fn enabled(&self, core: usize) -> u32 {
        if let Some(g) = self.emu.as_gba() {
            g.gba.bus.io.irq.ie() as u32
        } else if let Some(n) = self.emu.as_nds() {
            n.interrupts(nds_core(core)).ie()
        } else {
            0
        }
    }

    /// `IF` — the pending (requested) interrupt flags.
    pub fn flags(&self, core: usize) -> u32 {
        if let Some(g) = self.emu.as_gba() {
            g.gba.bus.io.irq.iflags() as u32
        } else if let Some(n) = self.emu.as_nds() {
            n.interrupts(nds_core(core)).iflags()
        } else {
            0
        }
    }

    /// `IME` — the master interrupt enable.
    pub fn master_enable(&self, core: usize) -> bool {
        if let Some(g) = self.emu.as_gba() {
            g.gba.bus.io.irq.ime()
        } else if let Some(n) = self.emu.as_nds() {
            n.interrupts(nds_core(core)).ime()
        } else {
            false
        }
    }

    /// Whether any enabled interrupt is pending (`IE & IF`).
    pub fn pending(&self, core: usize) -> bool {
        if let Some(g) = self.emu.as_gba() {
            g.gba.bus.io.irq.pending()
        } else if let Some(n) = self.emu.as_nds() {
            n.interrupts(nds_core(core)).pending()
        } else {
            false
        }
    }

    /// Whether the CPU's interrupt line is asserted (pending and `IME`).
    pub fn line_asserted(&self, core: usize) -> bool {
        if let Some(g) = self.emu.as_gba() {
            g.gba.bus.io.irq.line_asserted()
        } else if let Some(n) = self.emu.as_nds() {
            n.interrupts(nds_core(core)).line_asserted()
        } else {
            false
        }
    }
}

/// `emu.input.*` — keypad input injection (by button name).
pub struct Input<'a> {
    emu: &'a mut Emulator,
}

/// The DS keypad bit for a button name (set_keypad order), or `None` if unknown.
fn nds_button_bit(name: &str) -> Option<u32> {
    Some(match name.to_ascii_lowercase().as_str() {
        "a" => 0,
        "b" => 1,
        "select" => 2,
        "start" => 3,
        "right" => 4,
        "left" => 5,
        "up" => 6,
        "down" => 7,
        "r" => 8,
        "l" => 9,
        "x" => 10,
        "y" => 11,
        _ => return None,
    })
}

impl Input<'_> {
    /// Set a button (named `"A"`, `"Right"`, `"X"`, …) pressed or released; returns
    /// `false` for an unknown name.
    pub fn set_name(&mut self, name: &str, pressed: bool) -> bool {
        if let Some(g) = self.emu.as_gba_mut() {
            match gba::Key::from_name(name) {
                Some(key) => {
                    g.set_key(key, pressed);
                    true
                }
                None => false,
            }
        } else if let Some(n) = self.emu.as_nds_mut() {
            match nds_button_bit(name) {
                Some(bit) => {
                    let held = n.keypad_pressed();
                    n.set_keypad(if pressed { held | (1 << bit) } else { held & !(1 << bit) });
                    true
                }
                None => false,
            }
        } else {
            false
        }
    }

    /// Press a named button (returns `false` for an unknown name).
    pub fn press_name(&mut self, name: &str) -> bool {
        self.set_name(name, true)
    }

    /// Release a named button (returns `false` for an unknown name).
    pub fn release_name(&mut self, name: &str) -> bool {
        self.set_name(name, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emulator::{Console, Emulator};

    /// Build a bare GBA emulator around a freshly-constructed system for API tests.
    fn gba_emulator() -> Emulator {
        Emulator::from_gba_system(gba::System::new())
    }

    #[test]
    fn video_namespace_reports_mode_and_pixel_source() {
        let mut emu = gba_emulator();
        if let Some(system) = emu.as_gba_mut() {
            // Mode 3, BG2; set pixel (0,0) green in VRAM.
            system
                .gba
                .bus
                .write16(0x0400_0000, 0x0003 | (1 << 10), Access::cpu_data(), &mut system.scheduler);
            system.gba.bus.memory.vram[0..2].copy_from_slice(&0x03E0u16.to_le_bytes());
        }

        let mut dbg = Debugger::new(&mut emu);
        assert_eq!(dbg.video().current_mode(0), 3);
        let explanation = dbg.video().explain_pixel(0, 0, 0).unwrap();
        assert_eq!(explanation.final_color, gba::Color15(0x03E0));
        assert_eq!(dbg.emulator().console(), Console::Gba);
    }

    #[test]
    fn cpu_and_memory_namespaces_read_state() {
        let mut emu = gba_emulator();
        if let Some(system) = emu.as_gba_mut() {
            system.gba.bus.memory.vram[0x10..0x12].copy_from_slice(&0xBEEFu16.to_le_bytes());
        }
        let mut dbg = Debugger::new(&mut emu);
        assert_eq!(dbg.memory().read16(0, 0x0600_0010), 0xBEEF);
        let (r, _cpsr) = dbg.cpu().registers(0);
        assert_eq!(r.len(), 16);
    }

    #[test]
    fn input_namespace_presses_by_name() {
        let mut emu = gba_emulator();
        let mut dbg = Debugger::new(&mut emu);
        assert!(dbg.input().press_name("start"));
        assert!(!dbg.input().press_name("nonsense"));
    }
}
