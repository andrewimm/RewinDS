# dev-assets — local BIOS / firmware / test ROMs (never committed)

Everything in this folder except this README is gitignored. Drop your own dumps
here to get straight into a game on your own device or the simulator; the build
bundles whatever is present, and the app also lets you import these files at runtime
through the Files picker (Settings → System files) if you'd rather not bundle them.

## Recognized filenames

The app looks these up by exact name (bundled copies, then any you imported at runtime
which take precedence):

| File            | Purpose                                  | Required for       |
|-----------------|------------------------------------------|--------------------|
| `gba_bios.bin`  | GBA BIOS (16 KiB)                        | every GBA ROM      |
| `nds_bios9.bin` | DS ARM9 BIOS                             | DS direct boot     |
| `nds_bios7.bin` | DS ARM7 BIOS                             | DS direct boot     |
| `nds_firmware.bin` | DS firmware dump (optional)           | firmware boot only |

Test ROMs (`*.gba`, `*.nds`) dropped here also show up in the library as quick-launch
entries, so you don't have to import them through Files each time.

The app **direct-boots** DS ROMs (straight into the game), so `nds_firmware.bin` is
optional — it's only used if you later add a firmware-boot toggle.
