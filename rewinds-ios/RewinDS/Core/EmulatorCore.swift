import Foundation
import RewindsCore

/// Which console a handle holds.
enum Console: Int32 {
    case gba = 0  // REWINDS_CONSOLE_GBA
    case nds = 1  // REWINDS_CONSOLE_NDS

    /// The screens this console presents, top-first.
    var screenCount: Int { self == .nds ? 2 : 1 }
    /// Native pixel size of each screen.
    var screenSize: (width: Int, height: Int) { self == .nds ? (256, 192) : (240, 160) }
    var displayName: String { self == .nds ? "Nintendo DS" : "Game Boy Advance" }
}

/// Why a load failed, mapped from the core's `REWINDS_ERR_*` codes.
enum LoadError: Int32, Error, CustomStringConvertible {
    case unknownConsole = 1
    case missingBios = 2
    case unsupported = 3
    case badImage = 4
    case nullArgument = -1
    case panicked = -2

    var description: String {
        switch self {
        case .unknownConsole: return "Couldn't tell which console this ROM is for."
        case .missingBios: return "This console needs its BIOS. Add it under Settings → System files."
        case .unsupported: return "That console isn't supported yet."
        case .badImage: return "This ROM couldn't be booted — it may be corrupt or an unsupported format."
        case .nullArgument: return "Internal error: missing ROM data."
        case .panicked: return "The emulator core crashed while booting this ROM."
        }
    }
}

/// The images needed to boot. The core copies what it keeps, so these buffers only
/// need to live across the `load` call.
struct BootImages {
    var rom: Data?
    var bios: Data?
    var bios7: Data?
    var firmware: Data?
}

/// A borrowed view of one presented screen: `count` bytes of RGBA8 at `pixels`, valid
/// only until the next `runFrame`/`present`.
struct ScreenBuffer {
    let width: Int
    let height: Int
    let pixels: UnsafePointer<UInt8>
    let count: Int
}

/// A thin, safe Swift wrapper over the `rewinds_*` C surface. Owns one emulator handle
/// and must be driven from a single thread (the run loop on the main thread here).
final class EmulatorCore {
    private let handle: OpaquePointer
    let console: Console

    /// Build a machine, or throw a `LoadError`. `forceConsole` overrides header detection.
    init(images: BootImages, forceConsole: Console? = nil) throws {
        // Pin every buffer for the duration of the load so the raw pointers stay valid.
        // C `uintptr_t` maps to Swift `UInt`, so pass lengths as `UInt` throughout.
        func withOptional<R>(_ data: Data?, _ body: (UnsafePointer<UInt8>?, UInt) -> R) -> R {
            guard let data, !data.isEmpty else { return body(nil, 0) }
            return data.withUnsafeBytes { body($0.bindMemory(to: UInt8.self).baseAddress, UInt(data.count)) }
        }

        var err: Int32 = 0
        let handle: OpaquePointer? = withOptional(images.rom) { romPtr, romLen in
            withOptional(images.bios) { biosPtr, biosLen in
                withOptional(images.bios7) { bios7Ptr, bios7Len in
                    withOptional(images.firmware) { fwPtr, fwLen in
                        var cfg = RewindsLoad(
                            console: forceConsole?.rawValue ?? -1,
                            rom: romPtr, rom_len: romLen,
                            bios: biosPtr, bios_len: biosLen,
                            bios7: bios7Ptr, bios7_len: bios7Len,
                            firmware: fwPtr, firmware_len: fwLen
                        )
                        return rewinds_load(&cfg, &err)
                    }
                }
            }
        }

        guard let handle else {
            throw LoadError(rawValue: err) ?? .badImage
        }
        self.handle = handle
        self.console = Console(rawValue: rewinds_console(handle)) ?? forceConsole ?? .gba
    }

    deinit {
        rewinds_destroy(handle)
    }

    /// Detect a console from a ROM's header without loading it.
    static func detectConsole(rom: Data) -> Console? {
        let code = rom.withUnsafeBytes { raw -> Int32 in
            rewinds_detect_console(raw.bindMemory(to: UInt8.self).baseAddress, UInt(rom.count))
        }
        return Console(rawValue: code)
    }

    var frame: UInt64 { rewinds_frame(handle) }
    var screenCount: Int { Int(rewinds_screen_count(handle)) }

    func runFrame() { rewinds_run_frame(handle) }
    func present() { rewinds_present(handle) }

    /// The button mask (`REWINDS_BTN_*`) and touch applied on subsequent frames.
    func setInput(buttons: UInt32, touchX: Int16, touchY: Int16, touchPressed: Bool) {
        rewinds_set_input(handle, buttons, touchX, touchY, touchPressed)
    }

    /// Open/close the DS lid (game sleeps/wakes). No-op on GBA.
    func setLid(closed: Bool) { rewinds_set_lid(handle, closed) }

