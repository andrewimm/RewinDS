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
    // A condition (not a plain lock) so `recvBlocking` can park the emulation thread on
    // the socket at a transfer barrier and be woken the instant a frame or a status change
    // arrives — no busy-polling.
    private let cond = NSCondition()
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
        cond.lock(); defer { cond.unlock() }
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
        cond.lock(); defer { cond.unlock() }
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
        cond.lock()
        inbuf.append(contentsOf: data)
        // Split off every complete length-prefixed frame.
        var gotFrame = false
        while let len = inbuf.first.map(Int.init), inbuf.count >= 1 + len {
            inbox.append(Data(inbuf[1..<(1 + len)]))
            inbuf.removeFirst(1 + len)
            gotFrame = true
        }
        if gotFrame { cond.signal() } // wake a barrier wait in `recvBlocking`
        cond.unlock()
    }

    private func setStatus(_ newStatus: Status) {
        cond.lock()
        status = newStatus
        cond.signal() // wake a barrier wait so a drop/close doesn't wait out the timeout
        cond.unlock()
    }

    /// Block up to `timeout` for at least one frame, **parking** the emulation thread on
    /// the socket rather than spinning — the transfer barrier's wait. Returns whatever
    /// frames arrived (empty on timeout or a dead connection).
    func recvBlocking(timeout: TimeInterval) -> [Data] {
        cond.lock(); defer { cond.unlock() }
        let deadline = Date().addingTimeInterval(timeout)
        while inbox.isEmpty && Self.waitable(status) && Date() < deadline {
            if !cond.wait(until: deadline) { break } // timed out
        }
        let frames = inbox
        inbox.removeAll(keepingCapacity: true)
        return frames
    }

    /// Whether it's still worth waiting on this connection (not failed/closed).
    private static func waitable(_ status: Status) -> Bool {
        switch status {
        case .connecting, .ready: return true
        case .failed, .closed: return false
        }
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

    /// Apply pending intents and reconcile the core's link config. Returns whether a link
    /// is up and this frame should be driven by [`runLinkedFrame`] (the transfer barrier);
    /// `false` means run a normal frame (unlinked, or still connecting).
    func reconcile(_ core: EmulatorCore) -> Bool {
        applyIntents(core)
        guard let carrier else { return false }
        switch carrier.currentStatus() {
        case .connecting:
            return false
        case .ready:
            if !configured {
                core.setLinkConfig(connected: true, id: carrier.id, count: carrier.count)
                configured = true
                publish(.linked)
                LinkNotifier.notify("RewinDS linked", "Connected for multiplayer.")
            }
            return true
        case .failed(let message):
            let wasLinked = configured
            teardown(core)
            publish(.disconnected(message))
            if wasLinked { LinkNotifier.notify("Link disconnected", message) }
            return false
        case .closed:
            let wasLinked = configured
            teardown(core)
            publish(.disconnected("Link closed"))
            if wasLinked { LinkNotifier.notify("Link disconnected", "The other player left.") }
            return false
        }
    }

    /// Drive one video frame under the serial transfer barrier, mirroring the desktop host:
    /// advance in short slices, service the carrier between each, and at every transfer send
    /// our word and **park** for the peer's reply before resuming — so each transfer
    /// completes in-frame (clock-locked) even during the bursts a link session fires within
    /// a single frame. A stalled peer (no reply within the timeout) ends the frame early.
    /// Only called when [`reconcile`] returned `true`; runs on the emulation thread.
    func runLinkedFrame(_ core: EmulatorCore) {
        guard let carrier else { return }
        let sliceCycles: UInt64 = 512
        let barrierTimeout: TimeInterval = 0.05
        while true {
            for frame in carrier.poll() { core.linkDeliver(frame) }
            while let frame = core.linkPollOut() { carrier.send(frame) }
            switch core.runStep(maxCycles: sliceCycles) {
            case .frameComplete:
                return
            case .yielded:
                continue
            case .linkPending:
                while let frame = core.linkPollOut() { carrier.send(frame) }
                let frames = carrier.recvBlocking(timeout: barrierTimeout)
                if frames.isEmpty { return } // peer stalled — bail this frame rather than hang
                for frame in frames { core.linkDeliver(frame) }
            }
        }
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
