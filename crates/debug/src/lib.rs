//! A structured debug interface over a running emulator.
//!
//! This is the richer, semantic layer an external debugger or agent drives — the
//! Rust side of what a future debug socket exposes to a client. It borrows a
//! [`System`] and presents namespaced accessors (`video`, `cpu`, `memory`,
//! `scheduler`, `interrupts`) that mirror the eventual client API shape:
//!
//! ```ignore
//! let mut dbg = Debugger::new(&mut system);
//! let pixel = dbg.video().explain_pixel(131, 72)?;
//! let sprite = dbg.video().sprite_at(131, 72);
//! let events = dbg.scheduler().pending_events();
//! ```
//!
//! Everything here reads (or re-derives) live state without perturbing the guest
//! timeline. Capabilities that require infrastructure not yet built — a memory
//! last-writer log, execution tracing, rewind, input injection — are called out
//! in their namespaces and will attach here as that infrastructure lands.

use arm::cpu::Mode;
use emu_core::{ScheduledEvent, Timestamp};
use gba::ppu::debug::explain::WindowExplanation;
use gba::ppu::debug::provenance::{BackgroundId, ObjProvenance};
use gba::ppu::state::CandidatePixel;
use gba::{
    Access, BackgroundSummary, Color15, EventKind, ExplainError, Key, PixelExplanation,
    ScanlineExplanation, SpriteInstance, System,
};

pub mod server;

// Re-exported so debug clients can name buttons without depending on `gba`.
pub use gba::Key as Button;

/// A borrowed, structured debug view of a system.
pub struct Debugger<'a> {
    system: &'a mut System,
}

impl<'a> Debugger<'a> {
    pub fn new(system: &'a mut System) -> Self {
        Debugger { system }
    }

    /// Escape hatch to the underlying system.
    pub fn system(&mut self) -> &mut System {
        self.system
    }

    pub fn video(&mut self) -> Video<'_> {
        Video {
            system: &mut *self.system,
        }
    }

    pub fn cpu(&mut self) -> Cpu<'_> {
        Cpu {
            system: &mut *self.system,
        }
    }

    pub fn memory(&mut self) -> Memory<'_> {
        Memory {
            system: &mut *self.system,
        }
    }

    pub fn scheduler(&mut self) -> Scheduler<'_> {
        Scheduler {
            system: &mut *self.system,
        }
    }

    pub fn interrupts(&mut self) -> Interrupts<'_> {
        Interrupts {
            system: &mut *self.system,
        }
    }

    pub fn input(&mut self) -> Input<'_> {
        Input {
            system: &mut *self.system,
        }
    }
}

/// `emu.video.*` — graphics inspection and pixel provenance.
pub struct Video<'a> {
    system: &'a mut System,
}

impl Video<'_> {
    /// The active video mode (0-5).
    pub fn current_mode(&self) -> u8 {
        self.system.gba.bus.io.video.current_mode()
    }

    /// The four backgrounds' current configuration.
    pub fn backgrounds(&self) -> [BackgroundSummary; 4] {
        self.system.gba.bus.io.video.backgrounds()
    }

    /// The backgrounds actively producing pixels in the current mode.
    pub fn active_backgrounds(&self) -> Vec<BackgroundId> {
        self.system.gba.bus.io.video.active_backgrounds()
    }

    /// The current output image, in canonical BGR555.
    pub fn framebuffer(&self) -> &[Color15] {
        self.system.framebuffer()
    }

    /// Explain how the pixel at `(x, y)` came to be its color.
    pub fn explain_pixel(&mut self, x: u16, y: u16) -> Result<PixelExplanation, ExplainError> {
        self.system.explain_pixel(x, y)
    }

    /// A whole-scanline summary (latched state, evaluated sprites, final line).
    pub fn scanline(&mut self, y: u16) -> ScanlineExplanation {
        self.system.inspect_scanline(y)
    }

    /// The sprite owning pixel `(x, y)`, if any.
    pub fn sprite_at(&mut self, x: u16, y: u16) -> Option<ObjProvenance> {
        self.system
            .gba
            .bus
            .with_video_view(|video, mem| video.sprite_at(x, y, mem))
    }

    /// The sprites evaluated as visible on scanline `y`.
    pub fn sprites_on_scanline(&mut self, y: u16) -> Vec<SpriteInstance> {
        self.system
            .gba
            .bus
            .with_video_view(|video, mem| video.sprites_on_scanline(y, mem))
    }

    /// The candidate a background contributes at `(x, y)`.
    pub fn layer_pixel(&mut self, bg: usize, x: u16, y: u16) -> Option<CandidatePixel> {
        self.system
            .gba
            .bus
            .with_video_view(|video, mem| video.layer_pixel(bg, x, y, mem))
    }

    /// The window decision at `(x, y)`.
    pub fn window_at(&mut self, x: u16, y: u16) -> WindowExplanation {
        self.system
            .gba
            .bus
            .with_video_view(|video, mem| video.window_at(x, y, mem))
    }

    /// The guest memory addresses feeding pixel `(x, y)`.
    pub fn source_memory(&mut self, x: u16, y: u16) -> Vec<u32> {
        self.system
            .gba
            .bus
            .with_video_view(|video, mem| video.source_memory(x, y, mem))
    }
}

