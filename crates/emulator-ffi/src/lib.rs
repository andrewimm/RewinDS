//! The extern-C boundary over the [`emulator`] facade.
//!
//! Everything the host needs to run a game crosses here as plain pointers and
//! bytes — the same "C boundary is the host boundary" contract the facade was
//! built for. A native app (the iOS SwiftUI app today) links the static archive
//! this crate produces and drives one opaque [`RewindsEmulator`] handle:
//!
//! 1. [`rewinds_load`] builds a machine from ROM + BIOS byte buffers the host read.
//! 2. Each display refresh: [`rewinds_set_input`], [`rewinds_run_frame`], then
//!    [`rewinds_screen`] to get a pointer to the freshly presented RGBA8 image the
//!    host blits into a texture.
//! 3. Audio is a pull on a separate [`RewindsAudio`] handle the host owns on its
//!    real-time audio thread ([`rewinds_audio_read`]).
//! 4. Battery saves cross out via [`rewinds_save_data`] and back via
//!    [`rewinds_load_save_data`]; the host owns the file.
//!
//! # Safety contract for the host
//!
//! - Byte buffers passed in ([`rewinds_load`], [`rewinds_load_save_data`]) need only
//!   outlive the call — the core copies what it keeps.
//! - Buffers handed *out* ([`rewinds_screen`], [`rewinds_save_data`]) borrow the
//!   emulator and stay valid only until the next call that mutates it (the next
//!   [`rewinds_run_frame`] / load). Copy before then.
//! - Every handle is single-owner. The `RewindsEmulator` is driven from one thread;
//!   the `RewindsAudio` consumer may live on a different (audio) thread, but only one.
//! - Guest code can panic (unimplemented hardware); the entry points that run it
//!   catch the unwind rather than let it cross the C boundary as undefined behaviour.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::{null, null_mut};

use emulator::{button, Console, Emulator, Input, Load};
use emulator::audio::AudioSource;

/// cbindgen:opaque
/// An opaque emulator handle. Built by [`rewinds_load`], freed by [`rewinds_destroy`].
///
/// The pointer the host holds actually addresses a boxed [`Emulator`]; this marker
/// only gives that pointer a distinct C type (cbindgen forward-declares it). `Emulator`
/// is not `#[repr(C)]`, so it never crosses the boundary by value — the classic opaque
/// handle idiom. The `[u8; 1]` field is inert: the pointer is never dereferenced as a
/// `RewindsEmulator`, only cast back to `Emulator`.
pub struct RewindsEmulator {
    _private: [u8; 1],
}

/// cbindgen:opaque
/// An opaque audio-consumer handle, drained by the host's audio thread. The pointer
/// addresses a boxed [`AudioSource`]; see [`RewindsEmulator`] for the idiom.
pub struct RewindsAudio {
    _private: [u8; 1],
}

/// What to boot. Mirrors [`emulator::Load`] with raw pointers: each `*_len` of `0`
/// (or a null pointer) means "absent". `console` is `-1` to auto-detect, `0` GBA,
/// `1` NDS.
#[repr(C)]
pub struct RewindsLoad {
    pub console: i32,
    pub rom: *const u8,
    pub rom_len: usize,
    pub bios: *const u8,
    pub bios_len: usize,
    pub bios7: *const u8,
    pub bios7_len: usize,
    pub firmware: *const u8,
    pub firmware_len: usize,
}

/// A presentable screen: `width * height` RGBA8 pixels at `rgba` (`len` bytes),
/// borrowed from the emulator and valid only until the next mutating call.
#[repr(C)]
pub struct RewindsScreen {
    pub width: u32,
    pub height: u32,
    pub rgba: *const u8,
    pub len: usize,
}

/// A borrowed byte span handed out by the core (e.g. [`rewinds_save_data`]), valid
/// only until the next mutating call.
#[repr(C)]
pub struct RewindsBytes {
    pub ptr: *const u8,
    pub len: usize,
}

// --- Console codes (also the return of `rewinds_detect_console`) ---------------
/// GBA.
pub const REWINDS_CONSOLE_GBA: i32 = 0;
/// Nintendo DS.
pub const REWINDS_CONSOLE_NDS: i32 = 1;

