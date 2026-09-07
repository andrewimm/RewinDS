import SwiftUI

/// Hosts a running session: chooses the console's shell layout, owns the control
/// registry, and turns the toolbar actions into core operations plus brief feedback.
struct EmulatorView: View {
    @EnvironmentObject private var model: AppModel
    @ObservedObject var session: EmulatorSession

    // A reference type held stably for the life of this view.
    @State private var registry = ControlRegistry()
    @State private var toast: String?
    @State private var showMenu = false

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
                    onSave: save, onMenu: openMenu)
            case .nds:
                DSLayout(
                    session: session, shell: shell, registry: registry,
                    onSave: save, onRewind: rewind, onMenu: openMenu)
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
        .overlay {
            if showMenu {
                PauseMenu(
                    shell: shell,
                    onResume: resumeGame,
                    onSave: { session.saveNow() },
                    onQuit: { model.exitGame() })
                .transition(.opacity)
            }
        }
    }

    private func save() {
        session.saveNow()
        flash("Saved")
    }

    private func openMenu() {
        session.openMenu()
        withAnimation(.easeOut(duration: 0.2)) { showMenu = true }
    }

    private func resumeGame() {
        withAnimation(.easeOut(duration: 0.2)) { showMenu = false }
        session.closeMenu()
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

/// The in-game pause overlay. Emulation is already frozen by the session while this is
/// up; tapping the dimmed backdrop or Resume returns to the game.
private struct PauseMenu: View {
    let shell: Shell
    let onResume: () -> Void
    let onSave: () -> Void
    let onQuit: () -> Void

    @State private var savedConfirm = false

    var body: some View {
        ZStack {
            Color.black.opacity(0.55)
                .ignoresSafeArea()
                .contentShape(Rectangle())
                .onTapGesture(perform: onResume)

            VStack(spacing: 12) {
                Text("Paused")
                    .font(.title2.weight(.bold))
                    .foregroundStyle(.white)
                    .padding(.bottom, 4)

                button("Resume", "play.fill", tint: shell.accent, action: onResume)
                button(savedConfirm ? "Saved ✓" : "Save", "square.and.arrow.down",
                       tint: .white.opacity(0.16)) {
                    onSave()
                    withAnimation { savedConfirm = true }
                    Task {
                        try? await Task.sleep(for: .seconds(1.2))
                        withAnimation { savedConfirm = false }
                    }
                }
                button("Quit to Library", "rectangle.portrait.and.arrow.right",
                       tint: .red.opacity(0.85), action: onQuit)
            }
            .padding(22)
            .frame(maxWidth: 300)
            .background(shell.background, in: RoundedRectangle(cornerRadius: 20, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: 20, style: .continuous)
                    .strokeBorder(.white.opacity(0.08)))
            .shadow(color: .black.opacity(0.5), radius: 20, y: 8)
            .padding(40)
        }
    }

    private func button(_ title: String, _ icon: String, tint: Color, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Label(title, systemImage: icon)
                .font(.headline)
                .frame(maxWidth: .infinity)
                .padding(.vertical, 12)
        }
        .buttonStyle(.borderedProminent)
        .tint(tint)
        .foregroundStyle(.white)
    }
}
