import SwiftUI

/// A toolbar affordance (Save / Menu) — a stacked glyph and label. These are discrete
/// taps, so ordinary SwiftUI buttons, not part of the multitouch layer.
struct ToolbarActionButton: View {
    let systemImage: String
    let title: String
    let shell: Shell
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            VStack(spacing: 3) {
                Image(systemName: systemImage)
                    .font(.system(size: 19, weight: .medium))
                Text(title)
                    .font(.system(size: 10, weight: .semibold))
                    .tracking(0.6)
            }
            .foregroundStyle(shell.caption)
            .frame(minWidth: 46)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}

/// The "RewinDS" wordmark used as the rewind affordance in the DS top bar.
struct RewinDSWordmark: View {
    let shell: Shell
    var action: () -> Void

    var body: some View {
        Button(action: action) {
            Text("RewinDS")
                .font(.system(size: 16, weight: .heavy, design: .rounded))
                .foregroundStyle(shell.accent)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}
