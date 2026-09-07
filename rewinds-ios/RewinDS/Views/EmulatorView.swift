import SwiftUI

/// Hosts a running session: chooses the console's shell layout, owns the control
/// registry, and turns the toolbar actions into core operations plus brief feedback.
struct EmulatorView: View {
    @EnvironmentObject private var model: AppModel
    @ObservedObject var session: EmulatorSession

    // A reference type held stably for the life of this view.
    @State private var registry = ControlRegistry()
    @State private var toast: String?

    // Dev HUD: show the live frame counter when launched from the test harness.
    @State private var hudFrame: UInt64 = 0
    private var showHUD: Bool { ProcessInfo.processInfo.environment["REWINDS_AUTOLAUNCH"] != nil }
    private let hudTimer = Timer.publish(every: 0.5, on: .main, in: .common).autoconnect()

    var body: some View {
        let shell = Shell.of(session.console)
        ZStack {
            switch session.console {
            case .gba:
                GBALayout(
                    session: session, shell: shell, registry: registry,
                    onSave: save, onMenu: exit)
            case .nds:
                DSLayout(
                    session: session, shell: shell, registry: registry,
                    onSave: save, onRewind: rewind, onMenu: exit)
            }
        }
        .overlay(alignment: .bottomLeading) {
            if showHUD {
                Text("frame \(hudFrame)")
                    .font(.system(size: 12, weight: .bold, design: .monospaced))
                    .padding(6)
                    .background(.black.opacity(0.6), in: RoundedRectangle(cornerRadius: 6))
                    .foregroundStyle(.green)
                    .padding(8)
                    .onReceive(hudTimer) { _ in hudFrame = session.currentFrame() }
            }
        }
        .overlay(alignment: .top) {
            if let toast {
                Text(toast)
                    .font(.system(size: 13, weight: .semibold))
                    .padding(.horizontal, 16).padding(.vertical, 9)
                    .background(.ultraThinMaterial, in: Capsule())
                    .foregroundStyle(.white)
                    .padding(.top, 12)
                    .transition(.move(edge: .top).combined(with: .opacity))
            }
        }
    }

    private func save() {
        session.saveNow()
        flash("Saved")
    }

    private func exit() {
        model.exitGame()
    }

    private func rewind() {
        // The rewind/time-travel feature lives in the debug crate and isn't on the
        // emulator facade yet, so there's nothing to drive here. Say so honestly.
        flash("Rewind isn't wired to the core yet")
    }

    private func flash(_ message: String) {
        withAnimation(.spring(duration: 0.25)) { toast = message }
        Task {
            try? await Task.sleep(for: .seconds(1.3))
            withAnimation(.easeOut(duration: 0.25)) { toast = nil }
        }
    }
}
