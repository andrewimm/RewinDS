import Foundation
import Network
import Combine

/// A pluggable serial-link carrier. Mirrors the Rust reference host's `Transport` trait
/// (`crates/rewinds/src/link.rs`): `send` queues one opaque frame; `poll` returns whole
/// frames that have arrived. The emulator core is carrier-blind — it only produces and
/// consumes these opaque `LinkFrame` bytes, and neither end knows how they travel.
protocol LinkCarrier: AnyObject {
    func send(_ frame: Data)
    func poll() -> [Data]
}

/// A two-unit TCP link carrier over `Network.framework`, wire-compatible with the Rust
/// reference host's `TcpLink`: each frame is length-prefixed with a single byte
/// (`[len][payload…]`). This unit connects *out* to a parent, so it is always the child
/// (id 1); the parent (the Rust `--link listen` host, or a future advertising peer) is
/// id 0.
///
/// # Threading
/// The `NWConnection` runs on its own dispatch queue; its callbacks touch only this
/// carrier's `lock`-guarded state and never the emulator core. `send`, `poll`, and
/// `currentStatus` are safe to call from the emulation thread.
final class TcpCarrier: LinkCarrier {
    enum Status: Equatable {
        case connecting
        case ready
        case failed(String)
        case closed
    }

    let id: UInt8 = 1     // the connecting unit is the child
    let count: UInt8 = 2  // two-unit link for this cut

    private let connection: NWConnection
    private let queue = DispatchQueue(label: "com.setimmediate.link.tcp")

    private let lock = NSLock()
    private var status: Status = .connecting
    private var inbox: [Data] = []   // complete frames awaiting poll()
    private var inbuf = [UInt8]()    // partial receive buffer

    init(host: String, port: UInt16) {
        let tcp = NWProtocolTCP.Options()
        tcp.noDelay = true
        connection = NWConnection(
            host: NWEndpoint.Host(host),
            port: NWEndpoint.Port(rawValue: port) ?? .any,
            using: NWParameters(tls: nil, tcp: tcp))
        connection.stateUpdateHandler = { [weak self] state in self?.onState(state) }
        connection.start(queue: queue)
        receiveNext()
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
            // deliberate dev connect, surface it as a failure rather than retrying forever.
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
/// lockstep with the run loop.
///
/// # Threading
/// The carrier's entire lifecycle lives on the emulation thread: the UI only records an
/// *intent* (connect/disconnect) under `intentLock`, and `pump(_:)` — called from
/// `EmulatorSession.tick()` under the core lock — applies it, creates or tears down the
/// carrier, and reconciles the core's link config. So the core is only ever touched from
/// its owning thread. `state` is published for the UI and updated on the main queue.
final class LinkController: ObservableObject {
    enum State: Equatable {
        case offline
        case connecting
        case linked
        case disconnected(String)
    }

    @Published private(set) var state: State = .offline

    private let intentLock = NSLock()
    private var pendingConnect: (host: String, port: UInt16)?
    private var pendingDisconnect = false

    // Touched only on the emulation thread (inside `pump`), except `shutdown`.
    private var carrier: TcpCarrier?
    private var configured = false

    // --- UI thread ------------------------------------------------------------

    /// Request a connection to a parent host (the Rust `--link listen` reference host, or a
    /// future advertising peer). Applied on the next emulator tick.
    func connect(host: String, port: UInt16) {
        intentLock.lock()
        pendingConnect = (host, port)
        pendingDisconnect = false
        intentLock.unlock()
        publish(.connecting)
    }

    /// Request teardown of the current link.
    func disconnect() {
        intentLock.lock()
        pendingDisconnect = true
        pendingConnect = nil
        intentLock.unlock()
    }

    /// Tear the carrier down synchronously. Call only once the emulation thread has stopped
    /// (from `EmulatorSession.teardown`), so there is no concurrent `pump`.
    func shutdown() {
        carrier?.cancel()
        carrier = nil
        configured = false
    }

    // --- Emulation thread (under the core lock) -------------------------------

    /// Apply any pending intent, then shuttle frames between the core and the carrier.
    func pump(_ core: EmulatorCore) {
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
            }
            // Deliver inbound frames, then drain outbound — mirroring the reference host's
            // per-frame pump.
            for frame in carrier.poll() { core.linkDeliver(frame) }
            while let frame = core.linkPollOut() { carrier.send(frame) }
        case .failed(let message):
            teardown(core)
            publish(.disconnected(message))
        case .closed:
            teardown(core)
            publish(.disconnected("Link closed"))
        }
    }

    private func applyIntents(_ core: EmulatorCore) {
        intentLock.lock()
        let connect = pendingConnect; pendingConnect = nil
        let disconnect = pendingDisconnect; pendingDisconnect = false
        intentLock.unlock()

        if disconnect {
            teardown(core)
            publish(.offline)
        }
        if let connect {
            teardown(core)
            carrier = TcpCarrier(host: connect.host, port: connect.port)
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