/// `emu.cpu.*` — processor state.
pub struct Cpu<'a> {
    system: &'a mut System,
}

impl Cpu<'_> {
    /// General-purpose register `index` (0-15).
    pub fn register(&self, index: usize) -> u32 {
        self.system.cpu.register(index)
    }

    /// The program counter (R15).
    pub fn pc(&self) -> u32 {
        self.system.cpu.register(15)
    }

    /// The current processor mode.
    pub fn mode(&self) -> Option<Mode> {
        self.system.cpu.mode()
    }

    // `last_writer(addr)` awaits a memory write-provenance log (see `Memory`).
}

/// `emu.memory.*` — memory reads (writer tracking and watches are future work).
pub struct Memory<'a> {
    system: &'a mut System,
}

impl Memory<'_> {
    /// Read a byte via the bus (side-effect-free — does not advance the timeline).
    pub fn read8(&mut self, address: u32) -> u8 {
        self.system
            .gba
            .bus
            .read8(address, Access::cpu_data(), &mut self.system.scheduler)
            .value
    }

    pub fn read16(&mut self, address: u32) -> u16 {
        self.system
            .gba
            .bus
            .read16(address, Access::cpu_data(), &mut self.system.scheduler)
            .value
    }

    pub fn read32(&mut self, address: u32) -> u32 {
        self.system
            .gba
            .bus
            .read32(address, Access::cpu_data(), &mut self.system.scheduler)
            .value
    }

    /// Direct read-only views of the graphics regions.
    pub fn vram(&self) -> &[u8] {
        &self.system.gba.bus.memory.vram
    }

    pub fn palette(&self) -> &[u8] {
        &self.system.gba.bus.memory.palette
    }

    pub fn oam(&self) -> &[u8] {
        &self.system.gba.bus.memory.oam
    }

    // `last_writer(addr)` / `watch_writes(range)` await a write-provenance log.
}

/// `emu.scheduler.*` — the event timeline.
pub struct Scheduler<'a> {
    system: &'a mut System,
}

impl Scheduler<'_> {
    /// The current guest timestamp.
    pub fn now(&self) -> Timestamp {
        self.system.scheduler.now()
    }

    /// The earliest pending event's time, if any.
    pub fn next_deadline(&self) -> Option<Timestamp> {
        self.system.scheduler.next_deadline()
    }

    /// A chronologically sorted snapshot of the pending events. A returned event
    /// may be stale (superseded by a later reconfiguration); staleness is resolved
    /// only when it would fire.
    pub fn pending_events(&self) -> Vec<ScheduledEvent<EventKind>> {
        self.system.scheduler.pending_events()
    }

    /// The number of pending events, stale ones included.
    pub fn pending_count(&self) -> usize {
        self.system.scheduler.pending_count()
    }
}

/// `emu.interrupts.*` — interrupt controller state (history is future work).
pub struct Interrupts<'a> {
    system: &'a mut System,
}

impl Interrupts<'_> {
    /// `IE` — the enabled interrupt sources.
    pub fn enabled(&self) -> u16 {
        self.system.gba.bus.io.irq.ie()
    }

    /// `IF` — the pending (requested) interrupt flags.
    pub fn flags(&self) -> u16 {
        self.system.gba.bus.io.irq.iflags()
    }

    /// `IME` — the master interrupt enable.
    pub fn master_enable(&self) -> bool {
        self.system.gba.bus.io.irq.ime()
    }

    /// Whether any enabled interrupt is pending (`IE & IF`).
    pub fn pending(&self) -> bool {
        self.system.gba.bus.io.irq.pending()
    }

    /// Whether the CPU's interrupt line is asserted (pending and `IME`).
    pub fn line_asserted(&self) -> bool {
        self.system.gba.bus.io.irq.line_asserted()
    }

    // `history()` awaits an interrupt event log.
}

