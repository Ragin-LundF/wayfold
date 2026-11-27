#!/bin/bash
# Builds an unsigned Wayfold.app (OQ-4): release binary, Info.plist, third-party notices.
# Ad-hoc signed only, so Gatekeeper asks on first launch (right-click > Open).
# Output: target/release/Wayfold.app
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
export PATH="/opt/homebrew/opt/rustup/bin:$PATH" # rustup proxy, so rust-toolchain.toml applies

cargo build --release -p wayfold
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"

app="target/release/Wayfold.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/wayfold "$app/Contents/MacOS/wayfold"
cp THIRD-PARTY-NOTICES.md "$app/Contents/Resources/"
cp -R licenses "$app/Contents/Resources/licenses"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Wayfold</string>
  <key>CFBundleDisplayName</key><string>Wayfold</string>
  <key>CFBundleIdentifier</key><string>dev.wayfold.Wayfold</string>
  <key>CFBundleExecutable</key><string>wayfold</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSDesktopFolderUsageDescription</key><string>Wayfold shows and manages the files you browse to.</string>
  <key>NSDocumentsFolderUsageDescription</key><string>Wayfold shows and manages the files you browse to.</string>
  <key>NSDownloadsFolderUsageDescription</key><string>Wayfold shows and manages the files you browse to.</string>
  <key>NSRemovableVolumesUsageDescription</key><string>Wayfold shows and manages files on removable drives.</string>
  <key>NSNetworkVolumesUsageDescription</key><string>Wayfold shows and manages files on network volumes.</string>
</dict>
</plist>
EOF

plutil -lint "$app/Contents/Info.plist" >/dev/null
codesign --force --deep --sign - "$app"
echo "Built $app ($version)"
