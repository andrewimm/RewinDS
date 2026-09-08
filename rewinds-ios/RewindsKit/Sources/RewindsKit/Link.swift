import Foundation
import Network
import Combine
#if canImport(UIKit)
import UIKit
#endif

/// A pluggable serial-link carrier. Mirrors the Rust reference host's `Transport` trait
/// (`crates/rewinds/src/link.rs`): `send` queues one opaque frame; `poll` returns whole
/// frames that have arrived. The emulator core is carrier-blind — it only produces and
/// consumes these opaque `LinkFrame` bytes, and neither end knows how they travel.
protocol LinkCarrier: AnyObject {
    func send(_ frame: Data)
    func poll() -> [Data]
}

/// Bonjour service identity for RewinDS serial links, shared by the advertiser (host) and
/// the browser (joiner).
enum LinkService {
    static let type = "_rewinds-link._tcp"
    // A link connects two *compatible systems* and lets the games decide if they recognize
    // each other — exactly like a real cable. So we gate on console family + wire/ABI
    // version, never on ROM identity (Ruby↔Sapphire, FireRed↔LeafGreen, and cross-language
    // trades all use different ROMs and must link). `rom` is display-only.
    static let txtConsole = "con" // "gba" / "nds" — must match to link
    static let txtVersion = "ver" // the core ABI version (rewinds_core_version) — must match
    static let txtRom = "rom"     // the ROM's display name — informational only

    /// The name this device advertises itself under. On recent iOS this is a generic
    /// "iPhone" without a special entitlement, which is fine — the ROM in the TXT record is
    /// the useful disambiguator.
    static var deviceName: String {
        #if canImport(UIKit)
        return UIDevice.current.name
        #else
        return Host.current().localizedName ?? "RewinDS"
        #endif
    }

    static func value(_ txt: NWTXTRecord, _ key: String) -> String? {
        if case let .string(v) = txt.getEntry(for: key) { return v }
        return nil
    }
}

/// A framed serial-link connection over one `NWConnection`. The wire format matches the
/// Rust reference host's `TcpLink`: each frame is length-prefixed with a single byte
/// (`[len][payload…]`). Every carrier path uses this — the dev TCP connect and both sides
/// of a Bonjour link — differing only in how the connection is created and which unit id
/// it takes (0 = parent/host, 1 = child/joiner).
///
/// # Threading
/// The `NWConnection` runs on its own queue; its callbacks touch only this object's
/// `lock`-guarded state, never the emulator core. `send`/`poll`/`currentStatus` are safe
/// to call from the emulation thread.
final class LinkConnection: LinkCarrier {
    enum Status: Equatable {
        case connecting
        case ready
        case failed(String)
        case closed
    }

    let id: UInt8
    let count: UInt8 = 2

    private let connection: NWConnection
    private let queue = DispatchQueue(label: "com.setimmediate.link")
    private let lock = NSLock()
    private var status: Status = .connecting
    private var inbox: [Data] = []
    private var inbuf = [UInt8]()

    /// Connect out to a host:port (the dev TCP path). This unit is the child.
    convenience init(host: String, port: UInt16, id: UInt8) {
        self.init(connection: NWConnection(
            host: NWEndpoint.Host(host),
            port: NWEndpoint.Port(rawValue: port) ?? .any,
            using: Self.params()), id: id)
    }

    /// Connect out to a discovered Bonjour endpoint. This unit is the child (id 1).
    convenience init(endpoint: NWEndpoint, id: UInt8) {
        self.init(connection: NWConnection(to: endpoint, using: Self.params()), id: id)
    }

    /// Adopt a connection — an inbound one accepted by a listener (host, id 0), or one of
    /// the outbound connections above — starting it and pumping its receive loop.
    init(connection: NWConnection, id: UInt8) {
        self.connection = connection
        self.id = id
        connection.stateUpdateHandler = { [weak self] state in self?.onState(state) }
        connection.start(queue: queue)
        receiveNext()
    }

    private static func params() -> NWParameters {
        let tcp = NWProtocolTCP.Options()
        tcp.noDelay = true
        let params = NWParameters(tls: nil, tcp: tcp)
        params.includePeerToPeer = true
        return params
    }

