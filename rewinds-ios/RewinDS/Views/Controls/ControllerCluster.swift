import SwiftUI

/// Wraps a layout's controls (and screens) so touches are handled by the multitouch
/// overlay: it establishes the `controller` coordinate space, funnels every reported
/// control frame into the registry, and lays a `MultiTouchOverlay` on top that reports
/// the combined button mask to the session. Non-control touches (e.g. the DS
/// touchscreen sitting under the overlay) fall straight through.
struct ControllerCluster<Content: View>: View {
    let registry: ControlRegistry
    let session: EmulatorSession
    @ViewBuilder var content: () -> Content

    var body: some View {
        content()
            .coordinateSpace(name: ControllerSpace.name)
            .onPreferenceChange(ControlFramesKey.self) { frames in
                registry.apply(frames)
            }
            .overlay {
                MultiTouchOverlay(registry: registry) { mask in
                    session.setButtons(mask)
                }
            }
    }
}
