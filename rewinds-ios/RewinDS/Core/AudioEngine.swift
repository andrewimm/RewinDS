import AVFoundation

/// Streams the core's audio to the speaker. An `AVAudioSourceNode` render block (on the
/// real-time audio thread) pulls interleaved stereo from the lock-free `AudioConsumer`,
/// deinterleaves it into the engine's standard non-interleaved format, and pads with
/// silence on underrun. The emulator's producer-side resampler keeps the ring near
/// half-full, so playback stays gap-free as long as frames keep running.
///
/// The graph (source node → mixer) is built **once**; background/foreground use
/// `pause`/`resume` rather than rebuilding it, so cycling focus doesn't stack duplicate
/// nodes onto the engine. Audio-route/config changes (which iOS fires across those
/// transitions) restart the engine so playback recovers instead of going silent.
final class AudioEngine {
    let sampleRate: Double
    private let engine = AVAudioEngine()
    private var sourceNode: AVAudioSourceNode?
    private let consumer: AudioConsumer

    /// Scratch for one render's worth of interleaved samples, sized for a generous frame
    /// count so the render block never allocates.
    private let scratchCapacity = 8192
    private let scratch: UnsafeMutablePointer<Float>

    private var configured = false
    private var shouldPlay = false
    private var configObserver: NSObjectProtocol?

    init(consumer: AudioConsumer, sampleRate: Double = 48_000) {
        self.consumer = consumer
        self.sampleRate = sampleRate
        self.scratch = .allocate(capacity: scratchCapacity)
        self.scratch.initialize(repeating: 0, count: scratchCapacity)
    }

    deinit {
        if let configObserver { NotificationCenter.default.removeObserver(configObserver) }
        engine.stop()
        scratch.deinitialize(count: scratchCapacity)
        scratch.deallocate()
    }

    /// Configure the shared audio session for playback and report the actual hardware
    /// output rate, so the emulator can resample once straight to it (no hidden second
    /// conversion through a hardcoded 48 kHz).
    static func prepareSession() -> Double {
        let session = AVAudioSession.sharedInstance()
        try? session.setCategory(.playback, mode: .default, options: [])
        try? session.setActive(true)
        return session.sampleRate
    }

    /// Build the graph (once) and begin playback.
    func start() {
        buildGraphIfNeeded()
        shouldPlay = true
        activateSession()
        startEngine()
    }

    /// Pause playback, keeping the graph intact (unfocus / background).
    func pause() {
        shouldPlay = false
        if engine.isRunning { engine.pause() }
    }

    /// Resume playback after a pause (foreground).
    func resume() {
        shouldPlay = true
        activateSession()
        startEngine()
    }

    /// Fully stop and release the session (leaving the game).
    func stop() {
        shouldPlay = false
        engine.stop()
        try? AVAudioSession.sharedInstance().setActive(false, options: [.notifyOthersOnDeactivation])
    }

    private func buildGraphIfNeeded() {
        guard !configured,
              let format = AVAudioFormat(standardFormatWithSampleRate: sampleRate, channels: 2)
        else { return }

        let cap = scratchCapacity
        let scratch = self.scratch
        let consumer = self.consumer
        let node = AVAudioSourceNode(format: format) { _, _, frameCount, audioBufferList in
            let abl = UnsafeMutableAudioBufferListPointer(audioBufferList)
            let frames = Int(frameCount)
            let wanted = min(frames * 2, cap)
            let got = consumer.read(into: scratch, capacity: wanted)
            let gotFrames = got / 2

            // Deinterleave into the two channel buffers, zero-filling any underrun.
            let left = abl[0].mData!.assumingMemoryBound(to: Float.self)
            let right = abl.count > 1 ? abl[1].mData!.assumingMemoryBound(to: Float.self) : left
            for i in 0..<frames {
                if i < gotFrames {
                    left[i] = scratch[i * 2]
                    right[i] = scratch[i * 2 + 1]
                } else {
                    left[i] = 0
                    right[i] = 0
                }
            }
            return noErr
        }

        engine.attach(node)
        engine.connect(node, to: engine.mainMixerNode, format: format)
        sourceNode = node
        configured = true

        // A route/config change (common when returning from background) stops the engine;
        // restart it if we're meant to be playing so audio recovers rather than dying.
        configObserver = NotificationCenter.default.addObserver(
            forName: .AVAudioEngineConfigurationChange, object: engine, queue: .main
        ) { [weak self] _ in
            guard let self, self.shouldPlay else { return }
            self.activateSession()
            self.startEngine()
        }
    }

    private func startEngine() {
        guard configured, shouldPlay, !engine.isRunning else { return }
        try? engine.start()
    }

    private func activateSession() {
        try? AVAudioSession.sharedInstance().setActive(true)
    }
}