    /// Tear down the connection. Safe to call from any thread.
    func cancel() {
        connection.cancel()
        setStatus(.closed)
    }

    func currentStatus() -> Status {
        lock.lock(); defer { lock.unlock() }
        return status
    }

    // --- LinkCarrier ----------------------------------------------------------

    func send(_ frame: Data) {
        guard frame.count <= 255 else { return } // one-byte length prefix
        var packet = Data([UInt8(frame.count)])
        packet.append(frame)
        connection.send(content: packet, completion: .contentProcessed { _ in })
    }

    func poll() -> [Data] {
        lock.lock(); defer { lock.unlock() }
        guard !inbox.isEmpty else { return [] }
        let frames = inbox
        inbox.removeAll(keepingCapacity: true)
        return frames
    }

    // --- Connection plumbing (carrier queue) ----------------------------------

    private func onState(_ state: NWConnection.State) {
        switch state {
        case .ready:
            setStatus(.ready)
        case .failed(let err), .waiting(let err):
            // `waiting` means the endpoint is unreachable (e.g. connection refused); for a
            // deliberate connect, surface it as a failure rather than retrying forever.
            setStatus(.failed(err.localizedDescription))
        case .cancelled:
            setStatus(.closed)
        default:
            break
        }
    }

    private func receiveNext() {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 8192) { [weak self] data, _, isComplete, error in
            guard let self else { return }
            if let data, !data.isEmpty { self.ingest(data) }
            if let error { self.setStatus(.failed(error.localizedDescription)); return }
            if isComplete { self.setStatus(.closed); return }
            self.receiveNext()
        }
    }

    private func ingest(_ data: Data) {
        lock.lock()
        inbuf.append(contentsOf: data)
        // Split off every complete length-prefixed frame.
        while let len = inbuf.first.map(Int.init), inbuf.count >= 1 + len {
            inbox.append(Data(inbuf[1..<(1 + len)]))
            inbuf.removeFirst(1 + len)
        }
        lock.unlock()
    }

    private func setStatus(_ newStatus: Status) {
        lock.lock(); status = newStatus; lock.unlock()
    }
}

/// Owns the serial-link carrier and shuttles frames between it and the emulator core in
/// lockstep with the run loop. Hosts (advertises + accepts a peer), joins a discovered
/// peer, or connects to a dev TCP host.
///
/// # Threading
/// The active carrier's whole lifecycle lives on the emulation thread: every entry point
/// records an *intent* (a carrier to adopt, or a disconnect) under `intentLock`, and
/// `pump(_:)` — called from `EmulatorSession.tick()` under the core lock — applies it and
/// reconciles the core's link config. So the core is only ever touched from its owning
/// thread. The `NWListener` (advertising) and its accept callback never touch the core;
/// on accept they just record a carrier intent. `state` is published for the UI on the
/// main queue.
public final class LinkController: ObservableObject {
    public enum State: Equatable {
        case offline
        case hosting             // advertising, waiting for a peer
        case connecting
        case linked
        case disconnected(String)
    }

    @Published public private(set) var state: State = .offline

    public init() {}

    private let intentLock = NSLock()
    private var pendingCarrier: LinkConnection?
    private var pendingDisconnect = false

    // Touched only on the emulation thread (inside `pump`), except `shutdown`.
    private var carrier: LinkConnection?
    private var configured = false

    // Advertising side. Owned here; its accept handler records a carrier intent.
    private var listener: NWListener?
    private static let controlQueue = DispatchQueue(label: "com.setimmediate.link.control")

    // --- UI thread ------------------------------------------------------------

    /// Advertise this device over Bonjour and wait for a peer to join. This unit becomes
    /// the parent (id 0) when a peer connects.
    public func host(rom: String, console: String) {
        LinkNotifier.requestAuthIfNeeded()
        stopListener()
        guard let listener = try? NWListener(using: LinkConnection.hostParams()) else {
            publish(.disconnected("Couldn't start hosting"))
            return
        }
        let txt = NWTXTRecord([LinkService.txtConsole: console,
                               LinkService.txtVersion: String(EmulatorCore.coreVersion),
                               LinkService.txtRom: rom])
        listener.service = NWListener.Service(name: LinkService.deviceName, type: LinkService.type, txtRecord: txt)
        listener.newConnectionHandler = { [weak self] connection in
            guard let self else { connection.cancel(); return }
            // Two-unit only: adopt the first peer as parent (id 0) and stop advertising.
            self.setCarrierIntent(LinkConnection(connection: connection, id: 0))
            self.stopListener()
        }
        listener.stateUpdateHandler = { [weak self] state in
            if case .failed = state { self?.publish(.disconnected("Hosting failed")) }
        }
        self.listener = listener
        listener.start(queue: Self.controlQueue)
        publish(.hosting)
    }