    func setAudioMuted(_ muted: Bool) { rewinds_set_audio_muted(handle, muted) }

    /// Read one screen. The returned pointer borrows the core — copy it before the next
    /// `runFrame`. Runs `body` with the borrow so the lifetime is scoped.
    func withScreen<R>(_ index: Int, _ body: (ScreenBuffer) -> R) -> R? {
        var s = RewindsScreen(width: 0, height: 0, rgba: nil, len: 0)
        guard rewinds_screen(handle, UInt32(index), &s), let rgba = s.rgba else { return nil }
        return body(ScreenBuffer(width: Int(s.width), height: Int(s.height), pixels: rgba, count: Int(s.len)))
    }

    // --- Battery saves --------------------------------------------------------

    /// A copy of the current battery-save contents.
    func saveData() -> Data {
        var b = RewindsBytes(ptr: nil, len: 0)
        rewinds_save_data(handle, &b)
        guard let ptr = b.ptr, b.len > 0 else { return Data() }
        return Data(bytes: ptr, count: Int(b.len))
    }

    func loadSaveData(_ data: Data) {
        guard !data.isEmpty else { return }
        data.withUnsafeBytes { raw in
            rewinds_load_save_data(handle, raw.bindMemory(to: UInt8.self).baseAddress, UInt(data.count))
        }
    }

    var saveDirty: Bool { rewinds_save_dirty(handle) }
    func clearSaveDirty() { rewinds_clear_save_dirty(handle) }

    /// The cartridge save type's name (e.g. "FLASH1M"), or "none" for a console without
    /// configurable save types (the DS).
    func saveTypeName() -> String {
        var buf = [UInt8](repeating: 0, count: 32)
        let n = buf.withUnsafeMutableBufferPointer {
            rewinds_save_type_name(handle, $0.baseAddress, UInt($0.count))
        }
        return String(decoding: buf.prefix(Int(n)), as: UTF8.self)
    }

    /// Override the save type by name (from a host sidecar). Returns whether it was
    /// accepted (an unknown name or a DS handle returns false).
    @discardableResult
    func setSaveType(name: String) -> Bool {
        let bytes = Array(name.utf8)
        return bytes.withUnsafeBufferPointer {
            rewinds_set_save_type_by_name(handle, $0.baseAddress, UInt($0.count))
        }
    }

    // --- Serial link ----------------------------------------------------------

    /// The next opaque serial frame to transmit, or `nil` if nothing is queued. The core
    /// hands out a borrowed span (valid only until the next mutating call), so we copy it
    /// immediately. The host relays these bytes to a peer without interpreting them.
    func linkPollOut() -> Data? {
        var b = RewindsBytes(ptr: nil, len: 0)
        rewinds_link_poll_out(handle, &b)
        guard let ptr = b.ptr, b.len > 0 else { return nil }
        return Data(bytes: ptr, count: Int(b.len))
    }

    /// Deliver a peer's opaque serial frame received from the carrier.
    func linkDeliver(_ frame: Data) {
        guard !frame.isEmpty else { return }
        frame.withUnsafeBytes { raw in
            rewinds_link_deliver(handle, raw.bindMemory(to: UInt8.self).baseAddress, UInt(frame.count))
        }
    }

    /// Whether a serial transfer is mid-flight (the host should keep pumping).
    var linkPending: Bool { rewinds_link_pending(handle) }

    /// Attach/detach a carrier and set this unit's id (0 = parent, 1–3 = child) and the
    /// number of linked units.
    func setLinkConfig(connected: Bool, id: UInt8, count: UInt8) {
        rewinds_link_set_config(handle, connected, id, count)
    }

    // --- Audio ----------------------------------------------------------------

    /// Attach an audio output at `rate` Hz / `channels` channels; returns a consumer
    /// handle the audio thread drains. Nil if the core rejected it.
    func enableAudio(rate: UInt32, channels: Int) -> AudioConsumer? {
        guard let src = rewinds_enable_audio(handle, rate, UInt(channels)) else { return nil }
        return AudioConsumer(handle: src)
    }

    /// The core's ABI version (for a mismatch check against the header the app built with).
    static var coreVersion: UInt32 { rewinds_core_version() }
}

/// An owning wrapper over a `RewindsAudio` consumer. Lock-free; `read` is safe to call
/// from a real-time audio callback. Independent of the emulator's lifetime.
final class AudioConsumer {
    private let handle: OpaquePointer
    init(handle: OpaquePointer) { self.handle = handle }
    deinit { rewinds_audio_destroy(handle) }

    /// Drain up to `capacity` interleaved floats into `out`; returns how many were written.
    func read(into out: UnsafeMutablePointer<Float>, capacity: Int) -> Int {
        Int(rewinds_audio_read(handle, out, UInt(capacity)))
    }
}