// --- Load error codes (written to `err_out` by `rewinds_load`) -----------------
/// Success.
pub const REWINDS_OK: i32 = 0;
/// No console given and none detectable from the ROM header.
pub const REWINDS_ERR_UNKNOWN_CONSOLE: i32 = 1;
/// The GBA requires a BIOS image and none was supplied.
pub const REWINDS_ERR_MISSING_BIOS: i32 = 2;
/// The console is recognised but not implemented.
pub const REWINDS_ERR_UNSUPPORTED: i32 = 3;
/// A `.nds` image could not be booted.
pub const REWINDS_ERR_BAD_IMAGE: i32 = 4;
/// A null argument where one was required.
pub const REWINDS_ERR_NULL: i32 = -1;
/// Guest code panicked while booting.
pub const REWINDS_ERR_PANIC: i32 = -2;

// --- Button bits for `rewinds_set_input` -------------------------------------
// Literal values (not `button::A`, which cbindgen cannot evaluate across crates)
// so they land in the generated header; `assert_button_bits` below fails the build
// if they ever drift from the facade's canonical `emulator::button` order.
pub const REWINDS_BTN_A: u32 = 1 << 0;
pub const REWINDS_BTN_B: u32 = 1 << 1;
pub const REWINDS_BTN_SELECT: u32 = 1 << 2;
pub const REWINDS_BTN_START: u32 = 1 << 3;
pub const REWINDS_BTN_RIGHT: u32 = 1 << 4;
pub const REWINDS_BTN_LEFT: u32 = 1 << 5;
pub const REWINDS_BTN_UP: u32 = 1 << 6;
pub const REWINDS_BTN_DOWN: u32 = 1 << 7;
pub const REWINDS_BTN_R: u32 = 1 << 8;
pub const REWINDS_BTN_L: u32 = 1 << 9;
pub const REWINDS_BTN_X: u32 = 1 << 10;
pub const REWINDS_BTN_Y: u32 = 1 << 11;

const _: () = assert_button_bits();
// The asserts are constant by design — this is a compile-time guard, not a runtime check.
#[allow(clippy::assertions_on_constants)]
const fn assert_button_bits() {
    assert!(REWINDS_BTN_A == button::A);
    assert!(REWINDS_BTN_B == button::B);
    assert!(REWINDS_BTN_SELECT == button::SELECT);
    assert!(REWINDS_BTN_START == button::START);
    assert!(REWINDS_BTN_RIGHT == button::RIGHT);
    assert!(REWINDS_BTN_LEFT == button::LEFT);
    assert!(REWINDS_BTN_UP == button::UP);
    assert!(REWINDS_BTN_DOWN == button::DOWN);
    assert!(REWINDS_BTN_R == button::R);
    assert!(REWINDS_BTN_L == button::L);
    assert!(REWINDS_BTN_X == button::X);
    assert!(REWINDS_BTN_Y == button::Y);
}

/// Borrow a byte slice from a host pointer, treating null/empty as "absent".
///
/// # Safety
/// `ptr` must be null or point to at least `len` readable bytes that outlive the call.
unsafe fn borrow<'a>(ptr: *const u8, len: usize) -> Option<&'a [u8]> {
    if ptr.is_null() || len == 0 {
        None
    } else {
        Some(std::slice::from_raw_parts(ptr, len))
    }
}

/// Borrow the emulator behind a handle, or `None` for a null handle.
///
/// # Safety
/// `p` must be null or a live handle from [`rewinds_load`], driven from one thread.
unsafe fn emu<'a>(p: *mut RewindsEmulator) -> Option<&'a mut Emulator> {
    (p as *mut Emulator).as_mut()
}

/// Borrow the audio consumer behind a handle, or `None` for a null handle.
///
/// # Safety
/// `a` must be null or a live handle from [`rewinds_enable_audio`].
unsafe fn audio<'a>(a: *mut RewindsAudio) -> Option<&'a mut AudioSource> {
    (a as *mut AudioSource).as_mut()
}

