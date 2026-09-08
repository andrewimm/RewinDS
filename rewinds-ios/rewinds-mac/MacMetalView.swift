import SwiftUI
import MetalKit
import RewindsKit

/// An `MTKView` that blits one emulator screen on macOS. The renderer mirrors the iOS
/// `EmulatorMetalView` exactly (Metal is identical across platforms); only input differs —
/// the mouse drives the DS touch screen, and keyboard is handled at the SwiftUI layer.
final class MacEmulatorMetalView: MTKView, ScreenSink {
    private var queue: MTLCommandQueue?
    private var pipeline: MTLRenderPipelineState?
    private var texture: MTLTexture?
    private var texWidth = 0
    private var texHeight = 0

    private let frameLock = NSLock()
    private var latest = [UInt8]()
    private var latestWidth = 0
    private var latestHeight = 0

    /// When set (the DS lower screen), reports mouse location as native screen pixels, or
    /// `nil` on release.
    var onTouch: ((CGPoint?) -> Void)?

    init() {
        super.init(frame: .zero, device: MTLCreateSystemDefaultDevice())
        commonInit()
    }

    required init(coder: NSCoder) {
        super.init(coder: coder)
        commonInit()
    }

    // Top-left origin so mouse coordinates match the texture/screen space (and iOS).
    override var isFlipped: Bool { true }

    private func commonInit() {
        isPaused = false
        enableSetNeedsDisplay = false
        preferredFramesPerSecond = 60
        framebufferOnly = true
        colorPixelFormat = .bgra8Unorm
        clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)
        wantsLayer = true
        layer?.magnificationFilter = .nearest

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

    override func draw(_ rect: CGRect) {
        guard let queue, let pipeline else { return }

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

    // --- Mouse → touch (DS lower screen only) --------------------------------

    override func mouseDown(with event: NSEvent) { reportMouse(event) }
    override func mouseDragged(with event: NSEvent) { reportMouse(event) }
    override func mouseUp(with event: NSEvent) { onTouch?(nil) }

    private func reportMouse(_ event: NSEvent) {
        guard onTouch != nil, bounds.width > 0, bounds.height > 0 else { return }
        let p = convert(event.locationInWindow, from: nil)
        let fx = min(max(p.x / bounds.width, 0), 1)
        let fy = min(max(p.y / bounds.height, 0), 1)
        onTouch?(CGPoint(x: fx * CGFloat(max(texWidth, 1)), y: fy * CGFloat(max(texHeight, 1))))
    }
}

/// SwiftUI wrapper registering the view with the session for its screen index. The DS
/// lower screen forwards mouse drags as the console's touchscreen input.
struct MacMetalScreenView: NSViewRepresentable {
    let session: EmulatorSession
    let index: Int
    var isTouchScreen: Bool = false

    func makeNSView(context: Context) -> MacEmulatorMetalView {
        let view = MacEmulatorMetalView()
        if isTouchScreen {
            view.onTouch = { [weak session] pixel in session?.setTouch(pixel) }
        }
        session.registerScreen(view, at: index)
        return view
    }

    func updateNSView(_ nsView: MacEmulatorMetalView, context: Context) {}

    static func dismantleNSView(_ nsView: MacEmulatorMetalView, coordinator: ()) {
        nsView.onTouch = nil
    }
}
