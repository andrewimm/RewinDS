// swift-tools-version:5.9
import PackageDescription

// RewindsKit — the shared, platform-neutral core for the iOS and macOS apps: the
// emulator FFI wrapper, run loop, saves, ROM library, and the serial-link stack. It wraps
// the Rust core as a binary XCFramework (built by ../build-core.sh) so both apps link the
// same emulator. UI (Metal views, controls, windows) stays in each app target.
let package = Package(
    name: "RewindsKit",
    platforms: [.iOS(.v17), .macOS(.v14)],
    products: [
        .library(name: "RewindsKit", targets: ["RewindsKit"]),
    ],
    targets: [
        .binaryTarget(name: "RewindsCore", path: "../Frameworks/RewindsCore.xcframework"),
        .target(name: "RewindsKit", dependencies: ["RewindsCore"]),
    ]
)
