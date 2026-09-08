import SwiftUI
import RewindsKit

/// Top-level navigation + game-loading orchestrator.
@MainActor
final class AppModel: ObservableObject {
    enum Route {
        case library
        case game(EmulatorSession)
    }

    @Published var route: Route = .library
    @Published var alert: AlertItem?

    let library = GameLibrary.shared
    let bios = BIOSStore.shared

    struct AlertItem: Identifiable {
        let id = UUID()
        let title: String
        let message: String
    }

    /// Load and start a library entry.
    func launch(_ entry: GameEntry) {
        do {
            let data = try readROM(at: entry.url, bundled: entry.isBundled)
            // The file extension is authoritative; header sniffing is only a fallback.
            try launch(romData: data, romId: entry.id, consoleHint: entry.console)
        } catch {
            present("Couldn't open “\(entry.name)”", (error as? LoadError)?.description ?? error.localizedDescription)
        }
    }

    /// Load raw ROM bytes (already read) under a stable id. `consoleHint` (from the file
    /// extension) wins over header detection, which mis-sniffs some homebrew.
    func launch(romData data: Data, romId: String, consoleHint: Console? = nil) throws {
        guard let console = consoleHint ?? EmulatorCore.detectConsole(rom: data) else {
            throw LoadError.unknownConsole
        }
        guard bios.hasRequiredBios(for: console) else {
            present(
                "\(console.displayName) BIOS needed",
                "Add the \(console == .gba ? "GBA BIOS" : "DS ARM9 and ARM7 BIOS") under Settings → System files, then try again.")
            return
        }
        let images = bios.bootImages(for: console, rom: data)
        // Build the new machine before touching the current one, so a failed switch
        // leaves the running game untouched.
        let core = try EmulatorCore(images: images, forceConsole: console)
        let session = EmulatorSession(core: core, romId: romId)
        if case let .game(old) = route { old.teardown() }
        session.start()
        route = .game(session)
    }

    /// Dev hook: if `REWINDS_AUTOLAUNCH=<game name>` is set in the environment, launch
    /// that library entry on startup. Used to drive the emulator from a test harness; a
    /// no-op in normal use (the variable is never set).
    private var didAutolaunch = false
    func autolaunchIfRequested() {
        guard !didAutolaunch, case .library = route,
              let name = ProcessInfo.processInfo.environment["REWINDS_AUTOLAUNCH"],
              let game = library.games.first(where: { $0.name == name })
        else { return }
        didAutolaunch = true
        launch(game)
    }

    /// Leave the running game and return to the library.
    func exitGame() {
        if case let .game(session) = route {
            session.teardown()
        }
        route = .library
        library.reload()
    }

    // --- Lifecycle relays -----------------------------------------------------

    func handleScenePhase(_ phase: ScenePhase) {
        guard case let .game(session) = route else { return }
        switch phase {
        case .active: session.enterForeground()   // focused: resume + wake lid
        case .inactive: session.pause()            // lost focus: freeze, keep state
        case .background: session.enterBackground() // gone: freeze + sleep lid + flush save
        @unknown default: session.pause()
        }
    }

    // --- Helpers --------------------------------------------------------------

    private func readROM(at url: URL, bundled: Bool) throws -> Data {
        if bundled { return try Data(contentsOf: url) }
        let needsStop = url.startAccessingSecurityScopedResource()
        defer { if needsStop { url.stopAccessingSecurityScopedResource() } }
        return try Data(contentsOf: url)
    }

    private func present(_ title: String, _ message: String) {
        alert = AlertItem(title: title, message: message)
    }
}
