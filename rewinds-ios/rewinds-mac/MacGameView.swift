import SwiftUI
import RewindsKit

/// The running game on macOS: renders the console's screen(s), maps the keyboard to
/// buttons (mouse drives the DS touch screen from `MacMetalScreenView`), and exposes the
/// link picker from the toolbar. No on-screen controls — keyboard + mouse only.
///
/// Keys: arrows = D-pad, X = A, Z = B, A = L, S = R, Return = Start, Backspace = Select,
/// and hold Space to fast-forward (matching the desktop host).
struct MacGameView: View {
    @ObservedObject var session: EmulatorSession
    @ObservedObject private var link: LinkController

    @State private var mask: UInt32 = 0
    @State private var showLink = false
    @FocusState private var focused: Bool

    init(session: EmulatorSession) {
        self.session = session
        _link = ObservedObject(wrappedValue: session.link)
    }

    var body: some View {
        screens
            .padding(12)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(Color.black)
            .focusable()
            .focusEffectDisabled()
            .focused($focused)
            .onAppear { focused = true }
            .onKeyPress(phases: [.down, .up]) { press in handle(press) }
            .toolbar {
                ToolbarItem(placement: .primaryAction) {
                    Button { showLink = true } label: {
                        Label(linkLabel, systemImage: "antenna.radiowaves.left.and.right")
                    }
                }
            }
            .sheet(isPresented: $showLink) {
                MacLinkView(link: session.link, rom: session.romId, console: session.console)
            }
    }

    @ViewBuilder private var screens: some View {
        switch session.console {
        case .gba:
            MacMetalScreenView(session: session, index: 0)
                .aspectRatio(240.0 / 160.0, contentMode: .fit)
        case .nds:
            VStack(spacing: 6) {
                MacMetalScreenView(session: session, index: 0)
                    .aspectRatio(256.0 / 192.0, contentMode: .fit)
                MacMetalScreenView(session: session, index: 1, isTouchScreen: true)
                    .aspectRatio(256.0 / 192.0, contentMode: .fit)
            }
        }
    }

    private var linkLabel: String {
        switch link.state {
        case .offline, .disconnected: return "Link"
        case .hosting: return "Hosting…"
        case .connecting: return "Connecting…"
        case .linked: return "Linked"
        }
    }

    // --- Keyboard -------------------------------------------------------------

    private func handle(_ press: KeyPress) -> KeyPress.Result {
        // Hold Space to fast-forward (not a console button).
        if press.key == .space {
            session.setWarp(press.phase != .up)
            return .handled
        }
        guard let button = Self.button(for: press) else { return .ignored }
        if press.phase == .up {
            mask &= ~button.rawValue
        } else {
            mask |= button.rawValue
        }
        session.setButtons(mask)
        return .handled
    }

    private static func button(for press: KeyPress) -> GameButton? {
        switch press.key {
        case .upArrow: return .up
        case .downArrow: return .down
        case .leftArrow: return .left
        case .rightArrow: return .right
        case .return: return .start
        case .delete: return .select   // Backspace
        default: break
        }
        switch press.characters.lowercased() {
        case "x": return .a
        case "z": return .b
        case "a": return .l
        case "s": return .r
        default: return nil
        }
    }
}
