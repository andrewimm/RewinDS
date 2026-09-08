import SwiftUI
import UniformTypeIdentifiers
import RewindsKit

/// Quits the app when its last (only) window closes — this is a single-window app, not a
/// document app, so there's nothing to keep it running in the background.
final class MacAppDelegate: NSObject, NSApplicationDelegate {
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
}

@main
struct RewinDSMacApp: App {
    @NSApplicationDelegateAdaptor(MacAppDelegate.self) private var appDelegate
    @StateObject private var model = MacAppModel()

    init() {
        LinkNotifier.configure()
    }

    var body: some Scene {
        WindowGroup {
            MacRootView()
                .environmentObject(model)
                .frame(minWidth: 480, minHeight: 360)
        }
        .commands {
            CommandGroup(after: .newItem) {
                Button("Open ROM…") { model.openROM() }
                    .keyboardShortcut("o", modifiers: .command)
                if model.session != nil {
                    Button("Close Game") { model.quitGame() }
                        .keyboardShortcut("w", modifiers: .command)
                }
            }
        }
    }
}

/// Loads and runs one game on macOS. GBA boots with the built-in BIOS (the core falls back
/// to it when none is supplied); DS ROMs additionally need BIOS/firmware, which this dev
/// build doesn't manage yet.
@MainActor
final class MacAppModel: ObservableObject {
    @Published var session: EmulatorSession?
    @Published var errorMessage: String?

    private static let romTypes: [UTType] = ["gba", "nds"].compactMap { UTType(filenameExtension: $0) }

    func openROM() {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = false
        panel.canChooseDirectories = false
        panel.allowedContentTypes = Self.romTypes
        panel.allowsOtherFileTypes = true
        if panel.runModal() == .OK, let url = panel.url { load(url) }
    }

    func load(_ url: URL) {
        do {
            let data = try Data(contentsOf: url)
            let ext = url.pathExtension.lowercased()
            let console: Console? = ext == "nds" ? .nds : (ext == "gba" ? .gba : nil)
            let core = try EmulatorCore(images: BootImages(rom: data), forceConsole: console)
            let newSession = EmulatorSession(core: core, romId: url.deletingPathExtension().lastPathComponent)
            session?.teardown()
            newSession.start()
            session = newSession
            errorMessage = nil
        } catch {
            errorMessage = (error as? LoadError)?.description ?? error.localizedDescription
        }
    }

    func quitGame() {
        session?.teardown()
        session = nil
    }
}

struct MacRootView: View {
    @EnvironmentObject private var model: MacAppModel

    var body: some View {
        Group {
            if let session = model.session {
                MacGameView(session: session)
            } else {
                MacLibraryView()
            }
        }
        .alert("Couldn't open ROM",
               isPresented: Binding(get: { model.errorMessage != nil },
                                    set: { if !$0 { model.errorMessage = nil } })) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(model.errorMessage ?? "")
        }
    }
}

struct MacLibraryView: View {
    @EnvironmentObject private var model: MacAppModel

    var body: some View {
        VStack(spacing: 20) {
            Image(systemName: "gamecontroller")
                .font(.system(size: 52))
                .foregroundStyle(.secondary)
            Text("RewinDS")
                .font(.largeTitle.weight(.semibold))
            Text("Open a GBA ROM to play, then link with another device from the toolbar.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 360)
            Button {
                model.openROM()
            } label: {
                Label("Open ROM…", systemImage: "folder")
                    .padding(.horizontal, 8)
            }
            .controlSize(.large)
            .keyboardShortcut("o", modifiers: .command)
        }
        .padding(40)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}
