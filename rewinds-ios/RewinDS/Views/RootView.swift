import SwiftUI

/// Switches between the library and a running game, and surfaces load errors.
struct RootView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        Group {
            switch model.route {
            case .library:
                LibraryView()
            case .game(let session):
                EmulatorView(session: session)
            }
        }
        .alert(
            model.alert?.title ?? "",
            isPresented: Binding(
                get: { model.alert != nil },
                set: { if !$0 { model.alert = nil } }),
            presenting: model.alert
        ) { _ in
            Button("OK", role: .cancel) {}
        } message: { alert in
            Text(alert.message)
        }
        .onAppear { model.autolaunchIfRequested() }
    }
}
