//! The console-agnostic emulator facade.
//!
//! A [`gba::System`] and a future `nds::System` are disjoint machines with
//! different clocks, memory maps, and screen counts. [`Emulator`] is the thin
//! enum that carries either behind one uniform surface — load, run a frame, feed
//! input, read screens, pull audio, exchange save data — so a frontend never
//! needs to know which console it is holding.
//!
//! # FFI-ready by construction
//!
//! The eventual destination is a C FFI wrapped by native Swift apps, so the
//! **C boundary is the host boundary**: everywhere a frontend would otherwise
//! own policy, the core instead exchanges plain bytes and pointers.
//!
//! - ROMs and save data cross as byte slices ([`Load`], [`Emulator::save_data`]),
//!   never file paths — the host owns the filesystem.
//! - The core owns the presentable pixel buffers ([`Screen`], RGBA8), so the host
//!   blits a pointer straight into a texture.
//! - [`Input`] is a `#[repr(C)]` button bitmask plus touch, consumed by whichever
//!   subset a console models (the GBA ignores the DS-only bits).
//! - Audio is a pull: [`Emulator::enable_audio`] hands back an [`audio::AudioSource`]
//!   the host drains on its own thread, while the run loop feeds the producer.
//!
//! An [`Emulator`] is `Send` but not `Sync`: one owner thread drives it. The
//! `debug` crate remains a GBA-specific inspector for now and reaches the concrete
//! machine through [`Emulator::as_gba_mut`] / [`Emulator::into_gba`]; generalising
//! it is deferred until an NDS machine exists.

pub mod audio;

pub use audio::AudioSource;

/// Which console an image targets.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Console {
    Gba,
    Nds,
}

impl Console {
    /// Guess the console from a ROM image's header. The GBA header carries a fixed
    /// `0x96` at offset `0xB2`; an NDS image is guessed from a plausible ARM9 ROM
    /// offset in its 512-byte header. Returns `None` when neither matches — the
    /// caller can still force a console via [`Load::console`].
    pub fn detect(rom: &[u8]) -> Option<Console> {
        if rom.len() > 0xB2 && rom[0xB2] == 0x96 {
            return Some(Console::Gba);
        }
        if rom.len() >= 0x200 {
            let arm9_off =
                u32::from_le_bytes([rom[0x20], rom[0x21], rom[0x22], rom[0x23]]);
            if (0x200..rom.len() as u32).contains(&arm9_off) {
                return Some(Console::Nds);
            }
        }
        None
    }
}

/// Button bits for [`Input::buttons`]. The low ten match the GBA `KEYINPUT` bit
/// order so the mapping to GBA keys is direct; `X`/`Y` extend it for the DS.
pub mod button {
    pub const A: u32 = 1 << 0;
    pub const B: u32 = 1 << 1;
    pub const SELECT: u32 = 1 << 2;
    pub const START: u32 = 1 << 3;
    pub const RIGHT: u32 = 1 << 4;
    pub const LEFT: u32 = 1 << 5;
    pub const UP: u32 = 1 << 6;
    pub const DOWN: u32 = 1 << 7;
    /// Right shoulder.
    pub const R: u32 = 1 << 8;
    /// Left shoulder.
    pub const L: u32 = 1 << 9;
    /// DS only.
    pub const X: u32 = 1 << 10;
    /// DS only.
    pub const Y: u32 = 1 << 11;
}

/// A full input snapshot for one frame. A console consumes only the subset it
/// models: the GBA reads the low ten buttons and ignores touch and `X`/`Y`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Input {
    /// A bitmask of [`button`] constants (set bit = pressed).
    pub buttons: u32,
    /// DS touch coordinate (screen pixels); ignored on the GBA.
    pub touch_x: i16,
    pub touch_y: i16,
    /// Whether the DS touchscreen is being pressed.
    pub touch_pressed: bool,
}

impl Input {
    /// Set or clear one button by its [`button`] bit.
    pub fn set(&mut self, button: u32, pressed: bool) {
        if pressed {
            self.buttons |= button;
        } else {
            self.buttons &= !button;
        }
    }
}

/// A presentable screen: `width` × `height` pixels of RGBA8 (`rgba.len() ==
/// width * height * 4`), owned by the emulator and valid until the next
/// [`Emulator::run_frame`].
pub struct Screen<'a> {
    pub width: u32,
    pub height: u32,
    pub rgba: &'a [u8],
}

