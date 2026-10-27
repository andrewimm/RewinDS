# RewinDS for iOS

A SwiftUI front end that runs the RewinDS emulator core on iPhone: a landscape GBA
shell and a portrait DS shell, both drawing the emulator's framebuffers with Metal and
driving input through a true multitouch controller (hold Up+Left and press A at once).

```
┌─ rewinds-ios ──────────────────────────────────────────────┐
│ SwiftUI shell (this dir)                                    │
│   ├─ Metal screen views  ← RGBA framebuffers from the core  │
│   ├─ multitouch controls → rewinds_set_input                │
│   └─ AVAudioEngine       ← rewinds_audio_read (audio thread)│
│            │                                                │
│            ▼  extern-C (RewindsCore.xcframework)            │
│   crates/emulator-ffi  →  emulator facade  →  gba / nds     │
└─────────────────────────────────────────────────────────────┘
```

The Rust lives in the repo root Cargo workspace (unchanged — no `rewinds-core/` move
was needed); this app links it through a static-library XCFramework.

## Build & run

Prerequisites (one-time): `rustup`, plus `brew install cbindgen xcodegen`, and the iOS
Rust targets (`rustup target add aarch64-apple-ios aarch64-apple-ios-sim`). Xcode 26+.

```sh
cd rewinds-ios
./build-core.sh          # builds crates/emulator-ffi → Frameworks/RewindsCore.xcframework
xcodegen generate        # writes RewinDS.xcodeproj from project.yml
open RewinDS.xcodeproj    # then set your signing Team and run on a device/simulator
```

Re-run `./build-core.sh` whenever the Rust changes, and `xcodegen generate` whenever you
add/rename Swift files or edit `project.yml`. Both the generated `.xcodeproj` and the
`.xcframework` are gitignored build artifacts.

### Updating the core on a device

`build-core.sh` is the **only** thing that refreshes what the app links. A change to any
Rust crate the emulator uses (`arm`, `gba`, `nds`, `video2d`, `emulator`, `emulator-ffi`, …)
does **not** reach the app until you rebuild the framework — so after changing Rust:

```sh
./rewinds-ios/build-core.sh   # rebuilds Frameworks/RewindsCore.xcframework in place
# then rebuild the RewinDS scheme in Xcode and run to your device
```

Note that `cargo build`/`cargo run -p rewinds` rebuilds only the **desktop** host — it never
touches the XCFramework. That is why a fix can show up in the desktop binary but not on the
phone: the two are separate builds, and only `build-core.sh` updates the app's core.

If the app still runs the old core after rebuilding (Xcode sometimes caches the
`.xcframework`), force a clean pickup: **Product → Clean Build Folder** (⇧⌘K), or
`rm -rf ~/Library/Developer/Xcode/DerivedData/RewinDS-*`, then run again.

The framework isn't committed, so this is per-machine; committing the Rust source is what
makes a fix portable (anyone who then runs `build-core.sh`, or CI's `macos-app.yml`,
reproduces the same framework).

### Command-line build (simulator)

```sh
xcodebuild -project RewinDS.xcodeproj -scheme RewinDS \
    -sdk iphonesimulator -destination 'platform=iOS Simulator,name=iPhone 17 Pro' \
    -derivedDataPath build CODE_SIGNING_ALLOWED=NO build
```

## Getting a game running

1. **Add your BIOS** (copyrighted — never bundled in source). Either drop dumps into
   `dev-assets/` before building (see `dev-assets/README.md`) or import them in-app under
   the gear menu → System files. The app needs the GBA BIOS for `.gba`, and the DS ARM9 +
   ARM7 BIOS for `.nds` (DS ROMs direct-boot, so firmware is optional).
2. **Open a ROM** with the "Open a ROM…" button (Files picker), or drop `.gba`/`.nds`
   files into `dev-assets/` to have them appear in the library automatically.

## How it fits together

Shared, UI-agnostic Swift lives in the `RewindsKit` local Swift package
(`RewindsKit/Sources/RewindsKit/`), which both this app and the macOS app link; the
`RewinDS/` tree holds only the iOS-specific SwiftUI shell.

| Piece | File(s) |
|-------|---------|
| C boundary over the facade | `crates/emulator-ffi` (`rewinds_*`, cbindgen header) |
| Framework packaging | `build-core.sh` → `Frameworks/RewindsCore.xcframework` |
| Swift wrapper over the C API | `RewindsKit/Sources/RewindsKit/EmulatorCore.swift` |
| Run loop (own thread, 60 Hz), input, link | `RewindsKit/Sources/RewindsKit/EmulatorSession.swift` |
| Save files, audio | `RewindsKit/Sources/RewindsKit/{SaveStore,AudioEngine}.swift` |
| Metal blit of RGBA screens | `RewinDS/Views/MetalScreenView.swift`, `Shaders.metal` |
| Multitouch controller | `RewinDS/Views/Controls/*` |
| GBA landscape / DS portrait shells | `RewinDS/Views/GBALayout.swift`, `DSLayout.swift` |
| BIOS provisioning | `RewinDS/Core/BIOSStore.swift` |

## Architecture notes

- **Emulation runs on its own thread** (`EmulatorSession`), paced to ~60 Hz, so UI or
  main-thread hitches can't starve audio or stall the frame cadence. All core access is
  serialized by a lock (the core is `Send`, not `Sync`); the Metal views draw on their
  own display link, pulling the latest frame handed to them under a lock.
- **Screens** are `MTKView`s that run their own display loop and render the latest
  emulator frame in `draw(_:)` — presenting a drawable manually off that cycle leaves
  fast-changing (e.g. 3D) content stuck on black, which is why static screens rendered
  but animated ones didn't until this was fixed.
- **Console detection** trusts the `.gba`/`.nds` file extension (the core's header sniff
  mis-classifies some homebrew).

## Not yet wired

- **Rewind** (the "RewinDS" toolbar action) isn't connected: time-travel lives in the
  `debug` crate and isn't on the emulator facade yet, so the button reports that honestly.
- **Audio** plays but may want on-device tuning; the emulator resamples once to the
  actual output hardware rate.
- The app closes the DS lid on backgrounding and opens it on foreground (games sleep/wake).
- `REWINDS_AUTOLAUNCH=<game name>` (an environment variable) auto-launches a library entry
  on startup — a dev/testing hook, inert in normal use.
