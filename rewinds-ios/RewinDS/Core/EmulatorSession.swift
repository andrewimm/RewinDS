import SwiftUI
import QuartzCore

/// Drives one running game. The emulator runs on its own thread (a ~60 Hz paced loop),
/// not the main thread, so UI or main-thread hitches can't starve the audio ring or the
/// frame cadence. Each tick applies input, advances one frame, and hands the presented
/// screens to their Metal views (which draw on their own display link).
///
/// # Threading
/// The core is `Send` but not `Sync`: **all** core access is serialized by `coreLock`
/// (the emulation thread's tick, plus the occasional main-thread op — save, lid). Input
/// and the screen-view table are guarded by `stateLock`. The lock order is always
/// `stateLock` (released) → `coreLock`; the Metal view's own lock is taken *inside*
/// `coreLock` and never the other way, so there's no cycle.
final class EmulatorSession: NSObject, ObservableObject {
    let core: EmulatorCore
    let romId: String
    var console: Console { core.console }

    let input = InputState()

    /// Serial-link controller. Drives frame exchange with a peer carrier from `tick`, and
    /// publishes link state for the UI. Idle (a no-op each tick) until a link is requested.
    let link = LinkController()

    private let saveStore: SaveStore
    private var audio: AudioEngine?

    private let coreLock = NSLock()
    private let stateLock = NSLock()

    private var thread: Thread?
    private var alive = false
    private var frameCounter: UInt64 = 0

    /// Reasons the emulator is currently paused. The run loop advances only when the set
    /// is empty, so independent causes compose correctly — e.g. backgrounding while the
    /// in-game menu is open won't resume the game when you return until you close the menu.
    /// Guarded by `stateLock`.
    private struct PauseReason: OptionSet {
        let rawValue: Int
        static let lifecycle = PauseReason(rawValue: 1 << 0) // app inactive/background
        static let menu = PauseReason(rawValue: 1 << 1)      // in-game menu open
    }
    private var pauseReasons: PauseReason = []

    /// Weak references to the screen views, keyed by index, so the view hierarchy owns
    /// their lifetime and the session just borrows them each tick. Guarded by `stateLock`.
    private final class WeakView { weak var view: EmulatorMetalView? }
    private var screens: [Int: WeakView] = [:]

    init(core: EmulatorCore, romId: String, saveStore: SaveStore = .shared) {
        self.core = core
        self.romId = romId
        self.saveStore = saveStore
        super.init()
    }

    // --- Lifecycle (main thread) ---------------------------------------------

    func start() {
        guard thread == nil else { return }
        // No emulation thread yet, so these core touches need no lock.
        saveStore.restore(into: core, romId: romId)

        // Resample once, straight to the actual output hardware rate.
        let outputRate = AudioEngine.prepareSession()
        if let consumer = core.enableAudio(rate: UInt32(outputRate), channels: 2) {
            let engine = AudioEngine(consumer: consumer, sampleRate: outputRate)
            engine.start()
            audio = engine
        }

        lockOrientation()

        alive = true
        let t = Thread { [self] in runLoop() }
        t.name = "rewinds.emulator"
        t.qualityOfService = .userInteractive
        t.stackSize = 4 << 20 // 4 MB, comfortably above the default for a deep emulator.
        thread = t
        t.start()
    }

    /// Tear down for good (leaving the game): stop the thread, flush, stop audio, unlock.
    func teardown() {
        stateLock.lock(); alive = false; stateLock.unlock()
        // Acquiring coreLock blocks until any in-flight tick finishes; with `alive` false
        // the loop won't start another, so the core is ours after this point.
        coreLock.lock()
        saveStore.flushIfDirty(core, romId: romId)
        coreLock.unlock()
        // The run loop has stopped, so no `pump` can race this teardown of the carrier.
        link.shutdown()
        audio?.stop()
        audio = nil
        thread = nil
        OrientationLock.unlock()
    }

    /// Add or clear a pause reason, pausing/resuming audio on the empty↔non-empty edge.
    private func setPause(_ reason: PauseReason, _ on: Bool) {
        stateLock.lock()
        let wasPaused = !pauseReasons.isEmpty
        if on { pauseReasons.insert(reason) } else { pauseReasons.remove(reason) }
        let nowPaused = !pauseReasons.isEmpty
        stateLock.unlock()
        if nowPaused && !wasPaused { audio?.pause() }
        else if !nowPaused && wasPaused { audio?.resume() }
    }