/// Detect the console from a ROM header: [`REWINDS_CONSOLE_GBA`],
/// [`REWINDS_CONSOLE_NDS`], or `-1` if neither matches (the host can still force one).
///
/// # Safety
/// `rom`/`len` must describe a readable buffer that outlives the call.
#[no_mangle]
pub unsafe extern "C" fn rewinds_detect_console(rom: *const u8, len: usize) -> i32 {
    match borrow(rom, len).and_then(Console::detect) {
        Some(Console::Gba) => REWINDS_CONSOLE_GBA,
        Some(Console::Nds) => REWINDS_CONSOLE_NDS,
        None => -1,
    }
}

/// Build a machine. Returns an owning handle (free with [`rewinds_destroy`]) or null
/// on failure, writing a `REWINDS_ERR_*` / `REWINDS_OK` code to `err_out` when non-null.
///
/// # Safety
/// `cfg` must point to a valid [`RewindsLoad`] whose buffers outlive the call.
#[no_mangle]
pub unsafe extern "C" fn rewinds_load(
    cfg: *const RewindsLoad,
    err_out: *mut i32,
) -> *mut RewindsEmulator {
    let set_err = |code: i32| {
        if !err_out.is_null() {
            *err_out = code;
        }
    };
    let Some(cfg) = cfg.as_ref() else {
        set_err(REWINDS_ERR_NULL);
        return null_mut();
    };
    let console = match cfg.console {
        REWINDS_CONSOLE_GBA => Some(Console::Gba),
        REWINDS_CONSOLE_NDS => Some(Console::Nds),
        _ => None,
    };
    let load = Load {
        console,
        rom: borrow(cfg.rom, cfg.rom_len),
        bios: borrow(cfg.bios, cfg.bios_len),
        bios7: borrow(cfg.bios7, cfg.bios7_len),
        firmware: borrow(cfg.firmware, cfg.firmware_len),
    };
    match catch_unwind(AssertUnwindSafe(|| Emulator::load(load))) {
        Ok(Ok(inner)) => {
            set_err(REWINDS_OK);
            Box::into_raw(Box::new(inner)) as *mut RewindsEmulator
        }
        Ok(Err(e)) => {
            set_err(load_error_code(&e));
            null_mut()
        }
        Err(_) => {
            set_err(REWINDS_ERR_PANIC);
            null_mut()
        }
    }
}

fn load_error_code(e: &emulator::LoadError) -> i32 {
    use emulator::LoadError::*;
    match e {
        UnknownConsole => REWINDS_ERR_UNKNOWN_CONSOLE,
        MissingBios => REWINDS_ERR_MISSING_BIOS,
        Unsupported(_) => REWINDS_ERR_UNSUPPORTED,
        BadImage(_) => REWINDS_ERR_BAD_IMAGE,
    }
}

/// Free a handle from [`rewinds_load`]. Null is a no-op.
///
/// # Safety
/// `p` must be null or a handle not used again after this call.
#[no_mangle]
pub unsafe extern "C" fn rewinds_destroy(p: *mut RewindsEmulator) {
    if !p.is_null() {
        drop(Box::from_raw(p as *mut Emulator));
    }
}

/// Which console this handle holds ([`REWINDS_CONSOLE_GBA`] / `_NDS`), or `-1` if null.
///
/// # Safety
/// `p` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn rewinds_console(p: *mut RewindsEmulator) -> i32 {
    match emu(p).map(|e| e.console()) {
        Some(Console::Gba) => REWINDS_CONSOLE_GBA,
        Some(Console::Nds) => REWINDS_CONSOLE_NDS,
        None => -1,
    }
}

/// The number of video frames presented so far (0 if null).
///
/// # Safety
/// `p` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn rewinds_frame(p: *mut RewindsEmulator) -> u64 {
    emu(p).map(|e| e.frame()).unwrap_or(0)
}

/// Advance one video frame, presenting the result and feeding audio. A panic in guest
/// code is caught and turns this call into a no-op rather than crossing the boundary.
///
/// # Safety
/// `p` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn rewinds_run_frame(p: *mut RewindsEmulator) {
    if let Some(e) = emu(p) {
        let _ = catch_unwind(AssertUnwindSafe(|| e.run_frame()));
    }
}

/// Re-derive the present buffers from the current framebuffer without advancing
/// (for a repaint after a paused/stepped run).
///
/// # Safety
/// `p` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn rewinds_present(p: *mut RewindsEmulator) {
    if let Some(e) = emu(p) {
        e.present();
    }
}

