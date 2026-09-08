import SwiftUI
import RewindsKit

/// The macOS link picker: host over Bonjour, join a nearby RewinDS, or connect to a dev
/// reference host. Same `LinkController`/`LinkDiscovery` as iOS — only the chrome differs.
struct MacLinkView: View {
    @ObservedObject var link: LinkController
    let rom: String
    let console: Console

    @StateObject private var discovery = LinkDiscovery()
    @Environment(\.dismiss) private var dismiss
    @AppStorage("link.dev.host") private var devHost = ""
    @AppStorage("link.dev.port") private var devPort = "5555"

    private var myVersion: String { String(EmulatorCore.coreVersion) }
    private var myConsoleTag: String { console == .nds ? "nds" : "gba" }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack {
                Text("Link").font(.headline)
                Spacer()
                Button("Done") { dismiss() }
            }
            statusRow

            switch link.state {
            case .offline, .disconnected: content
            case .hosting: hostingRow
            case .connecting, .linked: EmptyView()
            }

            Divider()
            advanced
        }
        .padding(20)
        .frame(width: 380)
        .onAppear { discovery.start() }
        .onDisappear { discovery.stop() }
    }

    @ViewBuilder private var content: some View {
        Button { link.host(rom: rom, console: myConsoleTag) } label: {
            Label("Host a link", systemImage: "antenna.radiowaves.left.and.right")
                .frame(maxWidth: .infinity)
        }
        .controlSize(.large)

        Text("Nearby").font(.caption.weight(.semibold)).foregroundStyle(.secondary)
        if discovery.peers.isEmpty {
            Text("Looking for nearby players…").font(.footnote).foregroundStyle(.secondary)
        } else {
            ForEach(discovery.peers) { peer in peerRow(peer) }
        }
    }

    private var hostingRow: some View {
        HStack(spacing: 10) {
            ProgressView().controlSize(.small)
            Text("Waiting for a player to join…").font(.subheadline)
            Spacer()
            Button("Stop") { link.disconnect() }
        }
    }

    private var statusRow: some View {
        HStack(spacing: 8) {
            Circle().fill(statusColor).frame(width: 9, height: 9)
            Text(statusText).font(.subheadline)
            Spacer()
            if isActive {
                Button("Disconnect") { link.disconnect() }.foregroundStyle(.red)
            }
        }
    }

    @ViewBuilder private func peerRow(_ peer: DiscoveredPeer) -> some View {
        let issue = mismatchReason(peer)
        Button { if issue == nil { link.join(peer) } } label: {
            HStack(spacing: 10) {
                Image(systemName: "desktopcomputer").foregroundStyle(issue == nil ? Color.accentColor : .secondary)
                VStack(alignment: .leading, spacing: 1) {
                    Text(peer.name)
                    Text(issue ?? (peer.rom ?? "RewinDS"))
                        .font(.caption)
                        .foregroundStyle(issue == nil ? Color.secondary : Color.orange)
                }
                Spacer()
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(issue != nil)
    }

    private var advanced: some View {
        DisclosureGroup("Advanced (dev)") {
            HStack {
                TextField("Host", text: $devHost)
                TextField("Port", text: $devPort).frame(width: 70)
                Button("Connect") {
                    if let port = UInt16(devPort.trimmingCharacters(in: .whitespaces)) {
                        link.connectDev(host: devHost.trimmingCharacters(in: .whitespaces), port: port)
                    }
                }
                .disabled(devHost.isEmpty || UInt16(devPort) == nil)
            }
            .padding(.top, 6)
        }
    }

    private func mismatchReason(_ peer: DiscoveredPeer) -> String? {
        if let v = peer.version, v != myVersion { return "App version mismatch" }
        if let c = peer.console, c != myConsoleTag {
            return "\(c == "nds" ? "DS" : "GBA") game — different system"
        }
        return nil
    }

    private var isActive: Bool {
        switch link.state {
        case .hosting, .connecting, .linked: return true
        case .offline, .disconnected: return false
        }
    }

    private var statusText: String {
        switch link.state {
        case .offline: return "Not linked"
        case .hosting: return "Hosting"
        case .connecting: return "Connecting…"
        case .linked: return "Linked"
        case .disconnected(let message): return "Disconnected: \(message)"
        }
    }

    private var statusColor: Color {
        switch link.state {
        case .offline: return .gray
        case .hosting, .connecting: return .yellow
        case .linked: return .green
        case .disconnected: return .orange
        }
    }
}