    /// App backgrounded: close the DS lid (sleep), flush, and pause.
    func enterBackground() {
        setPause(.lifecycle, true)
        coreLock.lock()
        core.setLid(closed: true)
        saveStore.flushIfDirty(core, romId: romId)
        coreLock.unlock()
    }

    /// App lost focus without backgrounding (app switcher, Control Center, a system
    /// prompt): freeze and silence, keeping all state exactly as-is. Lighter than
    /// `enterBackground` — no lid sleep, no flush — so a quick peek resumes instantly.
    func pause() {
        setPause(.lifecycle, true)
    }

    /// App foregrounded: wake the lid and clear the lifecycle pause (the game stays paused
    /// if the in-game menu is still up).
    func enterForeground() {
        guard thread != nil else { return }
        coreLock.lock()
        core.setLid(closed: false)
        coreLock.unlock()
        setPause(.lifecycle, false)
    }

    /// Open/close the in-game menu, pausing the game while it's up.
    func openMenu() { setPause(.menu, true) }
    func closeMenu() { setPause(.menu, false) }

    /// The current frame count (for the dev HUD); takes the core lock so it's race-free.
    func currentFrame() -> UInt64 {
        coreLock.lock(); defer { coreLock.unlock() }
        return core.frame
    }

    // --- Input (main thread) --------------------------------------------------

    /// Replace the whole held-button mask (the multitouch controller reports this each
    /// touch event).
    func setButtons(_ mask: UInt32) {
        stateLock.lock(); input.buttons = mask; stateLock.unlock()
    }

    /// Set the DS touchscreen from a lower-screen touch in native pixels, or clear it.
    func setTouch(_ pixel: CGPoint?) {
        stateLock.lock()
        if let p = pixel {
            input.touchX = Int16(clamping: Int(p.x.rounded()))
            input.touchY = Int16(clamping: Int(p.y.rounded()))
            input.touchPressed = true
        } else {
            input.touchPressed = false
        }
        stateLock.unlock()
    }

    // --- Screen registration (main thread) ------------------------------------

    func registerScreen(_ view: EmulatorMetalView, at index: Int) {
        stateLock.lock()
        let box = screens[index] ?? WeakView()
        box.view = view
        screens[index] = box
        stateLock.unlock()
    }

    // --- Emulation thread -----------------------------------------------------

    private func runLoop() {
        let interval = 1.0 / 60.0
        var next = CACurrentMediaTime()
        while true {
            stateLock.lock()
            let live = alive
            let isPaused = !pauseReasons.isEmpty
            stateLock.unlock()
            guard live else { break }

            if !isPaused {
                autoreleasepool { tick() }
            }

            next += interval
            let now = CACurrentMediaTime()
            if next > now {
                Thread.sleep(forTimeInterval: next - now)
            } else {
                next = now // fell behind; resync rather than spiral
            }
        }
    }

    private func tick() {
        // Snapshot input + screen views under stateLock, then touch the core under coreLock.
        stateLock.lock()
        let buttons = input.buttons
        let touchX = input.touchX
        let touchY = input.touchY
        let touchPressed = input.touchPressed
        let views = (0..<console.screenCount).map { screens[$0]?.view }
        stateLock.unlock()

        coreLock.lock()
        core.setInput(buttons: buttons, touchX: touchX, touchY: touchY, touchPressed: touchPressed)
        // Shuttle serial-link frames with any connected peer before advancing the frame, so
        // inbound frames land before the guest polls SIO this frame.
        link.pump(core)
        core.runFrame()
        for (i, view) in views.enumerated() {
            guard let view else { continue }
            core.withScreen(i) { buf in
                view.present(width: buf.width, height: buf.height, pixels: buf.pixels)
            }
        }
        frameCounter &+= 1
        if frameCounter % 30 == 0 {
            saveStore.flushIfDirty(core, romId: romId)
        }
        coreLock.unlock()
    }

    private func lockOrientation() {
        switch console {
        case .gba: OrientationLock.landscape()
        case .nds: OrientationLock.portrait()
        }
    }
}
