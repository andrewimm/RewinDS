import SwiftUI
import RewindsKit

/// The in-game Link menu: host a link over the local network (or peer-to-peer Wi-Fi),
/// join a nearby RewinDS running the same game, and see live connection status. An
/// "Advanced" section keeps the dev connect to a `rewinds --link listen` reference host.
struct LinkPage: View {
    let shell: Shell
    @ObservedObject var link: LinkController
    let rom: String
    let console: Console

    @StateObject private var discovery = LinkDiscovery()
    @AppStorage("link.dev.host") private var devHost = ""
    @AppStorage("link.dev.port") private var devPort = "5555"

    private var myVersion: String { String(EmulatorCore.coreVersion) }
    private var myConsoleTag: String { console == .nds ? "nds" : "gba" }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                statusCard

                switch link.state {
                case .offline, .disconnected:
                    hostButton
                    nearbySection
                    advancedSection
                case .hosting:
                    waitingRow
                case .connecting, .linked:
                    EmptyView()
                }
            }
            .padding(20)
        }
        .onAppear { discovery.start() }
        .onDisappear { discovery.stop() }
    }

    // --- Status ---------------------------------------------------------------

    private var statusCard: some View {
        HStack(spacing: 10) {
            Circle().fill(statusColor).frame(width: 10, height: 10)
            Text(statusText).font(.subheadline.weight(.medium)).foregroundStyle(.white)
            Spacer()
            if isActive {
                Button("Disconnect") { link.disconnect() }
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.red)
            }
        }
        .padding(.vertical, 12).padding(.horizontal, 14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.white.opacity(0.06), in: RoundedRectangle(cornerRadius: 12))
    }

    private var isActive: Bool {
        switch link.state {
        case .hosting, .connecting, .linked: return true
        case .offline, .disconnected: return false
        }
    }

    // --- Host -----------------------------------------------------------------

    private var hostButton: some View {
        Button { link.host(rom: rom, console: myConsoleTag) } label: {
            Label("Host a link", systemImage: "antenna.radiowaves.left.and.right")
                .font(.headline).frame(maxWidth: .infinity).padding(.vertical, 12)
        }
        .buttonStyle(.borderedProminent)
        .tint(shell.accent)
        .foregroundStyle(.white)
    }

    private var waitingRow: some View {
        VStack(spacing: 12) {
            ProgressView()
            Text("Waiting for a player to join…")
                .font(.subheadline).foregroundStyle(.secondary)
            Button("Stop hosting") { link.disconnect() }
                .font(.subheadline.weight(.semibold)).foregroundStyle(.red)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 20)
        .background(.white.opacity(0.06), in: RoundedRectangle(cornerRadius: 12))
    }

    // --- Nearby ---------------------------------------------------------------

    private var nearbySection: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text("Nearby").font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                Spacer()
                if discovery.browsing { ProgressView().scaleEffect(0.7) }
            }
            if discovery.peers.isEmpty {
                Text("Looking for nearby players…")
                    .font(.footnote).foregroundStyle(.secondary)
                    .padding(.vertical, 6)
            } else {
                ForEach(discovery.peers) { peer in peerRow(peer) }
            }
        }
    }

    @ViewBuilder
    private func peerRow(_ peer: DiscoveredPeer) -> some View {
        let issue = mismatchReason(peer)
        Button { if issue == nil { link.join(peer) } } label: {
            HStack(spacing: 12) {
                Image(systemName: "iphone.gen3")
                    .foregroundStyle(issue == nil ? shell.accent : .secondary)
                    .frame(width: 24)
                VStack(alignment: .leading, spacing: 2) {
                    Text(peer.name).foregroundStyle(.white).lineLimit(1)
                    Text(issue ?? (peer.rom ?? "RewinDS"))
                        .font(.caption2)
                        .foregroundStyle(issue == nil ? Color.secondary : Color.orange)
                }
                Spacer()
                if issue == nil {
                    Image(systemName: "chevron.forward").font(.caption).foregroundStyle(.secondary)
                }
            }
            .padding(.vertical, 10).padding(.horizontal, 14)
            .background(.white.opacity(0.06), in: RoundedRectangle(cornerRadius: 12))
            .opacity(issue == nil ? 1 : 0.55)
        }
        .buttonStyle(.plain)
        .disabled(issue != nil)
    }

    /// Why this peer can't be joined, or nil if it's linkable. We gate on system + app
    /// version only — a link carries bytes between compatible handhelds and lets the games
    /// sort out compatibility, so a different ROM (Ruby↔Sapphire, FireRed↔LeafGreen, other
    /// languages) is fine and never blocks.
    private func mismatchReason(_ peer: DiscoveredPeer) -> String? {
        if let peerVer = peer.version, peerVer != myVersion { return "App version mismatch" }
        if let peerConsole = peer.console, peerConsole != myConsoleTag {
            let name = peerConsole == "nds" ? "DS" : "GBA"
            return "\(name) game — different system"
        }
        return nil
    }

    // --- Advanced (dev) -------------------------------------------------------

    private var advancedSection: some View {
        DisclosureGroup("Advanced") {
            VStack(alignment: .leading, spacing: 12) {
                Text("Connect to a reference host: `rewinds <rom> --link listen <port>`.")
                    .font(.footnote).foregroundStyle(.secondary)
                devField("Host", text: $devHost, placeholder: "192.168.1.42", keyboard: .URL)
                devField("Port", text: $devPort, placeholder: "5555", keyboard: .numberPad)
                Button("Connect (dev)") {
                    if let port = UInt16(devPort.trimmingCharacters(in: .whitespaces)) {
                        link.connectDev(host: devHost.trimmingCharacters(in: .whitespaces), port: port)
                    }
                }
                .buttonStyle(.bordered)
                .tint(shell.accent)
                .disabled(devHost.isEmpty || UInt16(devPort) == nil)
            }
            .padding(.top, 8)
        }
        .font(.subheadline)
        .tint(.secondary)
        .padding(.vertical, 12).padding(.horizontal, 14)
        .background(.white.opacity(0.04), in: RoundedRectangle(cornerRadius: 12))
    }

    private func devField(_ label: String, text: Binding<String>, placeholder: String, keyboard: UIKeyboardType) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(label).font(.caption.weight(.semibold)).foregroundStyle(.secondary)
            TextField(placeholder, text: text)
                .keyboardType(keyboard)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .foregroundStyle(.white)
                .padding(.vertical, 10).padding(.horizontal, 12)
                .background(.white.opacity(0.08), in: RoundedRectangle(cornerRadius: 10))
        }
    }

    // --- Status text ----------------------------------------------------------

    private var statusText: String {
        switch link.state {
        case .offline: return "Not linked"
        case .hosting: return "Hosting — waiting for a player"
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