/// How many screens this console presents (GBA: 1, NDS: 2; 0 if null).
///
/// # Safety
/// `p` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn rewinds_screen_count(p: *mut RewindsEmulator) -> u32 {
    emu(p).map(|e| e.screen_count() as u32).unwrap_or(0)
}

/// Fill `out` with the presentable screen at `index` (0 = top). Returns `false` for a
/// null handle, out-of-range index, or null `out`. The `rgba` pointer borrows the
/// emulator — copy it before the next mutating call.
///
/// # Safety
/// `p` must be null or a live handle; `out` must be null or point to a `RewindsScreen`.
#[no_mangle]
pub unsafe extern "C" fn rewinds_screen(
    p: *mut RewindsEmulator,
    index: u32,
    out: *mut RewindsScreen,
) -> bool {
    let (Some(e), false) = (emu(p), out.is_null()) else {
        return false;
    };
    match e.screen(index as usize) {
        Some(s) => {
            *out = RewindsScreen {
                width: s.width,
                height: s.height,
                rgba: s.rgba.as_ptr(),
                len: s.rgba.len(),
            };
            true
        }
        None => false,
    }
}

/// Set the input applied on subsequent frames. `buttons` is a mask of `REWINDS_BTN_*`;
/// touch (DS only) is `touch_x`/`touch_y` in bottom-screen pixels with `touch_pressed`.
///
/// # Safety
/// `p` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn rewinds_set_input(
    p: *mut RewindsEmulator,
    buttons: u32,
    touch_x: i16,
    touch_y: i16,
    touch_pressed: bool,
) {
    if let Some(e) = emu(p) {
        e.set_input(Input {
            buttons,
            touch_x,
            touch_y,
            touch_pressed,
        });
    }
}

/// Open or close the DS clamshell lid (`closed != 0` shuts it). No-op on the GBA. The
/// host drives this on lifecycle transitions (background/foreground) so the game sleeps.
///
/// # Safety
/// `p` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn rewinds_set_lid(p: *mut RewindsEmulator, closed: bool) {
    if let Some(e) = emu(p) {
        e.set_lid(closed);
    }
}

/// Mute audio capture (used for fast-forward): frames still run but drop their samples.
///
/// # Safety
/// `p` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn rewinds_set_audio_muted(p: *mut RewindsEmulator, muted: bool) {
    if let Some(e) = emu(p) {
        e.set_audio_muted(muted);
    }
}

/// Attach a host audio output at `rate` Hz with `channels` channels, returning a
/// consumer handle the host drains ([`rewinds_audio_read`]) on its audio thread. Null
/// on a null emulator. Replaces any previous attachment.
///
/// # Safety
/// `p` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn rewinds_enable_audio(
    p: *mut RewindsEmulator,
    rate: u32,
    channels: usize,
) -> *mut RewindsAudio {
    match emu(p) {
        Some(e) => Box::into_raw(Box::new(e.enable_audio(rate, channels))) as *mut RewindsAudio,
        None => null_mut(),
    }
}

/// Drain up to `capacity` interleaved `f32` samples into `out`, returning how many were
/// written (the caller fills the rest with silence on underrun). Lock-free; safe to call
/// from a real-time audio callback.
///
/// # Safety
/// `a` must be null or a live audio handle; `out`/`capacity` must describe a writable buffer.
#[no_mangle]
pub unsafe extern "C" fn rewinds_audio_read(
    a: *mut RewindsAudio,
    out: *mut f32,
    capacity: usize,
) -> usize {
    let (Some(a), false) = (audio(a), out.is_null() || capacity == 0) else {
        return 0;
    };
    a.read(std::slice::from_raw_parts_mut(out, capacity))
}

/// Free an audio handle from [`rewinds_enable_audio`]. Null is a no-op. Safe to call
/// after the emulator is destroyed (the consumer just underruns).
///
/// # Safety
/// `a` must be null or a handle not used again after this call.
#[no_mangle]
pub unsafe extern "C" fn rewinds_audio_destroy(a: *mut RewindsAudio) {
    if !a.is_null() {
        drop(Box::from_raw(a as *mut AudioSource));
    }
}

// --- Battery saves (the host owns the file) ------------------------------------