    /// Join a peer discovered over Bonjour. This unit becomes the child (id 1).
    public func join(_ peer: DiscoveredPeer) {
        LinkNotifier.requestAuthIfNeeded()
        stopListener()
        setCarrierIntent(LinkConnection(endpoint: peer.endpoint, id: 1))
        publish(.connecting)
    }

    /// Connect to a dev TCP host (`rewinds --link listen <port>`). This unit is the child.
    public func connectDev(host: String, port: UInt16) {
        stopListener()
        setCarrierIntent(LinkConnection(host: host, port: port, id: 1))
        publish(.connecting)
    }

    /// Request teardown of the current link (and stop advertising).
    public func disconnect() {
        stopListener()
        intentLock.lock()
        pendingDisconnect = true
        pendingCarrier = nil
        intentLock.unlock()
    }

    /// Tear the carrier down synchronously. Call only once the emulation thread has stopped
    /// (from `EmulatorSession.teardown`), so there is no concurrent `pump`.
    func shutdown() {
        stopListener()
        carrier?.cancel()
        carrier = nil
        configured = false
    }

    private func setCarrierIntent(_ connection: LinkConnection) {
        intentLock.lock()
        pendingCarrier = connection
        pendingDisconnect = false
        intentLock.unlock()
    }

    private func stopListener() {
        listener?.cancel()
        listener = nil
    }

    // --- Emulation thread (under the core lock) -------------------------------

    /// Before the frame: apply pending intents, reconcile link config, and deliver any
    /// inbound frames so the guest sees peers' data when it polls SIO this frame.
    func receive(_ core: EmulatorCore) {
        applyIntents(core)
        guard let carrier else { return }
        switch carrier.currentStatus() {
        case .connecting:
            break
        case .ready:
            if !configured {
                core.setLinkConfig(connected: true, id: carrier.id, count: carrier.count)
                configured = true
                publish(.linked)
                LinkNotifier.notify("RewinDS linked", "Connected for multiplayer.")
            }
            for frame in carrier.poll() { core.linkDeliver(frame) }
        case .failed(let message):
            let wasLinked = configured
            teardown(core)
            publish(.disconnected(message))
            if wasLinked { LinkNotifier.notify("Link disconnected", message) }
        case .closed:
            let wasLinked = configured
            teardown(core)
            publish(.disconnected("Link closed"))
            if wasLinked { LinkNotifier.notify("Link disconnected", "The other player left.") }
        }
    }

    /// After the frame: ship whatever serial frames it produced, so an outbound frame goes
    /// out the same frame the guest generated it — matching the desktop host's ordering
    /// (deliver before `run_frame`, poll-out after) and minimizing link round-trip latency.
    func transmit(_ core: EmulatorCore) {
        guard let carrier, configured else { return }
        while let frame = core.linkPollOut() { carrier.send(frame) }
    }

    private func applyIntents(_ core: EmulatorCore) {
        intentLock.lock()
        let newCarrier = pendingCarrier; pendingCarrier = nil
        let disconnect = pendingDisconnect; pendingDisconnect = false
        intentLock.unlock()

        if disconnect {
            teardown(core)
            publish(.offline)
        }
        if let newCarrier {
            teardown(core)
            carrier = newCarrier
        }
    }

    private func teardown(_ core: EmulatorCore) {
        carrier?.cancel()
        carrier = nil
        if configured {
            core.setLinkConfig(connected: false, id: 0, count: 1)
            configured = false
        }
    }

    private func publish(_ newState: State) {
        DispatchQueue.main.async { [weak self] in self?.state = newState }
    }
}

extension LinkConnection {
    /// Parameters for the advertising listener (TCP, peer-to-peer enabled).
    static func hostParams() -> NWParameters { params() }
}