/// `emu.input.*` — keypad input injection.
pub struct Input<'a> {
    system: &'a mut System,
}

impl Input<'_> {
    /// Press a button.
    pub fn press(&mut self, key: Key) {
        self.system.press_key(key);
    }

    /// Release a button.
    pub fn release(&mut self, key: Key) {
        self.system.release_key(key);
    }

    /// Set a button's pressed state.
    pub fn set(&mut self, key: Key, pressed: bool) {
        self.system.set_key(key, pressed);
    }

    /// Press a button named as a string (e.g. `"A"`, `"Right"`); returns `false`
    /// for an unknown name. The shape a string-keyed protocol drives.
    pub fn press_name(&mut self, name: &str) -> bool {
        match Key::from_name(name) {
            Some(key) => {
                self.system.press_key(key);
                true
            }
            None => false,
        }
    }

    /// Release a button named as a string; returns `false` for an unknown name.
    pub fn release_name(&mut self, name: &str) -> bool {
        match Key::from_name(name) {
            Some(key) => {
                self.system.release_key(key);
                true
            }
            None => false,
        }
    }

    /// Whether a button is currently pressed.
    pub fn is_pressed(&self, key: Key) -> bool {
        self.system.gba.bus.io.keypad.is_pressed(key)
    }

    /// The raw `KEYINPUT` value (active-low).
    pub fn keyinput(&self) -> u16 {
        self.system.gba.bus.io.keypad.read_input()
    }

    // `hold(key, frames)` and DS `touch(x, y)` are driver-level conveniences to
    // come once a frame-stepping run loop and the DS exist.
}

#[cfg(test)]
mod tests {
    use super::*;
    use gba::IrqSource;

    #[test]
    fn video_namespace_reports_mode_and_pixel_source() {
        let mut system = System::new();
        // Mode 3, BG2; set pixel (0,0) green in VRAM.
        system
            .gba
            .bus
            .write16(0x0400_0000, 0x0003 | (1 << 10), Access::cpu_data(), &mut system.scheduler);
        system.gba.bus.memory.vram[0..2].copy_from_slice(&0x03E0u16.to_le_bytes());

        let mut dbg = Debugger::new(&mut system);
        assert_eq!(dbg.video().current_mode(), 3);
        let explanation = dbg.video().explain_pixel(0, 0).unwrap();
        assert_eq!(explanation.final_color, Color15(0x03E0));
        let sources = dbg.video().source_memory(0, 0);
        assert!(sources.contains(&0x0600_0000));
    }

    #[test]
    fn scheduler_and_interrupt_namespaces_read_state() {
        let mut system = System::new();
        system.gba.bus.io.irq.set_ie(IrqSource::Timer0.mask());
        system.gba.bus.io.irq.set_ime(true);
        system.start_lcd(); // schedules PPU events

        let mut dbg = Debugger::new(&mut system);
        assert!(dbg.scheduler().pending_count() > 0);
        assert!(dbg.scheduler().next_deadline().is_some());
        assert_eq!(dbg.interrupts().enabled(), IrqSource::Timer0.mask());
        assert!(dbg.interrupts().master_enable());
    }

    #[test]
    fn input_namespace_presses_and_raises_irq() {
        let mut system = System::new();
        // Enable the keypad interrupt on A (OR condition).
        system.gba.bus.io.irq.set_ie(IrqSource::Keypad.mask());
        system.gba.bus.io.irq.set_ime(true);
        system
            .gba
            .bus
            .write16(0x0400_0132, (1 << 14) | Button::A.bit(), Access::cpu_data(), &mut system.scheduler);

        let mut dbg = Debugger::new(&mut system);
        assert!(!dbg.input().is_pressed(Button::A));
        dbg.input().press(Button::A);
        assert!(dbg.input().is_pressed(Button::A));
        // KEYINPUT is active-low: pressing A clears bit 0.
        assert_eq!(dbg.input().keyinput() & 1, 0);
        // The press raised the keypad interrupt.
        assert!(dbg.interrupts().line_asserted());

        assert!(dbg.input().press_name("start"));
        assert!(!dbg.input().press_name("nonsense"));
    }

    #[test]
    fn memory_namespace_reads_regions() {
        let mut system = System::new();
        system.gba.bus.memory.vram[0x10..0x12].copy_from_slice(&0xBEEFu16.to_le_bytes());
        let mut dbg = Debugger::new(&mut system);
        assert_eq!(dbg.memory().read16(0x0600_0010), 0xBEEF);
        assert_eq!(dbg.memory().vram()[0x10], 0xEF);
    }
}
