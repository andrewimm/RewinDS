import SwiftUI

/// The portrait DS shell: a top bar (RewinDS / Menu), the two stacked screens with a
/// hinge divider (the lower one touch-enabled), then L/R, the D-pad and X/Y/A/B diamond,
/// and small START/SELECT keys.
struct DSLayout: View {
    @ObservedObject var session: EmulatorSession
    let shell: Shell
    let registry: ControlRegistry
    let onRewind: () -> Void
    let onMenu: () -> Void

    private let dsAspect: CGFloat = 256.0 / 192.0

    var body: some View {
        ZStack {
            shell.background.ignoresSafeArea()

            GeometryReader { geo in
                // Reserve the top bar and controls, then give the two 4:3 screens the
                // largest matching size that fits the remaining height (or the width).
                let topBarH: CGFloat = 40
                let controlsReserve: CGFloat = 244
                let gap: CGFloat = 12
                let sideMargin: CGFloat = 8
                let budget = geo.size.height - topBarH - controlsReserve - gap
                let byHeight = max(budget / 2, 80)
                let byWidth = (geo.size.width - 2 * sideMargin) * 3.0 / 4.0
                let screenH = min(byHeight, byWidth)
                let screenW = screenH * 4.0 / 3.0

                VStack(spacing: 0) {
                    topBar
                        .frame(height: topBarH)
                        .padding(.horizontal, 22)

                    Spacer(minLength: 4)

                    // Screens sit outside the controller cluster: disjoint touch surfaces,
                    // so the touchscreen and buttons work at once. A small gap keeps the
                    // two-screen feel without a hinge decoration.
                    VStack(spacing: gap) {
                        GameScreen(session: session, index: 0, aspect: dsAspect, shell: shell)
                            .frame(width: screenW, height: screenH)
                        GameScreen(session: session, index: 1, aspect: dsAspect, shell: shell, touch: true)
                            .frame(width: screenW, height: screenH)
                    }
                    .frame(maxWidth: .infinity)

                    Spacer(minLength: 4)

                    ControllerCluster(registry: registry, session: session) {
                        controls
                    }
                    .padding(.bottom, 8)
                }
            }
        }
    }

    private var topBar: some View {
        ZStack {
            RewinDSWordmark(shell: shell, action: onRewind) // stays centered
            HStack {
                Spacer()
                ToolbarActionButton(systemImage: "line.3.horizontal", title: "MENU", shell: shell, action: onMenu)
            }
        }
    }

    private var controls: some View {
        VStack(spacing: 14) {
            HStack {
                ShoulderKey(button: .l, label: "L", shell: shell, width: 108, height: 36)
                Spacer()
                ShoulderKey(button: .r, label: "R", shell: shell, width: 108, height: 36)
            }

            HStack(alignment: .center) {
                DPadView(shell: shell, size: 128)
                Spacer()
                DSFaceButtons(shell: shell, diameter: 52, spread: 44)
            }

            HStack(spacing: 46) {
                MiniKey(button: .start, label: "START", shell: shell)
                MiniKey(button: .select, label: "SELECT", shell: shell)
            }
        }
        .padding(.horizontal, 26)
    }
}
