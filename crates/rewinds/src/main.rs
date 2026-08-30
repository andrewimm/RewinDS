//! RewinDS desktop runner: boots a GBA BIOS (and an optional cartridge) into a
//! window, mapping the host keyboard to the GBA keypad.
//!
//! Presentation uses minifb — a simple CPU-blitted pixel buffer — as the first
//! bootable milestone. The emulator core is backend-agnostic: it produces a
//! 240x160 BGR555 framebuffer and consumes keypad input. A future wgpu renderer
//! will replace only the window/present code in this file, reusing [`System`],
//! [`System::run_frame`], [`System::framebuffer`], and the key map below.

use gba::{Key as Button, System};
use minifb::{Key, Scale, Window, WindowOptions};
use std::error::Error;

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
    if let Some(rom_path) = positional.get(1) {
        system.gba.bus.load_rom(std::fs::read(rom_path)?);
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

    // Reused each frame: the BGR555 framebuffer converted to minifb's 0x00RRGGBB.
    let mut buffer = vec![0u32; WIDTH * HEIGHT];
    while window.is_open() && !window.is_key_down(Key::Escape) {
        // Mirror the host keyboard into the keypad each frame.
        for &(host, button) in KEY_MAP {
            system.set_key(button, window.is_key_down(host));
        }

        system.run_frame();

        for (out, color) in buffer.iter_mut().zip(system.framebuffer()) {
            let [r, g, b, _] = color.to_rgba8();
            *out = (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
        }
        window.update_with_buffer(&buffer, WIDTH, HEIGHT)?;
    }
    Ok(())
}
