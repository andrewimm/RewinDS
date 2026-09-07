import Foundation

/// A console button. Raw values are the `REWINDS_BTN_*` bits from the core header
/// (the low ten match the GBA `KEYINPUT` order; X/Y extend it for the DS). The Rust
/// side asserts this ordering at compile time, so these literals can't silently drift.
///
/// Named `GameButton`, not `Button`, to stay clear of SwiftUI's `Button` view.
enum GameButton: UInt32, CaseIterable {
    case a = 0x001
    case b = 0x002
    case select = 0x004
    case start = 0x008
    case right = 0x010
    case left = 0x020
    case up = 0x040
    case down = 0x080
    case r = 0x100
    case l = 0x200
    case x = 0x400
    case y = 0x800
}

/// The live input snapshot the run loop feeds the core each frame. Mutated by the
/// on-screen controls and read by the display-link tick — all on the main thread.
final class InputState {
    /// A mask of `Button` raw values currently held.
    var buttons: UInt32 = 0
    var touchX: Int16 = 0
    var touchY: Int16 = 0
    var touchPressed: Bool = false
}
