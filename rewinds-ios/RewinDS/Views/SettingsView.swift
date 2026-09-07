import SwiftUI
import UniformTypeIdentifiers

/// The BIOS/firmware list — reused by the Settings sheet (from the library) and the
/// in-game menu's "System Files" page. These are never shipped in source; the user
/// imports their own dumps here (stored privately in app-support), and imports take
/// precedence over any copy the developer bundled from `dev-assets/`.
struct SystemFilesList: View {
    @StateObject private var bios = BIOSStore.shared
    @State private var importingRole: SystemFile?

    private var biosTypes: [UTType] {
        [UTType(filenameExtension: "bin"), .data].compactMap { $0 }
    }

    var body: some View {
        List {
            Section {
                ForEach(SystemFile.allCases) { file in
                    row(file)
                }
            } header: {
                Text("System files")
            } footer: {
                Text("BIOS and firmware dumps are never included in the app's source. Add your own here — imported files stay private to the app and override any bundled copy.")
            }
        }
        .fileImporter(
            isPresented: Binding(
                get: { importingRole != nil },
                set: { if !$0 { importingRole = nil } }),
            allowedContentTypes: biosTypes
        ) { result in
            if let role = importingRole, case let .success(url) = result {
                try? bios.importFile(from: url, as: role)
            }
            importingRole = nil
        }
    }

    @ViewBuilder
    private func row(_ file: SystemFile) -> some View {
        HStack(spacing: 12) {
            Image(systemName: statusIcon(file))
                .foregroundStyle(statusColor(file))
                .font(.system(size: 18))
                .frame(width: 24)

            VStack(alignment: .leading, spacing: 2) {
                Text(file.title)
                Text(statusText(file))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Spacer()

            if bios.hasImport(file) {
                Menu {
                    Button("Replace…") { importingRole = file }
                    Button("Remove", role: .destructive) { bios.removeImport(file) }
                } label: {
                    Image(systemName: "ellipsis.circle")
                }
            } else {
                Button("Import") { importingRole = file }
                    .buttonStyle(.bordered)
            }
        }
        .padding(.vertical, 2)
    }

    private func statusText(_ file: SystemFile) -> String {
        if bios.hasImport(file) { return "Imported" }
        if bios.isAvailable(file) { return "Bundled with the build" }
        return file.required ? "Required — not added" : "Optional — not added"
    }

    private func statusIcon(_ file: SystemFile) -> String {
        if bios.isAvailable(file) { return "checkmark.circle.fill" }
        return file.required ? "exclamationmark.circle" : "circle"
    }

    private func statusColor(_ file: SystemFile) -> Color {
        if bios.isAvailable(file) { return .green }
        return file.required ? .orange : .secondary
    }
}

/// Settings sheet shown from the library.
struct SettingsView: View {
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            SystemFilesList()
                .navigationTitle("Settings")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Done") { dismiss() }
                    }
                }
        }
    }
}
