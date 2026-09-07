import UIKit

/// Per-console orientation locking. The app allows both orientations in Info.plist;
/// this pins the device to the one a console wants — GBA landscape, DS portrait — and
/// asks the window scene to rotate to it immediately.
enum OrientationLock {
    /// The mask the app delegate hands back for `supportedInterfaceOrientationsFor`.
    static var mask: UIInterfaceOrientationMask = .all

    /// Lock to `mask` and rotate the active scene into it now.
    static func set(_ mask: UIInterfaceOrientationMask, preferred: UIInterfaceOrientation) {
        self.mask = mask
        guard let scene = UIApplication.shared.connectedScenes
            .compactMap({ $0 as? UIWindowScene }).first(where: { $0.activationState == .foregroundActive })
            ?? UIApplication.shared.connectedScenes.compactMap({ $0 as? UIWindowScene }).first
        else { return }

        scene.requestGeometryUpdate(.iOS(interfaceOrientations: mask)) { _ in }
        // Nudge the top view controller so it re-evaluates the (now narrowed) mask.
        scene.keyWindow?.rootViewController?.setNeedsUpdateOfSupportedInterfaceOrientations()
    }

    static func landscape() { set(.landscape, preferred: .landscapeRight) }
    static func portrait() { set(.portrait, preferred: .portrait) }
    static func unlock() { set(.all, preferred: .portrait) }
}

/// Minimal app delegate whose only job is to report the current orientation lock.
final class AppDelegate: NSObject, UIApplicationDelegate {
    func application(
        _ application: UIApplication,
        supportedInterfaceOrientationsFor window: UIWindow?
    ) -> UIInterfaceOrientationMask {
        OrientationLock.mask
    }
}