/// Why [`Emulator::load`] could not build a machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// No console was given and none could be detected from the ROM.
    UnknownConsole,
    /// The GBA requires a BIOS image and none was supplied.
    MissingBios,
    /// The console is recognised but not yet implemented.
    Unsupported(Console),
    /// A `.nds` image could not be direct-booted.
    BadImage(nds::BootError),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::UnknownConsole => write!(f, "could not determine the console"),
            LoadError::MissingBios => write!(f, "a BIOS image is required"),
            LoadError::Unsupported(c) => write!(f, "{c:?} is not yet supported"),
            LoadError::BadImage(e) => write!(f, "invalid .nds image: {e:?}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// What to boot. ROMs and BIOS images cross as byte slices; the host reads the
/// files. Save data is restored separately via [`Emulator::load_save_data`], so
/// the host keeps sole ownership of persistence.
#[derive(Default)]
pub struct Load<'a> {
    /// Force a console; `None` detects it from `rom`.
    pub console: Option<Console>,
    /// The cartridge image (a `.gba`, or later a `.nds`). May be absent for a
    /// BIOS-only GBA boot, in which case `console` must be set.
    pub rom: Option<&'a [u8]>,
    /// The BIOS image. Required for the GBA; the ARM9 BIOS for the DS.
    pub bios: Option<&'a [u8]>,
    /// The DS ARM7 BIOS. Optional (BIOS-free homebrew runs without it).
    pub bios7: Option<&'a [u8]>,
}

/// Either console, behind one surface.
// The two machines differ in size, but the facade is held once and long-lived,
// so the variance costs nothing worth an extra indirection.
#[allow(clippy::large_enum_variant)]
pub enum Emulator {
    Gba(GbaEmulator),
    Nds(NdsEmulator),
}

impl Emulator {
    /// Build a machine from the given image. See [`LoadError`] for the failures.
    pub fn load(cfg: Load) -> Result<Emulator, LoadError> {
        let console = cfg
            .console
            .or_else(|| cfg.rom.and_then(Console::detect))
            .ok_or(LoadError::UnknownConsole)?;
        match console {
            Console::Gba => {
                let bios = cfg.bios.ok_or(LoadError::MissingBios)?;
                let mut system = gba::System::new();
                system.gba.bus.load_bios(bios);
                if let Some(rom) = cfg.rom {
                    system.gba.bus.load_rom(rom.to_vec());
                }
                // The CPU begins at the BIOS reset vector.
                system.cpu.set_pc(0);
                Ok(Emulator::Gba(GbaEmulator::new(system)))
            }
            Console::Nds => {
                let mut system = nds::System::new();
                // Load the BIOS images if given (optional for BIOS-free homebrew).
                if let (Some(b9), Some(b7)) = (cfg.bios, cfg.bios7) {
                    system.load_bios(b9, b7);
                }
                // Direct-boot the cartridge if present; a bare DS (no ROM) still
                // runs and presents its two screens.
                if let Some(rom) = cfg.rom {
                    system.direct_boot(rom).map_err(LoadError::BadImage)?;
                }
                Ok(Emulator::Nds(NdsEmulator::new(system)))
            }
        }
    }

    /// Which console this is.
    pub fn console(&self) -> Console {
        match self {
            Emulator::Gba(_) => Console::Gba,
            Emulator::Nds(_) => Console::Nds,
        }
    }

    /// Advance the machine by one video frame, presenting the result to
    /// [`Self::screen`] and (unless muted) feeding produced samples to the audio
    /// sink from [`Self::enable_audio`].
    pub fn run_frame(&mut self) {
        match self {
            Emulator::Gba(g) => g.run_frame(),
            Emulator::Nds(n) => n.run_frame(),
        }
    }

    /// Set the input state applied on subsequent frames.
    pub fn set_input(&mut self, input: Input) {
        match self {
            Emulator::Gba(g) => g.set_input(input),
            // The DS keypad shares the low-ten bit order; touch is not wired yet.
            Emulator::Nds(n) => n.system.set_keypad(input.buttons),
        }
    }

    /// Mute audio capture: while muted, [`Self::run_frame`] still runs but drops
    /// the frame's samples instead of feeding the sink. Used for fast-forward.
    pub fn set_audio_muted(&mut self, muted: bool) {
        match self {
            Emulator::Gba(g) => g.audio_muted = muted,
            Emulator::Nds(_) => {}
        }
    }

