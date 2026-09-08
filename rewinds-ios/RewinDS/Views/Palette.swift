import SwiftUI
import RewindsKit

extension Color {
    init(hex: UInt32) {
        self.init(
            .sRGB,
            red: Double((hex >> 16) & 0xFF) / 255,
            green: Double((hex >> 8) & 0xFF) / 255,
            blue: Double(hex & 0xFF) / 255,
            opacity: 1)
    }
}

/// The visual identity for one console's shell. Two deliberately different worlds: the
/// GBA is a warm twilight-indigo handheld with soft lavender keys; the DS is a graphite
/// slab with a purple "RewinDS" accent — mirroring the reference mockups.
struct Shell {
    var background: Color
    var backgroundEdge: Color
    var bezel: Color
    var face: Color          // key face
    var faceEdge: Color      // key bottom/shadow
    var faceLabel: Color     // glyph/letter on a key
    var caption: Color       // muted labels (SELECT/START text, captions)
    var accent: Color        // the RewinDS purple

    static let gba = Shell(
        background: Color(hex: 0x2E2866),
        backgroundEdge: Color(hex: 0x221B4C),
        bezel: Color(hex: 0x1C1642),
        face: Color(hex: 0xC9C5E9),
        faceEdge: Color(hex: 0x9A95C8),
        faceLabel: Color(hex: 0x4C4676),
        caption: Color(hex: 0xAEA9DA),
        accent: Color(hex: 0x8B7FF2))

    static let ds = Shell(
        background: Color(hex: 0x0D0D10),
        backgroundEdge: Color(hex: 0x000000),
        bezel: Color(hex: 0x000000),
        face: Color(hex: 0x2B2B32),
        faceEdge: Color(hex: 0x171719),
        faceLabel: Color(hex: 0xEAEAF1),
        caption: Color(hex: 0x8C8C97),
        accent: Color(hex: 0x8B7FF2))

    static func of(_ console: Console) -> Shell { console == .gba ? .gba : .ds }
}
