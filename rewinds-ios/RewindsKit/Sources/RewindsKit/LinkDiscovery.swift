import Foundation
import Network
import Combine

/// One RewinDS host discovered on the local network (or over peer-to-peer Wi-Fi).
public struct DiscoveredPeer: Identifiable, Equatable {
    public let id: String            // stable key derived from the endpoint
    public let name: String          // the host's advertised device name
    public let console: String?      // "gba" / "nds" — must match to link
    public let version: String?      // its core ABI version — must match to link
    public let rom: String?          // the ROM's display name (informational only)
    public let endpoint: NWEndpoint

    public static func == (lhs: DiscoveredPeer, rhs: DiscoveredPeer) -> Bool { lhs.id == rhs.id }
}

/// Browses for RewinDS link hosts advertised over Bonjour and publishes the live list.
/// Discovery never touches the emulator core, so it lives entirely on the UI side.
public final class LinkDiscovery: ObservableObject {
    @Published public private(set) var peers: [DiscoveredPeer] = []
    @Published public private(set) var browsing = false

    private var browser: NWBrowser?

    public init() {}

    /// Start browsing. Idempotent.
    public func start() {
        guard browser == nil else { return }
        let params = NWParameters()
        params.includePeerToPeer = true
        let browser = NWBrowser(for: .bonjourWithTXTRecord(type: LinkService.type, domain: nil), using: params)
        browser.browseResultsChangedHandler = { [weak self] results, _ in
            self?.update(results)
        }
        browser.stateUpdateHandler = { [weak self] state in
            switch state {
            case .ready: DispatchQueue.main.async { self?.browsing = true }
            case .failed, .cancelled: DispatchQueue.main.async { self?.browsing = false }
            default: break
            }
        }
        self.browser = browser
        browser.start(queue: .main)
    }

    /// Stop browsing and clear the list.
    public func stop() {
        browser?.cancel()
        browser = nil
        peers = []
        browsing = false
    }

    private func update(_ results: Set<NWBrowser.Result>) {
        let peers: [DiscoveredPeer] = results.compactMap { result in
            guard case let .service(name, _, _, _) = result.endpoint else { return nil }
            var console: String?
            var version: String?
            var rom: String?
            if case let .bonjour(txt) = result.metadata {
                console = LinkService.value(txt, LinkService.txtConsole)
                version = LinkService.value(txt, LinkService.txtVersion)
                rom = LinkService.value(txt, LinkService.txtRom)
            }
            return DiscoveredPeer(id: "\(result.endpoint)", name: name, console: console, version: version, rom: rom, endpoint: result.endpoint)
        }
        DispatchQueue.main.async {
            self.peers = peers.sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
        }
    }
}
