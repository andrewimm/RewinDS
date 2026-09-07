import SwiftUI

/// A cross-shaped directional pad. It's a single control region: the multitouch layer
/// reads the touch position within the cross and arms one or two directions, so corners
/// give diagonals (Up+Left, etc.) and a second finger can stack more.
struct DPadView: View {
    let shell: Shell
    var size: CGFloat = 150

    private var thickness: CGFloat { size * 0.36 }

    var body: some View {
        ZStack {
            arm.frame(width: thickness, height: size)
            arm.frame(width: size, height: thickness)

            Circle()
                .fill(shell.faceEdge.opacity(0.45))
                .frame(width: thickness * 0.62, height: thickness * 0.62)

            arrow.offset(y: -size * 0.31)
            arrow.rotationEffect(.degrees(90)).offset(x: size * 0.31)
            arrow.rotationEffect(.degrees(180)).offset(y: size * 0.31)
            arrow.rotationEffect(.degrees(270)).offset(x: -size * 0.31)
        }
        .frame(width: size, height: size)
        .shadow(color: .black.opacity(0.4), radius: 6, y: 3)
        .controlRegion(.dpad)
    }

    private var arm: some View {
        RoundedRectangle(cornerRadius: thickness * 0.26, style: .continuous)
            .fill(shell.face)
    }

    private var arrow: some View {
        Image(systemName: "triangle.fill")
            .font(.system(size: size * 0.1))
            .foregroundStyle(shell.faceLabel.opacity(0.9))
    }
}
