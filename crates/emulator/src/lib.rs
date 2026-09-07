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
    /// A DS firmware dump. When present (with both BIOS images), the DS runs the
    /// real firmware boot instead of direct boot — required by games that reject
    /// direct boot. Absent → direct boot.
    pub firmware: Option<&'a [u8]>,
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
                // A real BIOS image is optional: without one, fall back to the
                // built-in freely redistributable replacement.
                let bios: &[u8] = match cfg.bios {
                    Some(bios) => bios,
                    None => gba::default_bios(),
                };
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
                // Boot the cartridge if present; a bare DS (no ROM) still runs and
                // presents its two screens. A supplied firmware dump (with both
                // BIOS images) selects the real firmware boot; otherwise direct boot.
                if let Some(rom) = cfg.rom {
                    match (cfg.firmware, cfg.bios, cfg.bios7) {
                        (Some(fw), Some(_), Some(_)) => {
                            system.load_firmware(fw);
                            system.firmware_boot(rom).map_err(LoadError::BadImage)?;
                        }
                        _ => system.direct_boot(rom).map_err(LoadError::BadImage)?,
                    }
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

    /// The number of video frames presented so far (for frame-targeted run control).
    pub fn frame(&self) -> u64 {
        match self {
            Emulator::Gba(g) => g.system.gba.bus.io.video.frame(),
            Emulator::Nds(n) => n.system.frame(),
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
            // The DS keypad shares the low-ten bit order; touch maps the lower screen.
            Emulator::Nds(n) => {
                n.system.set_keypad(input.buttons);
                n.system.set_touch(input.touch_pressed.then_some((
                    input.touch_x as i32,
                    input.touch_y as i32,
                )));
            }
        }
    }

    /// Open or close the DS clamshell lid (`closed = true` shuts it). Unlike the
    /// per-frame [`Self::set_input`] snapshot, the lid is discrete, sticky console
    /// state: closing raises the hinge interrupt so the game enters sleep; opening
    /// raises it again so the game wakes. It changes only on real events, so the host
    /// drives it directly rather than folding it into every input frame.
    ///
    /// This is the surface a native host toggles on lifecycle transitions — e.g. an
    /// iOS app closing the lid in `applicationDidEnterBackground` and opening it in
    /// `applicationWillEnterForeground`, so the game suspends and resumes with the app.
    /// Idempotent (no change, no interrupt) and a no-op on the GBA, which has no lid.
    pub fn set_lid(&mut self, closed: bool) {
        if let Emulator::Nds(n) = self {
            n.system.set_lid(closed);
        }
    }

    /// Mute audio capture: while muted, [`Self::run_frame`] still runs but drops
    /// the frame's samples instead of feeding the sink. Used for fast-forward.
    pub fn set_audio_muted(&mut self, muted: bool) {
        match self {
            Emulator::Gba(g) => g.audio_muted = muted,
            Emulator::Nds(n) => n.audio_muted = muted,
        }
    }

    /// Attach a host audio output at `output_rate` Hz with `channels` channels,
    /// returning the consumer the host drains on its audio thread. Replaces any
    /// previous attachment.
    pub fn enable_audio(&mut self, output_rate: u32, channels: usize) -> AudioSource {
        let (sink, source) = audio::channel(output_rate, channels);
        match self {
            Emulator::Gba(g) => g.audio = Some(sink),
            Emulator::Nds(n) => n.audio = Some(sink),
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

    /// Re-derive the RGBA present buffers from the machine's current framebuffer,
    /// so [`Self::screen`] reflects the latest rendered frame even after partial
    /// runs (single-stepping) that did not go through [`Self::run_frame`].
    pub fn present(&mut self) {
        match self {
            Emulator::Gba(g) => g.refresh_rgba(),
            Emulator::Nds(n) => n.refresh_rgba(),
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
            Emulator::Nds(n) => n.system.cart_backup(),
        }
    }

    /// Restore previously saved contents.
    pub fn load_save_data(&mut self, data: &[u8]) {
        match self {
            Emulator::Gba(g) => g.system.gba.bus.cartridge.load_backup(data),
            Emulator::Nds(n) => n.system.load_cart_backup(data),
        }
    }

    /// Whether the save has been written since the last [`Self::clear_save_dirty`]
    /// — the host flushes only when this is true.
    pub fn save_dirty(&self) -> bool {
        match self {
            Emulator::Gba(g) => g.system.gba.bus.cartridge.backup_dirty(),
            Emulator::Nds(n) => n.system.cart_backup_dirty(),
        }
    }

    /// Acknowledge that the host has persisted the current save.
    pub fn clear_save_dirty(&mut self) {
        match self {
            Emulator::Gba(g) => g.system.gba.bus.cartridge.clear_backup_dirty(),
            Emulator::Nds(n) => n.system.clear_cart_backup_dirty(),
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

    // --- Serial link (host owns the carrier) --------------------------------

    /// Configure the serial link: whether a carrier is attached, this unit's id
    /// (0 = parent/master, 1-3 = child), and the number of linked units (2-4).
    /// Discrete, host-driven (like [`Self::set_lid`]). No-op on the DS.
    pub fn set_link_config(&mut self, connected: bool, id: u8, count: u8) {
        if let Emulator::Gba(g) = self {
            g.system.gba.bus.io.serial_set_link(connected, id, count);
        }
    }

    /// Take the next serial frame the core wants transmitted, as opaque bytes for
    /// the host to relay over its carrier. The slice borrows the emulator and is
    /// valid until the next mutating call. `None` when there is nothing to send.
    pub fn link_poll_out(&mut self) -> Option<&[u8]> {
        match self {
            Emulator::Gba(g) => match g.system.gba.bus.io.serial_poll_out() {
                Some(bytes) => {
                    g.link_out = bytes;
                    Some(&g.link_out)
                }
                None => None,
            },
            Emulator::Nds(_) => None,
        }
    }

    /// Deliver a peer's serial frame received from the carrier (may complete a
    /// transfer and raise the serial interrupt). No-op on the DS.
    pub fn link_deliver(&mut self, bytes: &[u8]) {
        if let Emulator::Gba(g) = self {
            g.system.gba.bus.io.serial_deliver(bytes);
        }
    }

    /// Whether a serial transfer is in progress — the host should keep pumping
    /// [`Self::link_poll_out`] / [`Self::link_deliver`].
    pub fn link_pending(&self) -> bool {
        match self {
            Emulator::Gba(g) => g.system.gba.bus.io.serial_pending(),
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

    /// Wrap an already-constructed GBA system as an emulator (for tooling and tests
    /// that build a system directly rather than through [`Self::load`]).
    pub fn from_gba_system(system: gba::System) -> Emulator {
        Emulator::Gba(GbaEmulator::new(system))
    }

    /// Wrap an already-constructed NDS system as an emulator.
    pub fn from_nds_system(system: nds::System) -> Emulator {
        Emulator::Nds(NdsEmulator::new(system))
    }

    /// The underlying NDS system, if this is an NDS. Counterpart to [`Self::as_gba`],
    /// for the `debug` inspector's NDS path.
    pub fn as_nds(&self) -> Option<&nds::System> {
        match self {
            Emulator::Nds(n) => Some(&n.system),
            Emulator::Gba(_) => None,
        }
    }

    /// Mutable counterpart to [`Self::as_nds`].
    pub fn as_nds_mut(&mut self) -> Option<&mut nds::System> {
        match self {
            Emulator::Nds(n) => Some(&mut n.system),
            Emulator::Gba(_) => None,
        }
    }

    /// A human-readable graphics-state diagnostic for the DS, or `None` on GBA. Used by
    /// the host's "dump state" key to capture a live screen for debugging.
    pub fn nds_debug_report(&mut self) -> Option<String> {
        match self {
            Emulator::Nds(n) => Some(n.system.debug_report()),
            Emulator::Gba(_) => None,
        }
    }

    /// The 3D render list rasterized with the depth test disabled (256×192 BGR555). Debug
    /// (NDS only) — black here means genuinely uncovered by geometry, not depth-rejected.
    pub fn nds_debug_no_depth(&self) -> Option<Vec<u16>> {
        match self {
            Emulator::Nds(n) => Some(n.system.gpu3d_debug_no_depth()),
            Emulator::Gba(_) => None,
        }
    }

    /// Debug (NDS only): toggle 3D winding culling off/on, to test whether black regions
    /// are back-face-culled geometry.
    pub fn nds_set_disable_cull(&mut self, on: bool) {
        if let Emulator::Nds(n) = self {
            n.system.gpu3d_set_disable_cull(on);
        }
    }

    /// Decode a 3D render-list polygon's full texture to `(width, height, BGR555)`, to
    /// inspect the texel sampler directly. Debug (NDS only).
    pub fn nds_dump_poly_texture(&self, poly: usize) -> Option<(u32, u32, Vec<u16>)> {
        match self {
            Emulator::Nds(n) => n.system.gpu3d_dump_poly_texture(poly),
            Emulator::Gba(_) => None,
        }
    }

    /// The isolated 256×192 BGR555 framebuffer for one BG (`0..4`) or OBJ (`4`) of a DS
    /// engine — force-enabled, so a *disabled* layer's content is still visible. Debug.
    pub fn nds_debug_layer(&mut self, engine: usize, layer: usize) -> Option<Vec<u16>> {
        match self {
            Emulator::Nds(n) => Some(n.system.debug_render_layer(engine, layer)),
            Emulator::Gba(_) => None,
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
    audio: Option<audio::AudioSink>,
    audio_muted: bool,
}

impl NdsEmulator {
    fn new(system: nds::System) -> Self {
        NdsEmulator {
            system,
            rgba: [
                vec![0; nds::ppu::WIDTH * nds::ppu::HEIGHT * 4],
                vec![0; nds::ppu::WIDTH * nds::ppu::HEIGHT * 4],
            ],
            audio: None,
            audio_muted: false,
        }
    }

    fn run_frame(&mut self) {
        self.system.run_frame();
        self.refresh_rgba();
        // Always drain (so the buffer can't grow unbounded); feed only when attached.
        let samples = self.system.take_audio();
        if let Some(sink) = self.audio.as_mut() {
            if !self.audio_muted {
                sink.push(&samples);
            }
        }
    }

    /// Convert both engines' BGR555 framebuffers into the RGBA8 present buffers.
    /// Both physical screens, each from its POWCNT1-assigned 2D engine.
    fn refresh_rgba(&mut self) {
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
    /// Stable backing for the outbound serial frame returned by
    /// [`Emulator::link_poll_out`] (valid until the next mutating call).
    link_out: [u8; gba::LINK_FRAME_LEN],
}

impl GbaEmulator {
    fn new(system: gba::System) -> Self {
        let mut g = GbaEmulator {
            system,
            rgba: vec![0; gba_screen::WIDTH * gba_screen::HEIGHT * 4],
            audio: None,
            audio_muted: false,
            link_out: [0; gba::LINK_FRAME_LEN],
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
            firmware: None,
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
    fn gba_without_bios_falls_back_to_the_builtin() {
        // No BIOS supplied: the GBA boots on the built-in replacement image.
        let mut emu = Emulator::load(Load {
            console: Some(Console::Gba),
            rom: None,
            bios: None,
            bios7: None,
            firmware: None,
        })
        .expect("GBA boots without a supplied BIOS");
        assert_eq!(emu.console(), Console::Gba);
        emu.run_frame();
    }

    #[test]
    fn nds_boots_bare_and_presents_two_screens() {
        let mut emu = Emulator::load(Load {
            console: Some(Console::Nds),
            rom: None,
            bios: None,
            bios7: None,
            firmware: None,
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
            firmware: None,
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
            firmware: None,
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
