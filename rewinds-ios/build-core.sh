#!/usr/bin/env bash
#
# Build the Rust emulator core into RewindsCore.xcframework for the iOS app.
#
# Produces a static-library XCFramework with two slices — device (ios-arm64) and
# simulator (ios-arm64-simulator) — plus the cbindgen-generated C header wrapped in
# a Clang modulemap, so Swift can `import RewindsCore`. Re-run this whenever
# crates/emulator-ffi or anything it links changes; the Xcode project references the
# framework at a fixed path, so a rebuild is picked up without project edits.
#
# Static linking (not a dynamic .dylib/.framework) is deliberate: it needs no embedded
# framework signing, links the whole emulator directly into the app binary, and is what
# `xcodebuild -create-xcframework` packages most cleanly for iOS.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
FFI_DIR="$REPO_ROOT/crates/emulator-ffi"
OUT_DIR="$SCRIPT_DIR/Frameworks"
XCFRAMEWORK="$OUT_DIR/RewindsCore.xcframework"
CONFIG="release"
LIB="librewinds_core.a"

# rustup/cargo live in ~/.cargo/bin, which Xcode's non-login build shell does not have.
export PATH="$HOME/.cargo/bin:$PATH"

echo "==> Ensuring iOS Rust targets are installed"
rustup target add aarch64-apple-ios aarch64-apple-ios-sim >/dev/null

echo "==> Building static lib (device + simulator, $CONFIG)"
cargo build -p emulator-ffi --release --target aarch64-apple-ios
cargo build -p emulator-ffi --release --target aarch64-apple-ios-sim

echo "==> Regenerating C header with cbindgen"
cbindgen --config "$FFI_DIR/cbindgen.toml" --crate emulator-ffi \
    --output "$FFI_DIR/include/rewinds_core.h"

echo "==> Assembling module headers"
HDR_DIR="$(mktemp -d)"
trap 'rm -rf "$HDR_DIR"' EXIT
cp "$FFI_DIR/include/rewinds_core.h" "$HDR_DIR/rewinds_core.h"
cat > "$HDR_DIR/module.modulemap" <<'EOF'
module RewindsCore {
    header "rewinds_core.h"
    export *
}
EOF

echo "==> Creating RewindsCore.xcframework"
rm -rf "$XCFRAMEWORK"
mkdir -p "$OUT_DIR"
xcodebuild -create-xcframework \
    -library "$REPO_ROOT/target/aarch64-apple-ios/$CONFIG/$LIB" -headers "$HDR_DIR" \
    -library "$REPO_ROOT/target/aarch64-apple-ios-sim/$CONFIG/$LIB" -headers "$HDR_DIR" \
    -output "$XCFRAMEWORK"

echo "==> Done: $XCFRAMEWORK"
