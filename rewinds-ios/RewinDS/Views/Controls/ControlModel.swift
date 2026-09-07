import SwiftUI

/// Identifies a control region for the multitouch layer. The d-pad is one region that
/// resolves to a *set* of directions (so a single touch near a corner arms a diagonal,
/// e.g. Up+Left) rather than four separate buttons.
enum ControlKey: Hashable {
    case button(GameButton)
    case dpad
}

/// The live map of control regions, in the controller's coordinate space. Rebuilt from
/// SwiftUI layout each pass and read by the touch overlay — all on the main thread.
final class ControlRegistry {
    private(set) var buttons: [GameButton: CGRect] = [:]
    private(set) var dpad: CGRect = .zero

    /// Fraction of the d-pad half-extent a touch must clear before a direction arms.
    private let deadZone: CGFloat = 0.22

    func apply(_ frames: [ControlFrameEntry]) {
        var buttons: [GameButton: CGRect] = [:]
        var dpad: CGRect = .zero
        for f in frames {
            switch f.key {
            case .button(let b): buttons[b] = f.rect
            case .dpad: dpad = f.rect
            }
        }
        self.buttons = buttons
        self.dpad = dpad
    }

    /// Whether a point lands on any control (used by the overlay to decide whether to own
    /// a touch or let it fall through to a screen behind it).
    func contains(_ p: CGPoint) -> Bool {
        if dpad.contains(p) { return true }
        return buttons.contains { $0.value.contains(p) }
    }

    /// The union of buttons pressed by the given active touch points.
    func mask(for points: [CGPoint]) -> UInt32 {
        var mask: UInt32 = 0
        for p in points {
            for (button, rect) in buttons where rect.contains(p) {
                mask |= button.rawValue
            }
            if dpad.contains(p) {
                mask |= directions(at: p)
            }
        }
        return mask
    }

    private func directions(at p: CGPoint) -> UInt32 {
        guard dpad.width > 0, dpad.height > 0 else { return 0 }
        let nx = (p.x - dpad.midX) / (dpad.width / 2)
        let ny = (p.y - dpad.midY) / (dpad.height / 2)
        var mask: UInt32 = 0
        if nx < -deadZone { mask |= GameButton.left.rawValue }
        if nx > deadZone { mask |= GameButton.right.rawValue }
        if ny < -deadZone { mask |= GameButton.up.rawValue }
        if ny > deadZone { mask |= GameButton.down.rawValue }
        return mask
    }
}

/// One reported control frame.
struct ControlFrameEntry: Equatable {
    let key: ControlKey
    let rect: CGRect
}

/// Collects every control's frame in the controller coordinate space.
struct ControlFramesKey: PreferenceKey {
    static let defaultValue: [ControlFrameEntry] = []
    static func reduce(value: inout [ControlFrameEntry], nextValue: () -> [ControlFrameEntry]) {
        value += nextValue()
    }
}

extension View {
    /// Report this view's frame as a named control region (visuals only — hit testing is
    /// handled entirely by the multitouch overlay).
    func controlRegion(_ key: ControlKey, space: String = ControllerSpace.name) -> some View {
        background(
            GeometryReader { geo in
                Color.clear.preference(
                    key: ControlFramesKey.self,
                    value: [ControlFrameEntry(key: key, rect: geo.frame(in: .named(space)))])
            }
        )
        .allowsHitTesting(false)
    }
}

enum ControllerSpace {
    static let name = "controller"
}

/// A transparent UIKit surface that captures **simultaneous** touches across the whole
/// controller cluster and reports the resulting button mask. It owns a touch only if it
/// began on a control (otherwise the touch passes through to a screen behind it), which
/// keeps the DS touchscreen usable at the same time as the buttons.
struct MultiTouchOverlay: UIViewRepresentable {
    let registry: ControlRegistry
    let onMask: (UInt32) -> Void

    func makeUIView(context: Context) -> MultiTouchView {
        let v = MultiTouchView()
        v.registry = registry
        v.onMask = onMask
        return v
    }

    func updateUIView(_ uiView: MultiTouchView, context: Context) {
        uiView.registry = registry
        uiView.onMask = onMask
    }
}

final class MultiTouchView: UIView {
    var registry: ControlRegistry?
    var onMask: ((UInt32) -> Void)?
    private var active: Set<UITouch> = []

    override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = true
        backgroundColor = .clear
    }

    required init?(coder: NSCoder) {
        super.init(coder: coder)
        isMultipleTouchEnabled = true
        backgroundColor = .clear
    }

    override func hitTest(_ point: CGPoint, with event: UIEvent?) -> UIView? {
        // Only intercept touches that start on a control; let the rest fall through.
        (registry?.contains(point) ?? false) ? self : nil
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        active.formUnion(touches)
        recompute()
    }
    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        recompute()
    }
    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        active.subtract(touches)
        recompute()
    }
    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        active.subtract(touches)
        recompute()
    }

    private func recompute() {
        guard let registry else { return }
        let points = active.map { $0.location(in: self) }
        onMask?(registry.mask(for: points))
    }
}
