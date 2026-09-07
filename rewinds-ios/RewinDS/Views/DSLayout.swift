import SwiftUI

/// The portrait DS shell: a top bar (Save / RewinDS / Menu), the two stacked screens
/// with a hinge divider (the lower one touch-enabled), then L/R, the D-pad and X/Y/A/B
/// diamond, and small START/SELECT keys.
struct DSLayout: View {
    @ObservedObject var session: EmulatorSession
    let shell: Shell
    let registry: ControlRegistry
    let onSave: () -> Void
    let onRewind: () -> Void
    let onMenu: () -> Void

    private let dsAspect: CGFloat = 256.0 / 192.0

    var body: some View {
        ZStack {
            shell.background.ignoresSafeArea()

            VStack(spacing: 0) {
                topBar
                    .padding(.horizontal, 22)
                    .padding(.top, 4)
                    .padding(.bottom, 10)

                // Screens sit outside the controller cluster: disjoint touch surfaces, so
                // the touchscreen and the buttons work at the same time without overlap.
                VStack(spacing: 0) {
                    GameScreen(session: session, index: 0, aspect: dsAspect, shell: shell)
                    hinge
                    GameScreen(session: session, index: 1, aspect: dsAspect, shell: shell, touch: true)
                }
                .padding(.horizontal, 16)

                Spacer(minLength: 12)

                ControllerCluster(registry: registry, session: session) {
                    controls
                }
                .padding(.bottom, 8)
            }
        }
    }

    private var topBar: some View {
        HStack {
            ToolbarActionButton(systemImage: "square.and.arrow.down", title: "SAVE", shell: shell, action: onSave)
            Spacer()
            RewinDSWordmark(shell: shell, action: onRewind)
            Spacer()
            ToolbarActionButton(systemImage: "line.3.horizontal", title: "MENU", shell: shell, action: onMenu)
        }
    }

    private var hinge: some View {
        HStack(spacing: 8) {
            Rectangle().fill(shell.face.opacity(0.25)).frame(height: 1)
            Capsule().fill(shell.faceEdge).frame(width: 22, height: 5)
            Text("MIC")
                .font(.system(size: 9, weight: .semibold))
                .tracking(1)
                .foregroundStyle(shell.caption)
            Rectangle().fill(shell.face.opacity(0.25)).frame(height: 1)
        }
        .padding(.vertical, 10)
    }

    private var controls: some View {
        VStack(spacing: 18) {
            HStack {
                ShoulderKey(button: .l, label: "L", shell: shell, width: 116, height: 40)
                Spacer()
                ShoulderKey(button: .r, label: "R", shell: shell, width: 116, height: 40)
            }

            HStack(alignment: .center) {
                DPadView(shell: shell, size: 142)
                Spacer()
                DSFaceButtons(shell: shell)
            }

            HStack(spacing: 46) {
                MiniKey(button: .start, label: "START", shell: shell)
                MiniKey(button: .select, label: "SELECT", shell: shell)
            }
            .padding(.top, 2)
        }
        .padding(.horizontal, 26)
    }
}
