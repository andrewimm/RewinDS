import Foundation

/// One launchable ROM in the library.
public struct GameEntry: Identifiable, Hashable {
    public let id: String            // stable per-ROM id (also the save filename)
    public let name: String          // display name
    public let url: URL
    public let console: Console?      // detected from extension; confirmed at load
    public let isBundled: Bool        // shipped in dev-assets vs. imported by the user
}

/// Enumerates launchable ROMs: the ones bundled from `dev-assets/` plus any the user
/// imported through Files (kept under Documents/ROMs).
public final class GameLibrary: ObservableObject {
    public static let shared = GameLibrary()

    @Published public private(set) var games: [GameEntry] = []

    private let romsDir: URL
    private static let romExtensions = ["gba", "nds"]

    init() {
        let docs = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        romsDir = docs.appendingPathComponent("ROMs", isDirectory: true)
        try? FileManager.default.createDirectory(at: romsDir, withIntermediateDirectories: true)
        reload()
    }

    public func reload() {
        var entries: [GameEntry] = []

        // Bundled dev-assets ROMs.
        if let bundled = Bundle.main.resourceURL?.appendingPathComponent("dev-assets"),
           let items = try? FileManager.default.contentsOfDirectory(
               at: bundled, includingPropertiesForKeys: nil) {
            for url in items where Self.romExtensions.contains(url.pathExtension.lowercased()) {
                entries.append(makeEntry(url: url, isBundled: true))
            }
        }

        // Imported ROMs.
        if let items = try? FileManager.default.contentsOfDirectory(
            at: romsDir, includingPropertiesForKeys: nil) {
            for url in items where Self.romExtensions.contains(url.pathExtension.lowercased()) {
                entries.append(makeEntry(url: url, isBundled: false))
            }
        }

        games = entries.sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
    }

    private func makeEntry(url: URL, isBundled: Bool) -> GameEntry {
        let name = url.deletingPathExtension().lastPathComponent
        let console: Console? = url.pathExtension.lowercased() == "nds" ? .nds : .gba
        return GameEntry(id: name, name: name, url: url, console: console, isBundled: isBundled)
    }

    /// Import a user-picked ROM into the library, returning its new entry.
    @discardableResult
    public func importROM(from url: URL) throws -> GameEntry {
        let needsStop = url.startAccessingSecurityScopedResource()
        defer { if needsStop { url.stopAccessingSecurityScopedResource() } }
        let dest = romsDir.appendingPathComponent(url.lastPathComponent)
        if FileManager.default.fileExists(atPath: dest.path) {
            try? FileManager.default.removeItem(at: dest)
        }
        try FileManager.default.copyItem(at: url, to: dest)
        reload()
        return makeEntry(url: dest, isBundled: false)
    }

    public func delete(_ entry: GameEntry) {
        guard !entry.isBundled else { return }
        try? FileManager.default.removeItem(at: entry.url)
        reload()
    }
}
