import Foundation
import RewindsKit

/// One system file the emulator may need to boot.
enum SystemFile: String, CaseIterable, Identifiable {
    case gbaBios = "gba_bios.bin"
    case ndsBios9 = "nds_bios9.bin"
    case ndsBios7 = "nds_bios7.bin"
    case ndsFirmware = "nds_firmware.bin"

    var id: String { rawValue }

    var title: String {
        switch self {
        case .gbaBios: return "GBA BIOS (optional override)"
        case .ndsBios9: return "DS ARM9 BIOS"
        case .ndsBios7: return "DS ARM7 BIOS"
        case .ndsFirmware: return "DS Firmware (optional)"
        }
    }

    /// Which console needs it, and whether it's mandatory to boot that console.
    /// The GBA BIOS is optional: without one the built-in replacement (embedded in
    /// the core) is used, so a supplied file only overrides it for fidelity.
    var console: Console { self == .gbaBios ? .gba : .nds }
    var required: Bool { self != .ndsFirmware && self != .gbaBios }
}

/// Resolves BIOS/firmware bytes. Files the user imported at runtime (into app-support)
/// win; otherwise the copy bundled from `dev-assets/` at build time is used. Either way
/// the app never ships these blobs in source control — the developer supplies them.
final class BIOSStore: ObservableObject {
    static let shared = BIOSStore()

    /// Bumps when an import changes availability, so SwiftUI settings refresh.
    @Published private(set) var revision = 0

    private let systemDir: URL

    init() {
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        systemDir = base.appendingPathComponent("System", isDirectory: true)
        try? FileManager.default.createDirectory(at: systemDir, withIntermediateDirectories: true)
    }

    /// The imported copy's path (may not exist).
    private func importedURL(_ file: SystemFile) -> URL {
        systemDir.appendingPathComponent(file.rawValue)
    }

    /// The copy bundled from `dev-assets/`, if the build included one.
    private func bundledURL(_ file: SystemFile) -> URL? {
        let name = (file.rawValue as NSString).deletingPathExtension
        let ext = (file.rawValue as NSString).pathExtension
        return Bundle.main.url(forResource: name, withExtension: ext, subdirectory: "dev-assets")
    }

    /// Bytes for a system file: imported copy first, then the bundled one.
    func data(_ file: SystemFile) -> Data? {
        if let d = try? Data(contentsOf: importedURL(file)) { return d }
        if let url = bundledURL(file), let d = try? Data(contentsOf: url) { return d }
        return nil
    }

    func isAvailable(_ file: SystemFile) -> Bool { data(file) != nil }

    /// Whether the required BIOS for a console is present. The GBA always boots on
    /// the core's built-in replacement BIOS, so it needs no supplied file; the DS
    /// still needs its ARM9/ARM7 images (firmware is optional).
    func hasRequiredBios(for console: Console) -> Bool {
        switch console {
        case .gba: return true
        case .nds: return isAvailable(.ndsBios9) && isAvailable(.ndsBios7)
        }
    }

    /// Assemble the boot images for a ROM. Firmware is intentionally omitted so DS ROMs
    /// direct-boot straight into the game rather than the DS home menu.
    func bootImages(for console: Console, rom: Data) -> BootImages {
        switch console {
        case .gba:
            // A nil BIOS tells the core to use its built-in replacement; a supplied
            // file (imported or bundled) overrides it.
            return BootImages(rom: rom, bios: data(.gbaBios), bios7: nil, firmware: nil)
        case .nds:
            return BootImages(rom: rom, bios: data(.ndsBios9), bios7: data(.ndsBios7), firmware: nil)
        }
    }

    /// Import a user-picked file as `file`, copying it into app-support (overwriting any
    /// previous import). `url` is a security-scoped Files URL.
    func importFile(from url: URL, as file: SystemFile) throws {
        let needsStop = url.startAccessingSecurityScopedResource()
        defer { if needsStop { url.stopAccessingSecurityScopedResource() } }
        let data = try Data(contentsOf: url)
        try data.write(to: importedURL(file), options: .atomic)
        revision += 1
    }

    /// Forget an imported copy (falls back to the bundled one, if any).
    func removeImport(_ file: SystemFile) {
        try? FileManager.default.removeItem(at: importedURL(file))
        revision += 1
    }

    /// Whether an imported (not merely bundled) copy exists — the Settings UI shows this
    /// so the user can tell what they've added vs. what the build baked in.
    func hasImport(_ file: SystemFile) -> Bool {
        FileManager.default.fileExists(atPath: importedURL(file).path)
    }
}
