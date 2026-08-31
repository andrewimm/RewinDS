//! RewinDS desktop runner: boots a GBA BIOS (and an optional cartridge) into a
//! window, mapping the host keyboard to console input.
//!
//! It is the *reference host* for the [`Emulator`] facade — a thin Rust app that
//! drives the exact console-agnostic surface (load, run a frame, feed input, read
//! screens, pull audio, exchange save bytes) that a native Swift app will drive
//! through the C FFI. Presentation uses minifb, a simple CPU-blitted pixel buffer;
//! a future wgpu renderer would replace only the window/present code here.

mod audio;
mod logging;

use emulator::{button, Console, Emulator, Input, Load};
use minifb::{Key, Scale, Window, WindowOptions};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const WIDTH: usize = 240;
const HEIGHT: usize = 160;

/// Host key → console button bit. Arrow keys drive the D-pad; X/Z are A/B (VBA
/// layout); A/S are the L/R shoulders; Enter/Backspace are Start/Select. Hold
/// Space to fast-forward.
const KEY_MAP: &[(Key, u32)] = &[
    (Key::X, button::A),
    (Key::Z, button::B),
    (Key::Enter, button::START),
    (Key::Backspace, button::SELECT),
    (Key::Up, button::UP),
    (Key::Down, button::DOWN),
    (Key::Left, button::LEFT),
    (Key::Right, button::RIGHT),
    (Key::A, button::L),
    (Key::S, button::R),
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

    let bios = std::fs::read(bios_path)?;
    let rom = positional.get(1).map(std::fs::read).transpose()?;
    let mut emulator = Emulator::load(Load {
        console: Some(Console::Gba),
        rom: rom.as_deref(),
        bios: Some(&bios),
    })?;

    // A loaded ROM brings a save backup: its type is detected from the ROM, an
    // optional `<rom>.sav.meta` sidecar may override it, and a `<rom>.sav` file
    // (raw chip dump, interoperable with other emulators) is restored if present.
    // Persistence is the host's job — the emulator only exchanges the bytes.
    let mut save_paths: Option<(PathBuf, PathBuf)> = None;
    if let Some(rom_path) = positional.get(1) {
        let sav = Path::new(rom_path).with_extension("sav");
        let meta = {
            let mut m = sav.clone().into_os_string();
            m.push(".meta");
            PathBuf::from(m)
        };
        if let Some(name) = read_meta_save_type(&meta) {
            emulator.set_save_type_by_name(&name);
        }
        if let Ok(bytes) = std::fs::read(&sav) {
            emulator.load_save_data(&bytes);
        }
        log::info!("cartridge save: {}", emulator.save_type_name());
        save_paths = Some((sav, meta));
    }

    // Headless debug-server mode: the client drives execution and inspects state.
    // The debug server is a GBA-specific inspector for now, so it takes the
    // concrete machine out of the facade.
    if let Some(port) = debug_port {
        let system = emulator.into_gba().expect("debug server supports GBA only");
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

    // Host audio output (silent if no device is available). Kept alive so the
    // stream keeps playing.
    let _audio = audio::Audio::open(&mut emulator);

    // Reused each frame: the RGBA present buffer packed into minifb's 0x00RRGGBB.
    let mut buffer = vec![0u32; WIDTH * HEIGHT];
    // Flush the save at most a few times a second, only after the game writes it.
    const FLUSH_EVERY_FRAMES: u32 = 180;
    let mut frames_since_flush = 0u32;
    while window.is_open() && !window.is_key_down(Key::Escape) {
        // Mirror the host keyboard into a single input snapshot each frame.
        let mut input = Input::default();
        for &(host, bit) in KEY_MAP {
            input.set(bit, window.is_key_down(host));
        }
        emulator.set_input(input);

        // Debug mix toggles (GBA-specific dev conveniences, via the escape hatch):
        // F1 = DirectSound, F2 = PSG, F3 = output low-pass.
        if let Some(system) = emulator.as_gba_mut() {
            if window.is_key_pressed(Key::F1, minifb::KeyRepeat::No) {
                let muted = system.gba.bus.io.apu.toggle_mute_directsound();
                log::info!("DirectSound {}", if muted { "muted" } else { "unmuted" });
            }
            if window.is_key_pressed(Key::F2, minifb::KeyRepeat::No) {
                let muted = system.gba.bus.io.apu.toggle_mute_psg();
                log::info!("PSG {}", if muted { "muted" } else { "unmuted" });
            }
            if window.is_key_pressed(Key::F3, minifb::KeyRepeat::No) {
                let on = system.gba.bus.io.apu.toggle_low_pass();
                log::info!("low-pass filter {}", if on { "on" } else { "off" });
            }
        }

        // Hold Space to fast-forward: run unthrottled for a display frame's worth
        // of real time, presenting only the final frame and muting audio (like
        // VBA's speed-up). Otherwise run one frame at 60 fps with audio.
        let warp = window.is_key_down(Key::Space);
        window.set_target_fps(if warp { 10_000 } else { 60 });
        emulator.set_audio_muted(warp);
        if warp {
            let deadline = Instant::now() + Duration::from_millis(14);
            loop {
                emulator.run_frame();
                if Instant::now() >= deadline {
                    break;
                }
            }
        } else {
            emulator.run_frame();
        }

        let screen = emulator.screen(0).expect("GBA presents one screen");
        for (out, px) in buffer.iter_mut().zip(screen.rgba.as_chunks::<4>().0) {
            *out = (u32::from(px[0]) << 16) | (u32::from(px[1]) << 8) | u32::from(px[2]);
        }
        window.update_with_buffer(&buffer, WIDTH, HEIGHT)?;

        if let Some((sav, meta)) = &save_paths {
            frames_since_flush += 1;
            if frames_since_flush >= FLUSH_EVERY_FRAMES && emulator.save_dirty() {
                if write_save(sav, meta, &emulator).is_ok() {
                    emulator.clear_save_dirty();
                }
                frames_since_flush = 0;
            }
        }
    }

    // Final flush on exit so the last writes are not lost.
    if let Some((sav, meta)) = &save_paths {
        if emulator.save_dirty() {
            if let Err(e) = write_save(sav, meta, &emulator) {
                log::warn!("could not write save {}: {e}", sav.display());
            }
        }
    }
    Ok(())
}

/// Read the `save_type` override name from a `.sav.meta` sidecar, if present.
fn read_meta_save_type(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let key = "\"save_type\"";
    let rest = &text[text.find(key)? + key.len()..];
    let start = rest.find('"')? + 1;
    let end = rest[start..].find('"')? + start;
    Some(rest[start..end].to_string())
}

/// Write the raw `.sav` (chip dump) plus a small JSON sidecar recording the save
/// type. Absent backups (empty dump) write nothing.
fn write_save(sav: &Path, meta: &Path, emulator: &Emulator) -> std::io::Result<()> {
    let bytes = emulator.save_data();
    if bytes.is_empty() {
        return Ok(());
    }
    std::fs::write(sav, bytes)?;
    std::fs::write(
        meta,
        format!("{{\n  \"save_type\": \"{}\"\n}}\n", emulator.save_type_name()),
    )?;
    Ok(())
}