/// Point `out` at the current battery-save contents (borrowed; copy before the next
/// mutating call). Empty span for a null handle or null `out`.
///
/// # Safety
/// `p` must be null or a live handle; `out` must be null or point to a `RewindsBytes`.
#[no_mangle]
pub unsafe extern "C" fn rewinds_save_data(p: *mut RewindsEmulator, out: *mut RewindsBytes) {
    if out.is_null() {
        return;
    }
    *out = match emu(p) {
        Some(e) => {
            let d = e.save_data();
            RewindsBytes {
                ptr: d.as_ptr(),
                len: d.len(),
            }
        }
        None => RewindsBytes { ptr: null(), len: 0 },
    };
}

/// Restore previously saved contents (copied in).
///
/// # Safety
/// `p` must be null or a live handle; `data`/`len` a readable buffer that outlives the call.
#[no_mangle]
pub unsafe extern "C" fn rewinds_load_save_data(
    p: *mut RewindsEmulator,
    data: *const u8,
    len: usize,
) {
    if let (Some(e), Some(bytes)) = (emu(p), borrow(data, len)) {
        e.load_save_data(bytes);
    }
}

/// Whether the save changed since the last [`rewinds_clear_save_dirty`] — the host
/// flushes only when this is `true`.
///
/// # Safety
/// `p` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn rewinds_save_dirty(p: *mut RewindsEmulator) -> bool {
    emu(p).map(|e| e.save_dirty()).unwrap_or(false)
}

/// Acknowledge that the host persisted the current save.
///
/// # Safety
/// `p` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn rewinds_clear_save_dirty(p: *mut RewindsEmulator) {
    if let Some(e) = emu(p) {
        e.clear_save_dirty();
    }
}

// --- Serial link (host owns the carrier; frames are opaque bytes) -----------

/// Configure the serial link: whether a carrier is attached, this unit's id
/// (0 = parent/master, 1-3 = child), and the number of linked units (2-4).
///
/// # Safety
/// `p` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn rewinds_link_set_config(
    p: *mut RewindsEmulator,
    connected: bool,
    id: u8,
    count: u8,
) {
    if let Some(e) = emu(p) {
        e.set_link_config(connected, id, count);
    }
}

/// Take the next serial frame to transmit, writing a borrowed span into `out`
/// (empty when there is nothing to send). The span is valid until the next
/// mutating call; the host copies it before transmitting.
///
/// # Safety
/// `p` must be null or a live handle; `out` must be non-null and writable.
#[no_mangle]
pub unsafe extern "C" fn rewinds_link_poll_out(p: *mut RewindsEmulator, out: *mut RewindsBytes) {
    if out.is_null() {
        return;
    }
    *out = match emu(p).and_then(|e| e.link_poll_out()) {
        Some(d) => RewindsBytes { ptr: d.as_ptr(), len: d.len() },
        None => RewindsBytes { ptr: null(), len: 0 },
    };
}

/// Deliver a peer's serial frame received from the carrier.
///
/// # Safety
/// `p` must be null or a live handle; `bytes`/`len` must describe a readable span
/// that outlives the call.
#[no_mangle]
pub unsafe extern "C" fn rewinds_link_deliver(
    p: *mut RewindsEmulator,
    bytes: *const u8,
    len: usize,
) {
    if let (Some(e), Some(frame)) = (emu(p), borrow(bytes, len)) {
        e.link_deliver(frame);
    }
}

/// Whether a serial transfer is in progress (the host should keep pumping).
///
/// # Safety
/// `p` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn rewinds_link_pending(p: *mut RewindsEmulator) -> bool {
    emu(p).map(|e| e.link_pending()).unwrap_or(false)
}

/// Write the save type's name (e.g. `"flash128k"`) as UTF-8 (no NUL) into `buf`,
/// returning the number of bytes written (truncated to `cap`). Pass a null `buf` /
/// `cap` of 0 to query the length.
///
/// # Safety
/// `p` must be null or a live handle; `buf` must be null or point to `cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn rewinds_save_type_name(
    p: *mut RewindsEmulator,
    buf: *mut u8,
    cap: usize,
) -> usize {
    let Some(e) = emu(p) else {
        return 0;
    };
    let name = e.save_type_name().as_bytes();
    let n = name.len().min(cap);
    if !buf.is_null() && n > 0 {
        std::ptr::copy_nonoverlapping(name.as_ptr(), buf, n);
    }
    n
}