    /// Attach a host audio output at `output_rate` Hz with `channels` channels,
    /// returning the consumer the host drains on its audio thread. Replaces any
    /// previous attachment. (The DS audio pipeline is not implemented yet, so an
    /// NDS returns a consumer that only ever underruns to silence.)
    pub fn enable_audio(&mut self, output_rate: u32, channels: usize) -> AudioSource {
        let (sink, source) = audio::channel(output_rate, channels);
        match self {
            Emulator::Gba(g) => g.audio = Some(sink),
            Emulator::Nds(_) => drop(sink),
        }
        source
    }

    /// How many screens this console presents (GBA: 1, NDS: 2).
    pub fn screen_count(&self) -> usize {
        match self {
            Emulator::Gba(_) => 1,
            Emulator::Nds(_) => 2,
        }
    }

    /// The presentable screen at `index`, or `None` if out of range. Screen 0 is
    /// the top screen.
    pub fn screen(&self, index: usize) -> Option<Screen<'_>> {
        match self {
            Emulator::Gba(g) => (index == 0).then(|| Screen {
                width: gba_screen::WIDTH as u32,
                height: gba_screen::HEIGHT as u32,
                rgba: &g.rgba,
            }),
            Emulator::Nds(n) => (index < 2).then(|| Screen {
                width: nds::ppu::WIDTH as u32,
                height: nds::ppu::HEIGHT as u32,
                rgba: &n.rgba[index],
            }),
        }
    }

    // --- Save data (host persists) ------------------------------------------

    /// The current battery-backed save contents, for the host to write out.
    pub fn save_data(&self) -> &[u8] {
        match self {
            Emulator::Gba(g) => g.system.gba.bus.cartridge.backup_bytes(),
            Emulator::Nds(_) => &[],
        }
    }

    /// Restore previously saved contents.
    pub fn load_save_data(&mut self, data: &[u8]) {
        match self {
            Emulator::Gba(g) => g.system.gba.bus.cartridge.load_backup(data),
            Emulator::Nds(_) => {}
        }
    }

    /// Whether the save has been written since the last [`Self::clear_save_dirty`]
    /// — the host flushes only when this is true.
    pub fn save_dirty(&self) -> bool {
        match self {
            Emulator::Gba(g) => g.system.gba.bus.cartridge.backup_dirty(),
            Emulator::Nds(_) => false,
        }
    }

    /// Acknowledge that the host has persisted the current save.
    pub fn clear_save_dirty(&mut self) {
        match self {
            Emulator::Gba(g) => g.system.gba.bus.cartridge.clear_backup_dirty(),
            Emulator::Nds(_) => {}
        }
    }

    /// The save type's name (e.g. `"flash128k"`), for a sidecar the host writes.
    pub fn save_type_name(&self) -> &'static str {
        match self {
            Emulator::Gba(g) => g.system.gba.bus.cartridge.save_type().name(),
            Emulator::Nds(_) => "none",
        }
    }

    /// Override the save type by name (from a host sidecar). Returns `false` for
    /// an unknown name or a console without configurable save types.
    pub fn set_save_type_by_name(&mut self, name: &str) -> bool {
        match self {
            Emulator::Gba(g) => match gba::SaveType::from_name(name) {
                Some(t) => {
                    g.system.gba.bus.cartridge.set_save_type(t);
                    true
                }
                None => false,
            },
            Emulator::Nds(_) => false,
        }
    }

    // --- Escape hatches (GBA-specific tooling) ------------------------------

    /// The underlying GBA system, if this is a GBA. For GBA-specific tooling (the
    /// `debug` inspector, dev toggles) that has no console-agnostic equivalent yet.
    pub fn as_gba(&self) -> Option<&gba::System> {
        match self {
            Emulator::Gba(g) => Some(&g.system),
            Emulator::Nds(_) => None,
        }
    }

    /// Mutable counterpart to [`Self::as_gba`].
    pub fn as_gba_mut(&mut self) -> Option<&mut gba::System> {
        match self {
            Emulator::Gba(g) => Some(&mut g.system),
            Emulator::Nds(_) => None,
        }
    }

    /// Take the underlying GBA system, consuming the facade. Used to hand the
    /// concrete machine to the (currently GBA-only) debug server.
    pub fn into_gba(self) -> Option<gba::System> {
        match self {
            Emulator::Gba(g) => Some(g.system),
            Emulator::Nds(_) => None,
        }
    }
}

