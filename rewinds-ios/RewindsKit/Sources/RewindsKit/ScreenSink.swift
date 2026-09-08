import Foundation

/// A destination for one presented emulator screen — a Metal view on iOS (`UIView`) or
/// macOS (`NSView`). The run loop hands it a freshly presented RGBA8 buffer, borrowed only
/// for the duration of the call, which the sink copies and draws on its own clock.
public protocol ScreenSink: AnyObject {
    func present(width: Int, height: Int, pixels: UnsafePointer<UInt8>)

    /// Called when presentation resumes (app foregrounded, in-game menu closed). A view can
    /// use it to re-establish a display link that a screen-off / lock can leave stalled —
    /// the emulator keeps producing frames, but the view stops drawing them. Default no-op.
    func displayResumed()
}

public extension ScreenSink {
    func displayResumed() {}
}
