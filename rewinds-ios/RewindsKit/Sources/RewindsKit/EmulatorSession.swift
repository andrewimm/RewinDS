import Foundation
import QuartzCore
import Combine

/// Drives one running game. The emulator runs on its own thread (a ~60 Hz paced loop),
/// not the main thread, so UI or main-thread hitches can't starve the audio ring or the
/// frame cadence. Each tick applies input, advances one frame, and hands the presented
/// screens to their `ScreenSink`s (Metal views that draw on their own display link).
///
/// # Threading
/// The core is `Send` but not `Sync`: **all** core access is serialized by `coreLock`
/// (the emulation thread's tick, plus the occasional main-thread op — save, lid). Input
/// and the screen-sink table are guarded by `stateLock`. The lock order is always
/// `stateLock` (released) → `coreLock`; the Metal view's own lock is taken *inside*
/// `coreLock` and never the other way, so there's no cycle.
public final class EmulatorSession: NSObject, ObservableObject {
    public let core: EmulatorCore
    public let romId: String
    public var console: Console { core.console }

    public let input = InputState()

    /// Serial-link controller. Drives frame exchange with a peer carrier from `tick`, and
    /// publishes link state for the UI. Idle (a no-op each tick) until a link is requested.
    public let link = LinkController()

    private let saveStore = SaveStore.shared
    private var audio: AudioEngine?

    private let coreLock = NSLock()
    private let stateLock = NSLock()

    private var thread: Thread?
    private var alive = false
    private var frameCounter: UInt64 = 0
    /// Set on a resume edge so the run loop reseeds its pacing clock to "now" on the next
    /// tick — a screen-off can otherwise leave the fixed-cadence clock mispaced. Guarded by
    /// `stateLock`.
    private var reseedClock = false

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

    /// Fast-forward ("warp"): while set, a tick runs the core unthrottled for ~a display
    /// frame's worth of real time and mutes audio, presenting only the final frame. Guarded
    /// by `stateLock`.
    private var warp = false

    /// Weak references to the screen sinks, keyed by index, so the view hierarchy owns
    /// their lifetime and the session just borrows them each tick. Guarded by `stateLock`.
    private final class WeakSink { weak var sink: ScreenSink? }
    private var screens: [Int: WeakSink] = [:]

    public init(core: EmulatorCore, romId: String) {
        self.core = core
        self.romId = romId
        super.init()
    }

    // --- Lifecycle (main thread) ---------------------------------------------

    public func start() {
        guard thread == nil else { return }
        // No emulation thread yet, so these core touches need no lock.
        saveStore.restore(into: core, romId: romId)

        // Seed the RTC with the device's local time so day/night and timed events match the
        // player's real clock. Calendar's weekday is 1=Sunday..7; the core wants 0=Sunday.
        let c = Calendar.current.dateComponents(
            [.year, .month, .day, .weekday, .hour, .minute, .second], from: Date())
        core.setClock(
            year: UInt16(c.year ?? 2024), month: UInt8(c.month ?? 1), day: UInt8(c.day ?? 1),
            weekday: UInt8((c.weekday ?? 1) - 1), hour: UInt8(c.hour ?? 0),
            minute: UInt8(c.minute ?? 0), second: UInt8(c.second ?? 0))

        // Resample once, straight to the actual output hardware rate.
        let outputRate = AudioEngine.prepareSession()
        if let consumer = core.enableAudio(rate: UInt32(outputRate), channels: 2) {
            let engine = AudioEngine(consumer: consumer, sampleRate: outputRate)
            engine.start()
            audio = engine
        }

        alive = true
        let t = Thread { [self] in runLoop() }
        t.name = "rewinds.emulator"
        t.qualityOfService = .userInteractive
        t.stackSize = 4 << 20 // 4 MB, comfortably above the default for a deep emulator.
        thread = t
        t.start()
    }

    /// Tear down for good (leaving the game): stop the thread, flush, stop audio.
    public func teardown() {
        stateLock.lock(); alive = false; stateLock.unlock()
        // Acquiring coreLock blocks until any in-flight tick finishes; with `alive` false
        // the loop won't start another, so the core is ours after this point.
        coreLock.lock()
        flushSaveWhenComplete()
        coreLock.unlock()
        // The run loop has stopped, so no `pump` can race this teardown of the carrier.
        link.shutdown()
        audio?.stop()
        audio = nil
        thread = nil
    }

    /// Add or clear a pause reason, pausing/resuming audio on the empty↔non-empty edge.
    private func setPause(_ reason: PauseReason, _ on: Bool) {
        stateLock.lock()
        let wasPaused = !pauseReasons.isEmpty
        if on { pauseReasons.insert(reason) } else { pauseReasons.remove(reason) }
        let nowPaused = !pauseReasons.isEmpty
        let resumed = wasPaused && !nowPaused
        if resumed { reseedClock = true }
        stateLock.unlock()
        if nowPaused && !wasPaused {
            audio?.pause()
        } else if resumed {
            audio?.resume()
            // Re-establish presentation: a screen-off can stall a view's display link, so
            // the game keeps running but stops drawing until the view is nudged. Doing this
            // on every resume makes foregrounding recover automatically — the same recovery
            // that opening and closing the menu triggers by hand.
            resumeDisplays()
        }
    }

    /// Kick every registered screen to re-establish its display link, on the main thread.
    private func resumeDisplays() {
        stateLock.lock()
        let sinks = screens.values.compactMap { $0.sink }
        stateLock.unlock()
        DispatchQueue.main.async {
            for sink in sinks { sink.displayResumed() }
        }
    }