/// A DS behind the facade: the system plus its two RGBA present buffers (top =
/// Engine A; bottom is black until Engine B lands).
pub struct NdsEmulator {
    system: nds::System,
    rgba: [Vec<u8>; 2],
}

impl NdsEmulator {
    fn new(system: nds::System) -> Self {
        NdsEmulator {
            system,
            rgba: [
                vec![0; nds::ppu::WIDTH * nds::ppu::HEIGHT * 4],
                vec![0; nds::ppu::WIDTH * nds::ppu::HEIGHT * 4],
            ],
        }
    }

    fn run_frame(&mut self) {
        self.system.run_frame();
        // Both physical screens, each from its POWCNT1-assigned 2D engine.
        for screen in 0..2 {
            for (px, &color) in self.rgba[screen]
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(self.system.screen(screen))
            {
                *px = bgr555_to_rgba8(color);
            }
        }
    }
}

/// Convert a BGR555 pixel to RGBA8.
fn bgr555_to_rgba8(color: u16) -> [u8; 4] {
    let r = (color & 0x1F) as u8;
    let g = ((color >> 5) & 0x1F) as u8;
    let b = ((color >> 10) & 0x1F) as u8;
    // Scale 5-bit to 8-bit by replicating the high bits into the low.
    [(r << 3) | (r >> 2), (g << 3) | (g >> 2), (b << 3) | (b >> 2), 255]
}

/// GBA screen dimensions.
mod gba_screen {
    pub const WIDTH: usize = 240;
    pub const HEIGHT: usize = 160;
}

/// A GBA behind the facade: the system plus the host-facing buffers the facade
/// owns (the presentable RGBA image and the optional audio sink).
pub struct GbaEmulator {
    system: gba::System,
    /// The presentable image, RGBA8, refreshed each frame from the BGR555
    /// framebuffer.
    rgba: Vec<u8>,
    audio: Option<audio::AudioSink>,
    audio_muted: bool,
}

impl GbaEmulator {
    fn new(system: gba::System) -> Self {
        let mut g = GbaEmulator {
            system,
            rgba: vec![0; gba_screen::WIDTH * gba_screen::HEIGHT * 4],
            audio: None,
            audio_muted: false,
        };
        g.refresh_rgba();
        g
    }

    fn run_frame(&mut self) {
        self.system.run_frame();
        self.refresh_rgba();
        // The APU buffer is always drained so it cannot grow unbounded, but the
        // samples are fed to the host only when audio is attached and unmuted.
        let samples = self.system.take_audio();
        if let Some(sink) = self.audio.as_mut() {
            if !self.audio_muted {
                sink.push(&samples);
            }
        }
    }

    /// Convert the canonical BGR555 framebuffer into the RGBA8 present buffer.
    fn refresh_rgba(&mut self) {
        for (px, color) in self.rgba.as_chunks_mut::<4>().0.iter_mut().zip(self.system.framebuffer()) {
            *px = color.to_rgba8();
        }
    }

