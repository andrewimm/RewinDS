import SwiftUI

/// The landscape GBA shell: a maximized centered screen with the controls pushed out to
/// the corners of the side margins — L/R at the top edges, the D-pad low-left, A/B
/// low-right, SELECT/START split to the far sides, and Save/Menu in the bottom corners.
struct GBALayout: View {
    @ObservedObject var session: EmulatorSession
    let shell: Shell
    let registry: ControlRegistry
    let onMenu: () -> Void

    var body: some View {
        ZStack {
            LinearGradient(
                colors: [shell.background, shell.backgroundEdge],
                startPoint: .top, endPoint: .bottom)
                .ignoresSafeArea()

            ControllerCluster(registry: registry, session: session) {
                GeometryReader { geo in
                    // Screen pushed to the top; leave a side margin for the controls and a
                    // small band beneath for START/SELECT.
                    let sideMargin: CGFloat = 150
                    let topMargin: CGFloat = 8
                    let bottomReserve: CGFloat = 46
                    let maxWidth = max(geo.size.width - 2 * sideMargin, 120)
                    let height = min(geo.size.height - topMargin - bottomReserve, maxWidth / 1.5)
                    let width = height * 1.5

                    ZStack {
                        // Screen at the top, with START/SELECT tucked directly beneath it.
                        VStack(spacing: 8) {
                            GameScreen(session: session, index: 0, aspect: 240.0 / 160.0, shell: shell)
                                .frame(width: width, height: height)
                            // SELECT / START aligned under the screen's left and right edges.
                            HStack {
                                PillKey(button: .select, label: "SELECT", shell: shell, width: 66)
                                Spacer()
                                PillKey(button: .start, label: "START", shell: shell, width: 66)
                            }
                            .frame(width: width)
                        }
                        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
                        .padding(.top, topMargin)

                        // L / R — small, near the top edges, dropped a little for reach.
                        place(.topLeading, top: 26, leading: 18) {
                            ShoulderKey(button: .l, label: "L", shell: shell, width: 88, height: 36)
                        }
                        place(.topTrailing, top: 26, trailing: 18) {
                            ShoulderKey(button: .r, label: "R", shell: shell, width: 88, height: 36)
                        }

                        // D-pad — south-west, clear of the Save button below it.
                        place(.bottomLeading, leading: 18, bottom: 74) {
                            DPadView(shell: shell, size: 112)
                        }
                        // A / B — south-east, clear of the Menu button.
                        place(.bottomTrailing, bottom: 70, trailing: 14) {
                            GBAFaceButtons(shell: shell, diameter: 54)
                        }
                    }
                    .frame(width: geo.size.width, height: geo.size.height)
                }
            }

            // Menu sits above the touch overlay in the bottom-right corner.
            VStack {
                Spacer()
                HStack {
                    Spacer()
                    ToolbarActionButton(systemImage: "line.3.horizontal", title: "MENU", shell: shell, action: onMenu)
                }
                .padding(.horizontal, 22)
                .padding(.bottom, 8)
            }
        }
    }

    /// Anchor a control to a corner of the play area with edge insets.
    private func place<V: View>(
        _ alignment: Alignment,
        top: CGFloat = 0, leading: CGFloat = 0, bottom: CGFloat = 0, trailing: CGFloat = 0,
        @ViewBuilder _ content: () -> V
    ) -> some View {
        content()
            .padding(.top, top).padding(.leading, leading)
            .padding(.bottom, bottom).padding(.trailing, trailing)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: alignment)
    }
}
