import SwiftUI

/// A single emulator screen, held at its native aspect ratio with a rounded bezel. The
/// DS lower screen passes `touch: true` to route touches as the console's touchscreen.
struct GameScreen: View {
    let session: EmulatorSession
    let index: Int
    let aspect: CGFloat
    let shell: Shell
    var touch: Bool = false

    var body: some View {
        MetalScreenView(session: session, index: index, isTouchScreen: touch)
            .aspectRatio(aspect, contentMode: .fit)
            .background(Color.black)
            .clipShape(RoundedRectangle(cornerRadius: 9, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: 9, style: .continuous)
                    .strokeBorder(shell.bezel, lineWidth: 3))
    }
}
