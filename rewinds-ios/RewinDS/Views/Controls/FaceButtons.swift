import SwiftUI

/// GBA face buttons: A upper-right, B lower-left, on the diagonal (as on the hardware).
struct GBAFaceButtons: View {
    let shell: Shell
    var diameter: CGFloat = 66

    var body: some View {
        ZStack {
            RoundKey(button: .b, label: "B", shell: shell, diameter: diameter)
                .offset(x: -diameter * 0.5, y: diameter * 0.5)
            RoundKey(button: .a, label: "A", shell: shell, diameter: diameter)
                .offset(x: diameter * 0.55, y: -diameter * 0.28)
        }
        .frame(width: diameter * 2.2, height: diameter * 2)
    }
}

/// DS face buttons in the X/Y/A/B diamond (X top, Y left, A right, B bottom).
struct DSFaceButtons: View {
    let shell: Shell
    var diameter: CGFloat = 58
    var spread: CGFloat = 50

    var body: some View {
        ZStack {
            RoundKey(button: .x, label: "X", shell: shell, diameter: diameter).offset(y: -spread)
            RoundKey(button: .y, label: "Y", shell: shell, diameter: diameter).offset(x: -spread)
            RoundKey(button: .a, label: "A", shell: shell, diameter: diameter).offset(x: spread)
            RoundKey(button: .b, label: "B", shell: shell, diameter: diameter).offset(y: spread)
        }
        .frame(width: spread * 2 + diameter, height: spread * 2 + diameter)
    }
}
