import SwiftUI

/// The landscape GBA shell: one screen framed by L/R shoulders, the D-pad and A/B keys,
/// SELECT/START pills under the screen, and Save/Menu in the bottom corners.
struct GBALayout: View {
    @ObservedObject var session: EmulatorSession
    let shell: Shell
    let registry: ControlRegistry
    let onSave: () -> Void
    let onMenu: () -> Void

    var body: some View {
        ZStack {
            LinearGradient(
                colors: [shell.background, shell.backgroundEdge],
                startPoint: .top, endPoint: .bottom)
                .ignoresSafeArea()

            ControllerCluster(registry: registry, session: session) {
                HStack(alignment: .top, spacing: 12) {
                    VStack(spacing: 0) {
                        ShoulderKey(button: .l, label: "L", shell: shell, width: 132)
                        Spacer()
                        DPadView(shell: shell, size: 148)
                        Spacer()
                    }

                    Spacer(minLength: 4)

                    VStack(spacing: 10) {
                        Spacer(minLength: 0)
                        GameScreen(session: session, index: 0, aspect: 240.0 / 160.0, shell: shell)
                            .frame(maxHeight: .infinity)
                        Text("GBA\u{2003}EMULATOR")
                            .font(.system(size: 12, weight: .semibold))
                            .tracking(1.5)
                            .foregroundStyle(shell.caption)
                        HStack(spacing: 22) {
                            PillKey(button: .select, label: "SELECT", shell: shell)
                            PillKey(button: .start, label: "START", shell: shell)
                        }
                        Spacer(minLength: 8)
                    }
                    .frame(maxWidth: .infinity)

                    Spacer(minLength: 4)

                    VStack(spacing: 0) {
                        ShoulderKey(button: .r, label: "R", shell: shell, width: 132)
                        Spacer()
                        GBAFaceButtons(shell: shell)
                        Spacer()
                    }
                }
                .padding(.horizontal, 26)
                .padding(.vertical, 16)
            }

            // Save / Menu sit above the touch overlay so they take taps directly.
            VStack {
                Spacer()
                HStack {
                    ToolbarActionButton(systemImage: "square.and.arrow.down", title: "SAVE", shell: shell, action: onSave)
                    Spacer()
                    ToolbarActionButton(systemImage: "line.3.horizontal", title: "MENU", shell: shell, action: onMenu)
                }
                .padding(.horizontal, 30)
                .padding(.bottom, 10)
            }
        }
    }
}
