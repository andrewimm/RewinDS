import SwiftUI

/// A round face key (A/B/X/Y) — a soft raised disc with a rounded letter.
struct RoundKey: View {
    let button: GameButton
    let label: String
    let shell: Shell
    var diameter: CGFloat = 64

    var body: some View {
        ZStack {
            Circle()
                .fill(shell.face)
                .overlay(
                    Circle().stroke(shell.faceEdge.opacity(0.6), lineWidth: 1))
            Text(label)
                .font(.system(size: diameter * 0.36, weight: .semibold, design: .rounded))
                .foregroundStyle(shell.faceLabel)
        }
        .frame(width: diameter, height: diameter)
        .shadow(color: .black.opacity(0.4), radius: 5, x: 0, y: 3)
        .controlRegion(.button(button))
    }
}

/// A wide shoulder key (L/R).
struct ShoulderKey: View {
    let button: GameButton
    let label: String
    let shell: Shell
    var width: CGFloat = 150
    var height: CGFloat = 46

    var body: some View {
        ZStack {
            RoundedRectangle(cornerRadius: height * 0.32, style: .continuous)
                .fill(shell.face)
                .overlay(
                    RoundedRectangle(cornerRadius: height * 0.32, style: .continuous)
                        .stroke(shell.faceEdge.opacity(0.6), lineWidth: 1))
            Text(label)
                .font(.system(size: 17, weight: .semibold, design: .rounded))
                .foregroundStyle(shell.faceLabel)
        }
        .frame(width: width, height: height)
        .shadow(color: .black.opacity(0.35), radius: 4, y: 2)
        .controlRegion(.button(button))
    }
}

/// A pill key with the label inside (GBA START / SELECT).
struct PillKey: View {
    let button: GameButton
    let label: String
    let shell: Shell
    var width: CGFloat = 96

    var body: some View {
        ZStack {
            Capsule(style: .continuous).fill(shell.face)
            Text(label)
                .font(.system(size: 12, weight: .bold))
                .tracking(0.5)
                .foregroundStyle(shell.faceLabel)
        }
        .frame(width: width, height: 34)
        .shadow(color: .black.opacity(0.3), radius: 3, y: 2)
        .controlRegion(.button(button))
    }
}

/// A small round key with the label below it (DS START / SELECT).
struct MiniKey: View {
    let button: GameButton
    let label: String
    let shell: Shell

    var body: some View {
        VStack(spacing: 6) {
            Circle()
                .fill(shell.face)
                .frame(width: 26, height: 26)
                .shadow(color: .black.opacity(0.4), radius: 3, y: 2)
                .controlRegion(.button(button))
            Text(label)
                .font(.system(size: 11, weight: .semibold))
                .tracking(0.5)
                .foregroundStyle(shell.caption)
        }
    }
}
