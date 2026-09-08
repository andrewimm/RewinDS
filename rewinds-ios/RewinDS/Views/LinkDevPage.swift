import SwiftUI

/// A developer affordance for exercising the serial link before device-to-device
/// discovery exists: point the emulator at a RewinDS reference host
/// (`rewinds <rom> --link listen <port>`) on the LAN and complete a link-cable exchange.
/// The real UI (Bonjour discovery, a device picker) replaces this later.
struct LinkDevPage: View {
    let shell: Shell
    @ObservedObject var link: LinkController

    @AppStorage("link.dev.host") private var host = ""
    @AppStorage("link.dev.port") private var portText = "5555"

    private var isActive: Bool {
        switch link.state {
        case .connecting, .linked: return true
        case .offline, .disconnected: return false
        }
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                Text("Connect to a RewinDS reference host running `rewinds <rom> --link listen <port>` on your local network to test the serial link.")
                    .font(.footnote)
                    .foregroundStyle(.secondary)

                field("Host", text: $host, placeholder: "192.168.1.42", keyboard: .URL)
                field("Port", text: $portText, placeholder: "5555", keyboard: .numberPad)

                statusRow

                actionButton
                    .disabled(!isActive && (host.isEmpty || port == nil))
            }
            .padding(20)
        }
    }

    private var port: UInt16? { UInt16(portText.trimmingCharacters(in: .whitespaces)) }

    // --- Building blocks ------------------------------------------------------

    private func field(_ label: String, text: Binding<String>, placeholder: String, keyboard: UIKeyboardType) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(label).font(.caption.weight(.semibold)).foregroundStyle(.secondary)
            TextField(placeholder, text: text)
                .keyboardType(keyboard)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .foregroundStyle(.white)
                .padding(.vertical, 10).padding(.horizontal, 12)
                .background(.white.opacity(0.08), in: RoundedRectangle(cornerRadius: 10))
                .disabled(isActive)
        }
    }

    private var statusRow: some View {
        HStack(spacing: 10) {
            Circle().fill(statusColor).frame(width: 10, height: 10)
            Text(statusText).font(.subheadline).foregroundStyle(.white)
            Spacer()
        }
        .padding(.vertical, 10).padding(.horizontal, 12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.white.opacity(0.06), in: RoundedRectangle(cornerRadius: 10))
    }

    private var actionButton: some View {
        Button {
            if isActive {
                link.disconnect()
            } else if let port {
                link.connect(host: host.trimmingCharacters(in: .whitespaces), port: port)
            }
        } label: {
            Label(isActive ? "Disconnect" : "Connect",
                  systemImage: isActive ? "xmark.circle.fill" : "antenna.radiowaves.left.and.right")
                .font(.headline)
                .frame(maxWidth: .infinity)
                .padding(.vertical, 12)
        }
        .buttonStyle(.borderedProminent)
        .tint(isActive ? .red.opacity(0.85) : shell.accent)
        .foregroundStyle(.white)
    }

    private var statusText: String {
        switch link.state {
        case .offline: return "Offline"
        case .connecting: return "Connecting…"
        case .linked: return "Linked"
        case .disconnected(let message): return "Disconnected: \(message)"
        }
    }

    private var statusColor: Color {
        switch link.state {
        case .offline: return .gray
        case .connecting: return .yellow
        case .linked: return .green
        case .disconnected: return .orange
        }
    }
}
