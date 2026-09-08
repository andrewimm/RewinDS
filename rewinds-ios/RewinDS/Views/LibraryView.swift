import SwiftUI
import RewindsKit
import UniformTypeIdentifiers

/// The start screen: a grid of ROMs (bundled + imported) with a prominent importer and a
/// route into system-file settings. This is the app's front door before a game loads.
struct LibraryView: View {
    @EnvironmentObject private var model: AppModel
    @StateObject private var library = GameLibrary.shared

    @State private var showImporter = false
    @State private var showSettings = false

    private let columns = [GridItem(.adaptive(minimum: 150, maximum: 220), spacing: 16)]

    private var romTypes: [UTType] {
        [UTType(filenameExtension: "gba"), UTType(filenameExtension: "nds")].compactMap { $0 } + [.data]
    }

    var body: some View {
        NavigationStack {
            ScrollView {
                if library.games.isEmpty {
                    emptyState
                        .padding(.top, 80)
                } else {
                    LazyVGrid(columns: columns, spacing: 16) {
                        ForEach(library.games) { game in
                            GameCard(game: game)
                                .onTapGesture { model.launch(game) }
                                .contextMenu {
                                    if !game.isBundled {
                                        Button(role: .destructive) {
                                            library.delete(game)
                                        } label: {
                                            Label("Delete", systemImage: "trash")
                                        }
                                    }
                                }
                        }
                    }
                    .padding(20)
                }
            }
            .background(Color(hex: 0x111018).ignoresSafeArea())
            .navigationTitle("RewinDS")
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Button {
                        showSettings = true
                    } label: {
                        Image(systemName: "gearshape")
                    }
                }
                ToolbarItem(placement: .topBarTrailing) {
                    Button {
                        showImporter = true
                    } label: {
                        Label("Open ROM", systemImage: "plus")
                    }
                }
            }
            .safeAreaInset(edge: .bottom) {
                Button {
                    showImporter = true
                } label: {
                    Label("Open a ROM…", systemImage: "tray.and.arrow.down")
                        .font(.headline)
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 14)
                }
                .buttonStyle(.borderedProminent)
                .tint(Color(hex: 0x8B7FF2))
                .padding(.horizontal, 20)
                .padding(.bottom, 8)
            }
        }
        .fileImporter(isPresented: $showImporter, allowedContentTypes: romTypes) { result in
            handleImport(result)
        }
        .sheet(isPresented: $showSettings) {
            SettingsView()
        }
    }

    private var emptyState: some View {
        VStack(spacing: 14) {
            Image(systemName: "gamecontroller")
                .font(.system(size: 52))
                .foregroundStyle(Color(hex: 0x8B7FF2))
            Text("No games yet")
                .font(.title2.weight(.semibold))
            Text("Open a .gba or .nds ROM to get started. You can add your BIOS files under the gear menu.")
                .font(.subheadline)
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
                .padding(.horizontal, 40)
        }
    }

    private func handleImport(_ result: Result<URL, Error>) {
        guard case let .success(url) = result else { return }
        do {
            let entry = try library.importROM(from: url)
            model.launch(entry)
        } catch {
            model.alert = .init(title: "Import failed", message: error.localizedDescription)
        }
    }
}

/// A single ROM tile: a console badge, the ROM's name, and where it came from.
struct GameCard: View {
    let game: GameEntry

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ZStack {
                LinearGradient(
                    colors: game.console == .nds
                        ? [Color(hex: 0x2A2A31), Color(hex: 0x121214)]
                        : [Color(hex: 0x2E2866), Color(hex: 0x1C1642)],
                    startPoint: .topLeading, endPoint: .bottomTrailing)
                Image(systemName: game.console == .nds ? "square.split.1x2" : "rectangle")
                    .font(.system(size: 34))
                    .foregroundStyle(.white.opacity(0.85))
            }
            .frame(height: 110)

            VStack(alignment: .leading, spacing: 4) {
                Text(game.name)
                    .font(.subheadline.weight(.semibold))
                    .lineLimit(2)
                    .foregroundStyle(.white)
                HStack(spacing: 6) {
                    Text(game.console == .nds ? "DS" : "GBA")
                        .font(.caption2.weight(.bold))
                        .padding(.horizontal, 7).padding(.vertical, 2)
                        .background(Color(hex: 0x8B7FF2).opacity(0.25), in: Capsule())
                        .foregroundStyle(Color(hex: 0xB9B2F6))
                    if game.isBundled {
                        Text("bundled")
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                    }
                }
            }
            .padding(12)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Color(hex: 0x1A1A20))
        }
        .clipShape(RoundedRectangle(cornerRadius: 14, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .strokeBorder(.white.opacity(0.06), lineWidth: 1))
    }
}