/// Override the save type by name (from a host sidecar). Returns `false` for an unknown
/// name, invalid UTF-8, or a console without configurable save types.
///
/// # Safety
/// `p` must be null or a live handle; `name`/`len` a readable buffer that outlives the call.
#[no_mangle]
pub unsafe extern "C" fn rewinds_set_save_type_by_name(
    p: *mut RewindsEmulator,
    name: *const u8,
    len: usize,
) -> bool {
    let (Some(e), Some(bytes)) = (emu(p), borrow(name, len)) else {
        return false;
    };
    match std::str::from_utf8(bytes) {
        Ok(s) => e.set_save_type_by_name(s),
        Err(_) => false,
    }
}

/// The ABI version of this core surface. Bumped when the `rewinds_*` signatures change,
/// so a host can refuse a mismatched framework.
#[no_mangle]
pub extern "C" fn rewinds_core_version() -> u32 {
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercise the C surface exactly as a host would: build a bare DS, run a frame,
    /// read screen 0's RGBA span, then free — all through raw pointers.
    #[test]
    fn nds_boots_runs_and_presents_through_the_c_surface() {
        let cfg = RewindsLoad {
            console: REWINDS_CONSOLE_NDS,
            rom: null(),
            rom_len: 0,
            bios: null(),
            bios_len: 0,
            bios7: null(),
            bios7_len: 0,
            firmware: null(),
            firmware_len: 0,
        };
        let mut err = -99;
        let emu = unsafe { rewinds_load(&cfg, &mut err) };
        assert_eq!(err, REWINDS_OK);
        assert!(!emu.is_null());
        assert_eq!(unsafe { rewinds_console(emu) }, REWINDS_CONSOLE_NDS);
        assert_eq!(unsafe { rewinds_screen_count(emu) }, 2);

        unsafe { rewinds_run_frame(emu) };
        assert!(unsafe { rewinds_frame(emu) } >= 1);

        let mut screen = RewindsScreen {
            width: 0,
            height: 0,
            rgba: null(),
            len: 0,
        };
        assert!(unsafe { rewinds_screen(emu, 0, &mut screen) });
        assert_eq!(screen.width, 256);
        assert_eq!(screen.height, 192);
        assert_eq!(screen.len as u32, screen.width * screen.height * 4);
        assert!(!screen.rgba.is_null());
        // Screen index past the count is rejected.
        assert!(!unsafe { rewinds_screen(emu, 5, &mut screen) });

        unsafe { rewinds_destroy(emu) };
    }

    /// A GBA load with no BIOS succeeds on the built-in replacement BIOS.
    #[test]
    fn gba_without_bios_uses_the_builtin() {
        let cfg = RewindsLoad {
            console: REWINDS_CONSOLE_GBA,
            rom: null(),
            rom_len: 0,
            bios: null(),
            bios_len: 0,
            bios7: null(),
            bios7_len: 0,
            firmware: null(),
            firmware_len: 0,
        };
        let mut err = REWINDS_ERR_MISSING_BIOS; // sentinel: load should overwrite with OK
        let emu = unsafe { rewinds_load(&cfg, &mut err) };
        assert!(!emu.is_null());
        assert_eq!(err, REWINDS_OK);
        unsafe { rewinds_destroy(emu) };
    }

    /// Null handles are inert everywhere (no panic, sane defaults).
    #[test]
    fn null_handle_is_inert() {
        assert_eq!(unsafe { rewinds_console(null_mut()) }, -1);
        assert_eq!(unsafe { rewinds_frame(null_mut()) }, 0);
        assert_eq!(unsafe { rewinds_screen_count(null_mut()) }, 0);
        assert!(!unsafe { rewinds_save_dirty(null_mut()) });
        unsafe {
            rewinds_run_frame(null_mut());
            rewinds_set_input(null_mut(), REWINDS_BTN_A, 0, 0, false);
            rewinds_set_lid(null_mut(), true);
            rewinds_destroy(null_mut());
            rewinds_audio_destroy(null_mut());
        }
    }
}
