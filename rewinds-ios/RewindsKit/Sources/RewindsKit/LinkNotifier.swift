import Foundation
import UserNotifications

/// Local notifications for link lifecycle events (connected / disconnected). Authorization
/// is requested lazily the first time the user hosts or joins, so a player who never links
/// is never prompted. Foreground presentation is enabled via `Presenter`, set as the
/// notification-center delegate at launch.
public enum LinkNotifier {
    private static var didRequestAuth = false

    /// Install the foreground-presentation delegate. Called once at launch; does not prompt.
    public static func configure() {
        UNUserNotificationCenter.current().delegate = Presenter.shared
    }

    /// Ask for notification permission the first time linking is used.
    static func requestAuthIfNeeded() {
        guard !didRequestAuth else { return }
        didRequestAuth = true
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound]) { _, _ in }
    }

    /// Post a link notification. No-op if authorization was denied.
    static func notify(_ title: String, _ body: String) {
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        content.sound = .default
        let request = UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request)
    }

    /// Presents link notifications as banners even while RewinDS is in the foreground
    /// (the usual case during a game), where iOS would otherwise suppress them.
    final class Presenter: NSObject, UNUserNotificationCenterDelegate {
        static let shared = Presenter()

        func userNotificationCenter(
            _ center: UNUserNotificationCenter,
            willPresent notification: UNNotification,
            withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
        ) {
            completionHandler([.banner, .sound])
        }
    }
}
