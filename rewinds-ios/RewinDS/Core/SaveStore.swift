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

    /// Restore a previously written save into the core, if one exists.
    func restore(into core: EmulatorCore, romId: String) {
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
            core.clearSaveDirty()
            return true
        } catch {
            return false
        }
    }
}
