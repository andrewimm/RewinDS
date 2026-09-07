import SwiftUI
import MetalKit

/// An `MTKView` that blits one emulator screen. The run loop hands it a freshly
/// presented RGBA8 buffer via `present`, which uploads it into a source texture and
/// draws immediately — the view is otherwise paused, so it renders exactly in lockstep
/// with the emulator rather than on its own clock.
final class EmulatorMetalView: MTKView {
    private var queue: MTLCommandQueue?
    private var pipeline: MTLRenderPipelineState?
    private var texture: MTLTexture?
    private var texWidth = 0
    private var texHeight = 0

    // The latest frame the run loop handed us, copied and drawn on the view's own tick.
    // Written by the emulation thread (`present`) and read by the main thread (`draw`),
    // so all access is guarded by `frameLock`.
    private let frameLock = NSLock()
    private var latest = [UInt8]()
    private var latestWidth = 0
    private var latestHeight = 0

    /// When set (the DS lower screen), reports touch location as native screen pixels,
    /// or `nil` on release.
    var onTouch: ((_ pixel: CGPoint?) -> Void)?

    init() {
        super.init(frame: .zero, device: MTLCreateSystemDefaultDevice())
        commonInit()
    }

    required init(coder: NSCoder) {
        super.init(coder: coder)
        commonInit()
    }

    private func commonInit() {
        // Let the view run its own display link and render the latest uploaded frame in
        // its managed `draw(_:)` cycle — the canonical MTKView pattern. Presenting a
        // drawable manually off this cycle leaves rapidly-changing content on black.
        isPaused = false
        enableSetNeedsDisplay = false
        preferredFramesPerSecond = 60
        framebufferOnly = true
        colorPixelFormat = .bgra8Unorm
        // A DS shows its backdrop as black when idle; match that behind the quad.
        clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)
        layer.magnificationFilter = .nearest

        guard let device else { return }
        queue = device.makeCommandQueue()

        guard let library = try? device.makeDefaultLibrary(bundle: .main),
              let vfn = library.makeFunction(name: "screen_vertex"),
              let ffn = library.makeFunction(name: "screen_fragment")
        else { return }

        let desc = MTLRenderPipelineDescriptor()
        desc.vertexFunction = vfn
        desc.fragmentFunction = ffn
        desc.colorAttachments[0].pixelFormat = colorPixelFormat
        pipeline = try? device.makeRenderPipelineState(descriptor: desc)
    }

    /// Hand the view the latest presented frame (`width * height` RGBA8, borrowed only
    /// for this call). It's copied and drawn on the view's next display tick.
    func present(width: Int, height: Int, pixels: UnsafePointer<UInt8>) {
        guard width > 0, height > 0 else { return }
        let count = width * height * 4
        frameLock.lock()
        if latest.count != count { latest = [UInt8](repeating: 0, count: count) }
        latest.withUnsafeMutableBytes { dst in
            dst.baseAddress!.copyMemory(from: pixels, byteCount: count)
        }
        latestWidth = width
        latestHeight = height
        frameLock.unlock()
    }

    /// Render the latest frame. Called by the view's own display link each vsync.
    override func draw(_ rect: CGRect) {
        guard let queue, let pipeline else { return }

        // Upload the latest frame into the source texture under the lock (the emulation
        // thread may be writing it concurrently), then render outside the lock.
        frameLock.lock()
        let w = latestWidth, h = latestHeight
        guard w > 0, h > 0 else { frameLock.unlock(); return }
        if texture == nil || texWidth != w || texHeight != h {
            let d = MTLTextureDescriptor.texture2DDescriptor(
                pixelFormat: .rgba8Unorm, width: w, height: h, mipmapped: false)
            d.usage = .shaderRead
            texture = device?.makeTexture(descriptor: d)
            texWidth = w
            texHeight = h
        }
        if let texture {
            latest.withUnsafeBytes { src in
                texture.replace(
                    region: MTLRegionMake2D(0, 0, w, h),
                    mipmapLevel: 0,
                    withBytes: src.baseAddress!,
                    bytesPerRow: w * 4)
            }
        }
        frameLock.unlock()

        guard texture != nil,
              let rpd = currentRenderPassDescriptor,
              let drawable = currentDrawable,
              let cmd = queue.makeCommandBuffer(),
              let enc = cmd.makeRenderCommandEncoder(descriptor: rpd)
        else { return }
        enc.setRenderPipelineState(pipeline)
        enc.setFragmentTexture(texture, index: 0)
        enc.drawPrimitives(type: .triangleStrip, vertexStart: 0, vertexCount: 4)
        enc.endEncoding()
        cmd.present(drawable)
        cmd.commit()
    }

    // --- Touch (DS lower screen only) ----------------------------------------

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) { reportTouch(touches) }
    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) { reportTouch(touches) }
    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) { onTouch?(nil) }
    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) { onTouch?(nil) }

    private func reportTouch(_ touches: Set<UITouch>) {
        guard onTouch != nil, let t = touches.first, bounds.width > 0, bounds.height > 0 else { return }
        let p = t.location(in: self)
        let fx = min(max(p.x / bounds.width, 0), 1)
        let fy = min(max(p.y / bounds.height, 0), 1)
        onTouch?(CGPoint(x: fx * CGFloat(max(texWidth, 1)), y: fy * CGFloat(max(texHeight, 1))))
    }
}

/// SwiftUI wrapper. Registers the underlying view with the session for its screen index
/// so the run loop can push frames to it; wires touch for the DS lower screen.
struct MetalScreenView: UIViewRepresentable {
    let session: EmulatorSession
    let index: Int
    /// The DS lower screen forwards touches as the console's touchscreen input.
    var isTouchScreen: Bool = false

    func makeUIView(context: Context) -> EmulatorMetalView {
        let view = EmulatorMetalView()
        if isTouchScreen {
            view.isMultipleTouchEnabled = false
            view.onTouch = { [weak session] pixel in
                session?.setTouch(pixel)
            }
        } else {
            view.isUserInteractionEnabled = false
        }
        session.registerScreen(view, at: index)
        return view
    }

    func updateUIView(_ uiView: EmulatorMetalView, context: Context) {}

    static func dismantleUIView(_ uiView: EmulatorMetalView, coordinator: ()) {
        uiView.onTouch = nil
    }
}
