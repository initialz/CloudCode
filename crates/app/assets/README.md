# cloudcode-app assets

`icon.png` is a 512×512 placeholder app icon (solid dark). Replace it with a
real design when one exists.

## Generating `icon.icns` (macOS)

`cargo bundle` and the release CI expect `icon.icns`. macOS builds it from the
PNG via `iconutil` (macOS-only tool — not available on Linux, so the `.icns` is
**not committed**; the CI/macOS build generates it). The release workflow runs:

```bash
mkdir -p icon.iconset
sips -z 16 16     assets/icon.png --out icon.iconset/icon_16x16.png
sips -z 32 32     assets/icon.png --out icon.iconset/icon_16x16@2x.png
sips -z 32 32     assets/icon.png --out icon.iconset/icon_32x32.png
sips -z 64 64     assets/icon.png --out icon.iconset/icon_32x32@2x.png
sips -z 128 128   assets/icon.png --out icon.iconset/icon_128x128.png
sips -z 256 256   assets/icon.png --out icon.iconset/icon_128x128@2x.png
sips -z 256 256   assets/icon.png --out icon.iconset/icon_256x256.png
sips -z 512 512   assets/icon.png --out icon.iconset/icon_256x256@2x.png
sips -z 512 512   assets/icon.png --out icon.iconset/icon_512x512.png
cp assets/icon.png icon.iconset/icon_512x512@2x.png
iconutil -c icns icon.iconset -o crates/app/assets/icon.icns
```

If `icon.icns` is missing at bundle time, `cargo bundle` falls back to a
default icon — the build still succeeds.
