import Foundation

/// Battery-save persistence. The core owns the live save RAM; this writes it out to a
/// `.sav` next to a per-ROM id and restores it on load. The host owns the file — the
/// core only ever exchanges bytes.
final class SaveStore {
    static let shared = SaveStore()

    private let savesDir: URL

    init() {
        let docs = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        savesDir = docs.appendingPathComponent("Saves", isDirectory: true)
        try? FileManager.default.createDirectory(at: savesDir, withIntermediateDirectories: true)
    }

    private func url(for romId: String) -> URL {
        savesDir.appendingPathComponent(romId).appendingPathExtension("sav")
    }

    /// The `<rom>.sav.meta` sidecar recording the GBA save type, interchangeable with the
    /// desktop host's.
    private func metaURL(for romId: String) -> URL {
        url(for: romId).appendingPathExtension("meta")
    }

    /// Restore a previously written save into the core, if one exists.
    func restore(into core: EmulatorCore, romId: String) {
        // Apply the save-type hint first so the raw chip dump is interpreted correctly.
        // Entirely optional: a missing or unreadable sidecar just leaves the core's own
        // ROM-based auto-detection in place.
        if let text = try? String(contentsOf: metaURL(for: romId), encoding: .utf8),
           let type = Self.parseSaveType(text) {
            core.setSaveType(name: type)
        }
        guard let data = try? Data(contentsOf: url(for: romId)), !data.isEmpty else { return }
        core.loadSaveData(data)
    }

    /// Flush the core's save to disk if it has changed since the last flush. Returns
    /// whether anything was written.
    @discardableResult
    func flushIfDirty(_ core: EmulatorCore, romId: String) -> Bool {
        guard core.saveDirty else { return false }
        let data = core.saveData()
        guard !data.isEmpty else { core.clearSaveDirty(); return false }
        do {
            try data.write(to: url(for: romId), options: .atomic)
            writeMeta(core, romId: romId) // best-effort; never fails the actual save
            core.clearSaveDirty()
            return true
        } catch {
            return false
        }
    }

    /// Write the `.sav.meta` sidecar with the current save type. Skipped for consoles
    /// without configurable save types (the DS reports "none").
    private func writeMeta(_ core: EmulatorCore, romId: String) {
        let type = core.saveTypeName()
        guard !type.isEmpty, type != "none" else { return }
        let json = "{\n  \"save_type\": \"\(type)\"\n}\n"
        try? json.write(to: metaURL(for: romId), atomically: true, encoding: .utf8)
    }

    /// Pull the `save_type` value out of a `.sav.meta`. Deliberately lenient — find the
    /// key, then the next quoted string — so odd formatting still survives; returns nil
    /// (→ fall back to auto-detection) if it can't.
    private static func parseSaveType(_ text: String) -> String? {
        guard let key = text.range(of: "\"save_type\"") else { return nil }
        let afterKey = text[key.upperBound...]
        guard let open = afterKey.firstIndex(of: "\"") else { return nil }
        let value = afterKey[afterKey.index(after: open)...]
        guard let close = value.firstIndex(of: "\"") else { return nil }
        let name = String(value[..<close])
        return name.isEmpty ? nil : name
    }
}
