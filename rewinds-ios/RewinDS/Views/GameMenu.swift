import SwiftUI

/// A page in the in-game menu. Add a case here plus a branch in `GameMenu.pageContent`
/// to introduce a new menu (e.g. a future `.gamepad`); navigation and back handling come
/// for free from the page stack.
enum MenuPage: Hashable {
    case root
    case games
    case systemFiles
    case link

    var title: String {
        switch self {
        case .root: return "Paused"
        case .games: return "Games"
        case .systemFiles: return "System Files"
        case .link: return "Link (dev)"
        }
    }
}

/// The in-game menu overlay. It owns an explicit page stack: `push` appends, `back` pops,
/// and Resume (or the ✕, or tapping the backdrop) dismisses the whole thing and returns
/// to the paused game — so you can never get stranded in a submenu.
struct GameMenu: View {
    let shell: Shell
    @ObservedObject var session: EmulatorSession
    let onResume: () -> Void

    @EnvironmentObject private var model: AppModel

    @State private var stack: [MenuPage] = [.root]
    @State private var goingBack = false

    private var current: MenuPage { stack.last ?? .root }

    var body: some View {
        ZStack {
            Color.black.opacity(0.55)
                .ignoresSafeArea()
                .contentShape(Rectangle())
                .onTapGesture(perform: onResume)

            VStack(spacing: 0) {
                header
                Divider().overlay(shell.faceEdge.opacity(0.3))
                pageContent
                    .id(current)
                    .transition(.asymmetric(
                        insertion: .move(edge: goingBack ? .leading : .trailing).combined(with: .opacity),
                        removal: .move(edge: goingBack ? .trailing : .leading).combined(with: .opacity)))
            }
            .frame(maxWidth: 380)
            // Root sizes to its content; list pages fill the height so they can scroll.
            .frame(maxHeight: current == .root ? nil : .infinity)
            .background(shell.background, in: RoundedRectangle(cornerRadius: 20, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: 20, style: .continuous)
                    .strokeBorder(.white.opacity(0.08)))
            .shadow(color: .black.opacity(0.5), radius: 20, y: 8)
            .padding(.vertical, 22)
            .padding(.horizontal, 20)
        }
    }

    // --- Chrome ---------------------------------------------------------------

    private var header: some View {
        ZStack {
            Text(current.title)
                .font(.headline)
                .foregroundStyle(.white)

            HStack {
                if stack.count > 1 {
                    iconButton("chevron.backward", action: back)
                } else {
                    Color.clear.frame(width: 30, height: 30)
                }
                Spacer()
                iconButton("xmark", action: onResume)
            }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 14)
    }

    private func iconButton(_ system: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: system)
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(.white)
                .frame(width: 30, height: 30)
                .background(.white.opacity(0.12), in: Circle())
        }
        .buttonStyle(.plain)
    }

    // --- Pages ----------------------------------------------------------------

    @ViewBuilder
    private var pageContent: some View {
        switch current {
        case .root: rootPage
        case .games: gamesPage
        case .systemFiles: SystemFilesList().scrollContentBackground(.hidden)
        case .link: LinkDevPage(shell: shell, link: session.link)
        }
    }

    private var rootPage: some View {
        // A plain VStack (not a ScrollView) so the panel can size to its content.
        VStack(spacing: 12) {
            filledButton("Resume", "play.fill", tint: shell.accent, action: onResume)
            navRow("Games", "square.grid.2x2") { push(.games) }
            navRow("System Files", "cpu") { push(.systemFiles) }
            navRow("Link (dev)", "antenna.radiowaves.left.and.right") { push(.link) }
            filledButton("Quit to Library", "rectangle.portrait.and.arrow.right",
                         tint: .red.opacity(0.85)) { model.exitGame() }
        }
        .padding(20)
    }

    private var gamesPage: some View {
        ScrollView {
            VStack(spacing: 10) {
                ForEach(model.library.games) { game in
                    Button {
                        if game.id == session.romId {
                            onResume() // already playing this one
                        } else {
                            model.launch(game) // switches; this menu goes away with the session
                        }
                    } label: {
                        HStack(spacing: 12) {
                            Image(systemName: game.console == .nds ? "square.split.1x2" : "rectangle")
                                .foregroundStyle(shell.accent)
                                .frame(width: 24)
                            VStack(alignment: .leading, spacing: 2) {
                                Text(game.name).foregroundStyle(.white).lineLimit(1)
                                Text(game.console == .nds ? "DS" : "GBA")
                                    .font(.caption2).foregroundStyle(.secondary)
                            }
                            Spacer()
                            if game.id == session.romId {
                                Text("Playing").font(.caption2.weight(.semibold))
                                    .foregroundStyle(shell.accent)
                            }
                        }
                        .padding(.vertical, 10).padding(.horizontal, 14)
                        .background(.white.opacity(0.06), in: RoundedRectangle(cornerRadius: 12))
                    }
                    .buttonStyle(.plain)
                }
            }
            .padding(20)
        }
    }

    // --- Building blocks ------------------------------------------------------

    private func push(_ page: MenuPage) {
        goingBack = false
        withAnimation(.easeInOut(duration: 0.22)) { stack.append(page) }
    }

    private func back() {
        goingBack = true
        withAnimation(.easeInOut(duration: 0.22)) { _ = stack.removeLast() }
    }

    private func filledButton(_ title: String, _ icon: String, tint: Color, action: @escaping () -> Void) -> some View {
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

    private func navRow(_ title: String, _ icon: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack {
                Label(title, systemImage: icon).font(.headline)
                Spacer()
                Image(systemName: "chevron.forward").font(.subheadline).foregroundStyle(.secondary)
            }
            .padding(.vertical, 12).padding(.horizontal, 14)
            .frame(maxWidth: .infinity)
            .background(.white.opacity(0.10), in: RoundedRectangle(cornerRadius: 12))
        }
        .buttonStyle(.plain)
        .foregroundStyle(.white)
    }
}