    /// App backgrounded: close the DS lid (sleep), flush, and pause.
    public func enterBackground() {
        setPause(.lifecycle, true)
        coreLock.lock()
        // Let any in-progress save finish and persist it BEFORE closing the lid — closing
        // the lid sleeps the game, which would freeze a half-written save.
        flushSaveWhenComplete()
        core.setLid(closed: true)
        coreLock.unlock()
    }

    /// App lost focus without backgrounding (app switcher, Control Center, a system
    /// prompt): freeze and silence, keeping all state exactly as-is. Lighter than
    /// `enterBackground` — no lid sleep, no flush — so a quick peek resumes instantly.
    public func pause() {
        setPause(.lifecycle, true)
    }

    /// App foregrounded: wake the lid and clear the lifecycle pause (the game stays paused
    /// if the in-game menu is still up).
    public func enterForeground() {
        guard thread != nil else { return }
        coreLock.lock()
        core.setLid(closed: false)
        coreLock.unlock()
        setPause(.lifecycle, false)
    }

    /// Open/close the in-game menu, pausing the game while it's up.
    public func openMenu() { setPause(.menu, true) }
    public func closeMenu() { setPause(.menu, false) }

    /// The current frame count (for the dev HUD); takes the core lock so it's race-free.
    public func currentFrame() -> UInt64 {
        coreLock.lock(); defer { coreLock.unlock() }
        return core.frame
    }

    // --- Input (main thread) --------------------------------------------------

    /// Replace the whole held-button mask (the controller reports this each input event).
    public func setButtons(_ mask: UInt32) {
        stateLock.lock(); input.buttons = mask; stateLock.unlock()
    }

    /// Enable/disable fast-forward. Typically bound to a held key (Space), matching the
    /// desktop host: the emulator runs unthrottled and audio is muted while it's on.
    public func setWarp(_ on: Bool) {
        stateLock.lock(); warp = on; stateLock.unlock()
    }

    /// Set the DS touchscreen from a lower-screen touch in native pixels, or clear it.
    public func setTouch(_ pixel: CGPoint?) {
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

    public func registerScreen(_ sink: ScreenSink, at index: Int) {
        stateLock.lock()
        let box = screens[index] ?? WeakSink()
        box.sink = sink
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
            let reseed = reseedClock
            if reseed { reseedClock = false }
            stateLock.unlock()
            guard live else { break }

            // After a resume, restart the fixed-cadence clock from now so a long screen-off
            // can't leave it drifted.
            if reseed { next = CACurrentMediaTime() }

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
        // Snapshot input + screen sinks under stateLock, then touch the core under coreLock.
        stateLock.lock()
        let buttons = input.buttons
        let touchX = input.touchX
        let touchY = input.touchY
        let touchPressed = input.touchPressed
        let warpHeld = warp
        let sinks = (0..<console.screenCount).map { screens[$0]?.sink }
        stateLock.unlock()

        coreLock.lock()
        core.setInput(buttons: buttons, touchX: touchX, touchY: touchY, touchPressed: touchPressed)
        // Reconcile the link (apply connect/disconnect intents, set config on ready). When a
        // link is up, drive the frame under the transfer barrier; fast-forward is disabled
        // then (it would desync the two machines).
        let linked = link.reconcile(core)
        let warping = warpHeld && !linked
        core.setAudioMuted(warping)
        if linked {
            link.runLinkedFrame(core)
        } else if warping {
            // Fast-forward: run unthrottled for ~a display frame's worth of real time, then
            // present only the final frame (audio muted above) — matching the desktop host.
            let deadline = CACurrentMediaTime() + 0.014
            repeat { core.runFrame() } while CACurrentMediaTime() < deadline
        } else {
            core.runFrame()
        }
        for (i, sink) in sinks.enumerated() {
            guard let sink else { continue }
            core.withScreen(i) { buf in
                sink.present(width: buf.width, height: buf.height, pixels: buf.pixels)
            }
        }
        frameCounter &+= 1
        // Track whether the game is still writing its save. A DS save is a large,
        // multi-frame FLASH write; persisting it mid-write stores a half-written save
        // (e.g. a Pokémon HG/SS save with only one of its two mirror copies, which the
        // game then rejects). Only flush once the backup write-epoch has held steady for
        // a short while — the save routine has finished.
        let epoch = core.saveWriteEpoch
        if epoch != lastSaveEpoch { lastSaveEpoch = epoch; saveQuiescentFrames = 0 }
        else { saveQuiescentFrames &+= 1 }
        if frameCounter % 30 == 0 && saveQuiescentFrames >= Self.saveQuiescentFrames {
            saveStore.flushIfDirty(core, romId: romId)
        }
        coreLock.unlock()
    }

    /// Frames the save backup must stay un-written before we treat a save as finished.
    private static let saveQuiescentFrames: UInt64 = 20
    private var lastSaveEpoch: UInt64 = 0
    private var saveQuiescentFrames: UInt64 = 0

    /// Persist a COMPLETE save: if the game is mid-save, step the core until its save
    /// routine finishes (the write-epoch holds steady) before flushing, so a save forced
    /// by backgrounding/teardown is never truncated. Must be called holding `coreLock`.
    private func flushSaveWhenComplete() {
        guard core.saveDirty else { return }
        var prev = core.saveWriteEpoch
        var steady: UInt64 = 0
        var guardCount = 0
        while steady < Self.saveQuiescentFrames && guardCount < 600 {
            core.runFrame()
            let e = core.saveWriteEpoch
            if e != prev { prev = e; steady = 0 } else { steady &+= 1 }
            guardCount += 1
        }
        saveStore.flushIfDirty(core, romId: romId)
    }
}
