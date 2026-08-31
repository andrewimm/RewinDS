//! RewinDS desktop runner: boots a GBA BIOS (and an optional cartridge) into a
//! window, mapping the host keyboard to the GBA keypad.
//!
//! Presentation uses minifb — a simple CPU-blitted pixel buffer — as the first
//! bootable milestone. The emulator core is backend-agnostic: it produces a
//! 240x160 BGR555 framebuffer and consumes keypad input. A future wgpu renderer
//! will replace only the window/present code in this file, reusing [`System`],
//! [`System::run_frame`], [`System::framebuffer`], and the key map below.

mod audio;
mod logging;

use gba::{Cartridge, Key as Button, SaveType, System};
use minifb::{Key, Scale, Window, WindowOptions};
use std::error::Error;
use std::path::{Path, PathBuf};

const WIDTH: usize = 240;
const HEIGHT: usize = 160;

/// Host key → GBA button. Arrow keys drive the D-pad; Z/X are A/B; A/S are the
/// L/R shoulders; Enter/Backspace are Start/Select.
const KEY_MAP: &[(Key, Button)] = &[
    (Key::Z, Button::A),
    (Key::X, Button::B),
    (Key::Enter, Button::Start),
    (Key::Backspace, Button::Select),
    (Key::Up, Button::Up),
    (Key::Down, Button::Down),
    (Key::Left, Button::Left),
    (Key::Right, Button::Right),
    (Key::A, Button::L),
    (Key::S, Button::R),
];

fn main() -> Result<(), Box<dyn Error>> {
    logging::init();

    // Parse args: positional bios [rom], plus optional --debug-port <port>.
    let mut positional = Vec::new();
    let mut debug_port = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--debug-port" => {
                debug_port = Some(args.next().ok_or("--debug-port needs a value")?.parse::<u16>()?);
            }
            _ => positional.push(arg),
        }
    }
    let bios_path = positional
        .first()
        .ok_or("usage: rewinds <bios.bin> [rom.gba] [--debug-port N]")?;

    let mut system = System::new();
    system.gba.bus.load_bios(&std::fs::read(bios_path)?);

    // A loaded ROM brings a save backup: its type is detected from the ROM, an
    // optional `<rom>.sav.meta` sidecar may override it, and a `<rom>.sav` file
    // (raw chip dump, interoperable with other emulators) is restored if present.
    let mut save_paths: Option<(PathBuf, PathBuf)> = None;
    if let Some(rom_path) = positional.get(1) {
        system.gba.bus.load_rom(std::fs::read(rom_path)?);
        let sav = Path::new(rom_path).with_extension("sav");
        let meta = {
            let mut m = sav.clone().into_os_string();
            m.push(".meta");
            PathBuf::from(m)
        };
        if let Some(save_type) = read_meta_save_type(&meta) {
            system.gba.bus.cartridge.set_save_type(save_type);
        }
        if let Ok(bytes) = std::fs::read(&sav) {
            system.gba.bus.cartridge.load_backup(&bytes);
        }
        let save_type = system.gba.bus.cartridge.save_type();
        log::info!("cartridge save: {} ({} bytes)", save_type.name(), save_type.backup_size());
        save_paths = Some((sav, meta));
    }

    // The CPU begins at the BIOS reset vector.
    system.cpu.set_pc(0);

    // Headless debug-server mode: the client drives execution and inspects state.
    if let Some(port) = debug_port {
        debug::server::serve(system, port)?;
        return Ok(());
    }

    let mut window = Window::new(
        "RewinDS",
        WIDTH,
        HEIGHT,
        WindowOptions {
            scale: Scale::X4,
            ..WindowOptions::default()
        },
    )?;
    window.set_target_fps(60);

    // Host audio output (silent if no device is available).
    let mut audio = audio::Audio::new();

    // Reused each frame: the BGR555 framebuffer converted to minifb's 0x00RRGGBB.
    let mut buffer = vec![0u32; WIDTH * HEIGHT];
    // Flush the save at most a few times a second, only after the game writes it.
    const FLUSH_EVERY_FRAMES: u32 = 180;
    let mut frames_since_flush = 0u32;
    while window.is_open() && !window.is_key_down(Key::Escape) {
        // Mirror the host keyboard into the keypad each frame.
        for &(host, button) in KEY_MAP {
            system.set_key(button, window.is_key_down(host));
        }

        // Debug mix mutes: F1 = DirectSound, F2 = PSG.
        if window.is_key_pressed(Key::F1, minifb::KeyRepeat::No) {
            let muted = system.gba.bus.io.apu.toggle_mute_directsound();
            log::info!("DirectSound {}", if muted { "muted" } else { "unmuted" });
        }
        if window.is_key_pressed(Key::F2, minifb::KeyRepeat::No) {
            let muted = system.gba.bus.io.apu.toggle_mute_psg();
            log::info!("PSG {}", if muted { "muted" } else { "unmuted" });
        }
        // F3 = A/B the output low-pass filter.
        if window.is_key_pressed(Key::F3, minifb::KeyRepeat::No) {
            let on = system.gba.bus.io.apu.toggle_low_pass();
            log::info!("low-pass filter {}", if on { "on" } else { "off" });
        }

        system.run_frame();

        if let Some(a) = audio.as_mut() {
            a.push(&system.take_audio());
        }

        for (out, color) in buffer.iter_mut().zip(system.framebuffer()) {
            let [r, g, b, _] = color.to_rgba8();
            *out = (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
        }
        window.update_with_buffer(&buffer, WIDTH, HEIGHT)?;

        if let Some((sav, meta)) = &save_paths {
            frames_since_flush += 1;
            if frames_since_flush >= FLUSH_EVERY_FRAMES && system.gba.bus.cartridge.backup_dirty() {
                if write_save(sav, meta, &system.gba.bus.cartridge).is_ok() {
                    system.gba.bus.cartridge.clear_backup_dirty();
                }
                frames_since_flush = 0;
            }
        }
    }

    // Final flush on exit so the last writes are not lost.
    if let Some((sav, meta)) = &save_paths {
        if system.gba.bus.cartridge.backup_dirty() {
            if let Err(e) = write_save(sav, meta, &system.gba.bus.cartridge) {
                log::warn!("could not write save {}: {e}", sav.display());
            }
        }
    }
    Ok(())
}

/// Read the `save_type` override from a `.sav.meta` sidecar, if present and valid.
fn read_meta_save_type(path: &Path) -> Option<SaveType> {
    let text = std::fs::read_to_string(path).ok()?;
    let key = "\"save_type\"";
    let rest = &text[text.find(key)? + key.len()..];
    let start = rest.find('"')? + 1;
    let end = rest[start..].find('"')? + start;
    SaveType::from_name(&rest[start..end])
}

/// Write the raw `.sav` (chip dump) plus a small JSON sidecar recording the save
/// type. Absent backups (empty dump) write nothing.
fn write_save(sav: &Path, meta: &Path, cartridge: &Cartridge) -> std::io::Result<()> {
    let bytes = cartridge.backup_bytes();
    if bytes.is_empty() {
        return Ok(());
    }
    std::fs::write(sav, bytes)?;
    std::fs::write(
        meta,
        format!("{{\n  \"save_type\": \"{}\"\n}}\n", cartridge.save_type().name()),
    )?;
    Ok(())
}