    fn set_input(&mut self, input: Input) {
        use gba::Key;
        const MAP: &[(u32, Key)] = &[
            (button::A, Key::A),
            (button::B, Key::B),
            (button::SELECT, Key::Select),
            (button::START, Key::Start),
            (button::RIGHT, Key::Right),
            (button::LEFT, Key::Left),
            (button::UP, Key::Up),
            (button::DOWN, Key::Down),
            (button::R, Key::R),
            (button::L, Key::L),
        ];
        for &(bit, key) in MAP {
            self.system.set_key(key, input.buttons & bit != 0);
        }
        // Touch and X/Y have no GBA equivalent and are ignored.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid-looking BIOS/ROM pair for boot smoke tests (contents do not
    /// matter; the machine just needs bytes to fetch).
    fn tiny_bios() -> Vec<u8> {
        vec![0; 0x4000]
    }

    #[test]
    fn detects_gba_by_header_byte() {
        let mut rom = vec![0u8; 0x200];
        rom[0xB2] = 0x96;
        assert_eq!(Console::detect(&rom), Some(Console::Gba));
    }

    #[test]
    fn gba_boots_bios_only_and_presents_one_screen() {
        let bios = tiny_bios();
        let mut emu = Emulator::load(Load {
            console: Some(Console::Gba),
            rom: None,
            bios: Some(&bios),
            bios7: None,
        })
        .expect("bios-only GBA boot");
        assert_eq!(emu.console(), Console::Gba);
        assert_eq!(emu.screen_count(), 1);
        emu.run_frame();
        let screen = emu.screen(0).expect("screen 0");
        assert_eq!(screen.width, 240);
        assert_eq!(screen.height, 160);
        assert_eq!(screen.rgba.len(), 240 * 160 * 4);
        assert!(emu.screen(1).is_none());
    }

    #[test]
    fn gba_without_bios_is_an_error() {
        let result = Emulator::load(Load {
            console: Some(Console::Gba),
            rom: None,
            bios: None,
            bios7: None,
        });
        assert!(matches!(result, Err(LoadError::MissingBios)));
    }

    #[test]
    fn nds_boots_bare_and_presents_two_screens() {
        let mut emu = Emulator::load(Load {
            console: Some(Console::Nds),
            rom: None,
            bios: None,
            bios7: None,
        })
        .expect("bare NDS");
        assert_eq!(emu.console(), Console::Nds);
        assert_eq!(emu.screen_count(), 2);
        emu.run_frame();
        for i in 0..2 {
            let screen = emu.screen(i).expect("screen");
            assert_eq!(screen.width, 256);
            assert_eq!(screen.height, 192);
            assert_eq!(screen.rgba.len(), 256 * 192 * 4);
        }
        assert!(emu.screen(2).is_none());
    }

    #[test]
    fn nds_direct_boots_a_real_rom_to_a_rendered_frame() {
        fn put(rom: &mut [u8], off: usize, v: u32) {
            rom[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }
        let mut rom = vec![0u8; 0x6000];
        // Header: ARM9 binary at rom 0x4000 → 0x0200_0000; ARM7 at 0x5000 → 0x0210_0000.
        put(&mut rom, 0x20, 0x4000);
        put(&mut rom, 0x24, 0x0200_0000);
        put(&mut rom, 0x28, 0x0200_0000);
        put(&mut rom, 0x2C, 7 * 4);
        put(&mut rom, 0x30, 0x5000);
        put(&mut rom, 0x34, 0x0210_0000);
        put(&mut rom, 0x38, 0x0210_0000);
        put(&mut rom, 0x3C, 4);
        // ARM9: BG palette entry 0 = green, DISPCNT = graphics mode → the compositor
        // fills the top screen with the backdrop. Then park.
        let arm9 = [
            0xE3A0_0650u32, // mov r0, #0x0500_0000  (BG palette)
            0xE3A0_2E3E,    // mov r2, #0x03E0       (BGR555 green)
            0xE1C0_20B0,    // strh r2, [r0]
            0xE3A0_1640,    // mov r1, #0x0400_0000  (DISPCNT)
            0xE3A0_3801,    // mov r3, #0x0001_0000  (display mode 1 = graphics)
            0xE581_3000,    // str r3, [r1]
            0xEAFF_FFFE,    // b .
        ];
        for (i, w) in arm9.iter().enumerate() {
            put(&mut rom, 0x4000 + i * 4, *w);
        }
        put(&mut rom, 0x5000, 0xEAFF_FFFE); // ARM7: b .

        let mut emu = Emulator::load(Load {
            console: Some(Console::Nds),
            rom: Some(&rom),
            bios: None,
            bios7: None,
        })
        .expect("direct boot");
        emu.run_frame();

        // The top screen shows the green backdrop the ARM9 code programmed.
        let top = emu.screen(0).unwrap();
        assert_eq!(&top.rgba[0..4], &[0, 255, 0, 255]);
        assert!(top.rgba.as_chunks::<4>().0.iter().all(|p| *p == [0, 255, 0, 255]));
    }

    #[test]
    fn input_maps_buttons_and_ignores_ds_only_bits() {
        let bios = tiny_bios();
        let mut emu = Emulator::load(Load {
            console: Some(Console::Gba),
            rom: None,
            bios: Some(&bios),
            bios7: None,
        })
        .unwrap();
        let mut input = Input::default();
        input.set(button::A, true);
        input.set(button::X, true); // DS-only, must be harmless on GBA
        emu.set_input(input);
        let system = emu.as_gba().unwrap();
        assert!(system.gba.bus.io.keypad.is_pressed(gba::Key::A));
    }
}
