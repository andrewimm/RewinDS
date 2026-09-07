//! RewinDS desktop runner: boots a GBA BIOS (and an optional cartridge) into a
//! window, mapping the host keyboard to console input.
//!
//! It is the *reference host* for the [`Emulator`] facade — a thin Rust app that
//! drives the exact console-agnostic surface (load, run a frame, feed input, read
//! screens, pull audio, exchange save bytes) that a native Swift app will drive
//! through the C FFI. Presentation uses minifb, a simple CPU-blitted pixel buffer;
//! a future wgpu renderer would replace only the window/present code here.

mod audio;
mod link;
mod logging;

use link::Transport;

use emulator::{button, Console, Emulator, Input, Load};
use minifb::{Key, MouseButton, MouseMode, Scale, Window, WindowOptions};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Host key → console button bit. Arrow keys drive the D-pad; X/Z are A/B; A/S are
/// the L/R shoulders; Enter/Backspace are Start/Select. Hold Space to fast-forward.
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

const USAGE: &str = "usage: rewinds <rom.gba|rom.nds> [--bios <path>] [--bios7 <path>] \
[--debug-port N] [--link listen <port> | --link connect <host:port>]";

fn main() -> Result<(), Box<dyn Error>> {
    logging::init();

    // Parse args: a positional ROM plus --bios / --bios7 / --debug-port.
    let mut rom_path: Option<String> = None;
    let mut bios_path: Option<String> = None;
    let mut bios7_path: Option<String> = None;
    let mut firmware_path: Option<String> = None;
    let mut debug_port = None;
    let mut link_listen: Option<u16> = None;
    let mut link_connect: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bios" => bios_path = Some(args.next().ok_or("--bios needs a value")?),
            "--bios7" => bios7_path = Some(args.next().ok_or("--bios7 needs a value")?),
            "--firmware" => firmware_path = Some(args.next().ok_or("--firmware needs a value")?),
            "--debug-port" => {
                debug_port = Some(args.next().ok_or("--debug-port needs a value")?.parse::<u16>()?);
            }
            "--link" => match args.next().as_deref() {
                Some("listen") => {
                    link_listen = Some(args.next().ok_or("--link listen needs a port")?.parse()?);
                }
                Some("connect") => {
                    link_connect = Some(args.next().ok_or("--link connect needs host:port")?);
                }
                _ => return Err(format!("--link expects 'listen <port>' or 'connect <host:port>'\n{USAGE}").into()),
            },
            other if other.starts_with("--") => {
                return Err(format!("unknown option {other}\n{USAGE}").into());
            }
            _ => rom_path = Some(arg),
        }
    }

    // The ROM's extension selects the console (a bare `--bios` boots the GBA BIOS).
    let console = match rom_path.as_deref() {
        Some(p) if p.ends_with(".nds") => Console::Nds,
        Some(_) => Console::Gba,
        None => Console::Gba,
    };

    let rom = rom_path.as_ref().map(std::fs::read).transpose()?;
    let bios = bios_path.as_ref().map(std::fs::read).transpose()?;
    let bios7 = bios7_path.as_ref().map(std::fs::read).transpose()?;
    let firmware = firmware_path.as_ref().map(std::fs::read).transpose()?;
    // With no `--bios`, the GBA falls back to the built-in replacement BIOS.
    let mut emulator = Emulator::load(Load {
        console: Some(console),
        rom: rom.as_deref(),
        bios: bios.as_deref(),
        bios7: bios7.as_deref(),
        firmware: firmware.as_deref(),
    })?;

    // Optional serial link: this host owns the carrier (TCP here). The emulator
    // core stays carrier-blind — it just produces/consumes opaque frame bytes.
    let mut serial_link: Option<link::TcpLink> = match (link_listen, link_connect) {
        (Some(port), _) => Some(link::TcpLink::listen(port)?),
        (_, Some(addr)) => Some(link::TcpLink::connect(&addr)?),
        _ => None,
    };
    if let Some(l) = &serial_link {
        emulator.set_link_config(true, l.id(), l.count());
    }

    // A loaded GBA ROM brings a save backup: its type is detected from the ROM, an
    // optional `<rom>.sav.meta` sidecar may override it, and a `<rom>.sav` file
    // (raw chip dump) is restored if present. Persistence is the host's job.
    let mut save_paths: Option<(PathBuf, PathBuf)> = None;
    if console == Console::Gba {
        if let Some(rp) = rom_path.as_deref() {
        let sav = Path::new(rp).with_extension("sav");
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
    }

    // Headless debug-server mode: the client drives execution and inspects state.
    // The debug server now drives the console-agnostic facade, so it works for GBA
    // and DS alike (DS methods take an engine/core selector).
    if let Some(port) = debug_port {
        debug::server::serve(emulator, port)?;
        return Ok(());
    }

    // Window geometry from the console's screens: each screen's width, and the
    // screens stacked vertically (GBA: one; NDS: two, top over bottom).
    let screen_w = emulator.screen(0).expect("a screen").width as usize;
    let screen_h = emulator.screen(0).expect("a screen").height as usize;
    let screen_count = emulator.screen_count();
    let (win_w, win_h) = (screen_w, screen_h * screen_count);
    let scale = if console == Console::Nds { Scale::X2 } else { Scale::X4 };
    let mut window = Window::new(
        "RewinDS",
        win_w,
        win_h,
        WindowOptions {
            scale,
            ..WindowOptions::default()
        },
    )?;
    window.set_target_fps(60);

    // Host audio output (silent if no device is available). Kept alive so the
    // stream keeps playing.
    let _audio = audio::Audio::open(&mut emulator);

    // Reused each frame: the RGBA present buffer packed into minifb's 0x00RRGGBB.
    let mut buffer = vec![0u32; win_w * win_h];
    // Flush the save at most a few times a second, only after the game writes it.
    const FLUSH_EVERY_FRAMES: u32 = 180;
    let mut frames_since_flush = 0u32;
    let mut cull_disabled = false;
    while window.is_open() && !window.is_key_down(Key::Escape) {
        // Mirror the host keyboard into a single input snapshot each frame.
        let mut input = Input::default();
        for &(host, bit) in KEY_MAP {
            input.set(bit, window.is_key_down(host));
        }
        // Mouse over the lower screen drives the DS touchscreen. The screens stack
        // top-over-bottom, so the lower screen occupies buffer rows [screen_h, 2·h);
        // subtract that to get a pixel within the touch panel.
        if screen_count > 1 && window.get_mouse_down(MouseButton::Left) {
            if let Some((mx, my)) = window.get_mouse_pos(MouseMode::Discard) {
                let (px, py) = (mx as i32, my as i32 - screen_h as i32);
                if px >= 0 && px < screen_w as i32 && py >= 0 && py < screen_h as i32 {
                    input.touch_x = px as i16;
                    input.touch_y = py as i16;
                    input.touch_pressed = true;
                }
            }
        }
        emulator.set_input(input);
        // Hold L to shut the clamshell lid — the same discrete call a native host makes
        // on background/foreground (raises the hinge interrupt; the game sleeps while
        // held and wakes when released). Idempotent, so driving it every frame is fine.
        emulator.set_lid(window.is_key_down(Key::L));

        // Serial link: deliver any peer frames that have arrived. The SIO busy-wait
        // is itself the sync barrier, so no extra frame pacing is needed.
        if let Some(l) = serial_link.as_mut() {
            for frame in l.recv() {
                emulator.link_deliver(&frame);
            }
        }

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

        // F6: toggle 3D winding culling (DS debug) — to test whether black regions are
        // back-face-culled geometry.
        if window.is_key_pressed(Key::F6, minifb::KeyRepeat::No) {
            cull_disabled = !cull_disabled;
            emulator.nds_set_disable_cull(cull_disabled);
            log::info!("3D culling {}", if cull_disabled { "DISABLED" } else { "enabled" });
        }

        // F5: dump a graphics-state diagnostic + both screens (PPM) to /tmp, for
        // debugging the current on-screen state (DS only).
        if window.is_key_pressed(Key::F5, minifb::KeyRepeat::No) {
            if let Some(report) = emulator.nds_debug_report() {
                let _ = std::fs::write("/tmp/rewinds_state.txt", &report);
                for i in 0..screen_count {
                    let s = emulator.screen(i).expect("screen");
                    let mut ppm = format!("P6\n{} {}\n255\n", s.width, s.height).into_bytes();
                    for px in s.rgba.as_chunks::<4>().0 {
                        ppm.extend_from_slice(&px[..3]);
                    }
                    let _ = std::fs::write(format!("/tmp/rewinds_screen{i}.ppm"), ppm);
                }
                // Dump the 3D render with the depth test disabled: black here is geometry
                // that genuinely doesn't cover the pixel, vs black from depth rejection.
                if let Some(fb) = emulator.nds_debug_no_depth() {
                    let mut ppm = b"P6\n256 192\n255\n".to_vec();
                    for p in &fb {
                        let (r, g, b) = ((p & 0x1F) as u8, ((p >> 5) & 0x1F) as u8, ((p >> 10) & 0x1F) as u8);
                        ppm.extend_from_slice(&[(r << 3) | (r >> 2), (g << 3) | (g >> 2), (b << 3) | (b >> 2)]);
                    }
                    let _ = std::fs::write("/tmp/rewinds_nodepth.ppm", ppm);
                }
                // Dump a spread of 3D polygons' decoded textures, to inspect the texel
                // sampler (e.g. the 4×4 decode) directly for glitch patterns.
                for &poly in &[0usize, 60, 120, 180, 240] {
                    if let Some((tw, th, px)) = emulator.nds_dump_poly_texture(poly) {
                        let mut ppm = format!("P6\n{tw} {th}\n255\n").into_bytes();
                        for p in &px {
                            let (r, g, b) = ((p & 0x1F) as u8, ((p >> 5) & 0x1F) as u8, ((p >> 10) & 0x1F) as u8);
                            ppm.extend_from_slice(&[(r << 3) | (r >> 2), (g << 3) | (g >> 2), (b << 3) | (b >> 2)]);
                        }
                        let _ = std::fs::write(format!("/tmp/rewinds_tex_poly{poly}.ppm"), ppm);
                    }
                }
                // Also dump each Engine-A BG in isolation (force-enabled), so the
                // content of a *disabled* layer is still captured.
                for layer in 0..4 {
                    if let Some(fb) = emulator.nds_debug_layer(0, layer) {
                        let mut ppm = b"P6\n256 192\n255\n".to_vec();
                        for p in &fb {
                            let (r, g, b) = ((p & 0x1F) as u8, ((p >> 5) & 0x1F) as u8, ((p >> 10) & 0x1F) as u8);
                            ppm.extend_from_slice(&[(r << 3) | (r >> 2), (g << 3) | (g >> 2), (b << 3) | (b >> 2)]);
                        }
                        let _ = std::fs::write(format!("/tmp/rewinds_A_bg{layer}.ppm"), ppm);
                    }
                }
                log::info!("dumped graphics state -> /tmp/rewinds_state.txt + screen/BG PPMs");
            }
        }

        // Hold Space to fast-forward: run unthrottled for a display frame's worth
        // of real time, presenting only the final frame and muting audio. Otherwise
        // run one frame at 60 fps with audio.
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

        // Ship whatever serial frames the core produced this step.
        if let Some(l) = serial_link.as_mut() {
            while let Some(frame) = emulator.link_poll_out() {
                let f = frame.to_vec();
                l.send(&f);
            }
        }

        // Present every screen, stacked top-to-bottom.
        for i in 0..screen_count {
            let screen = emulator.screen(i).expect("screen");
            let base = i * screen_h * win_w;
            for (out, px) in buffer[base..].iter_mut().zip(screen.rgba.as_chunks::<4>().0) {
                *out = (u32::from(px[0]) << 16) | (u32::from(px[1]) << 8) | u32::from(px[2]);
            }
        }
        window.update_with_buffer(&buffer, win_w, win_h)?;

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
