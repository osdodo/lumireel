#!/usr/bin/env bash
# Build dist/LumiReel.app: release binaries (lumireel + reel), Info.plist and icon,
# then pack it as the drag-to-install DMG the release workflow uploads.
set -euo pipefail

cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
arch=$(uname -m)
app=dist/LumiReel.app
dmg=dist/lumireel-$version-macos-$arch.dmg

cargo build --release --bins

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
# reel sits next to lumireel: the agent terminal puts this directory on PATH.
cp target/release/lumireel target/release/reel "$app/Contents/MacOS/"
cp assets/AppIcon.icns "$app/Contents/Resources/"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>LumiReel</string>
  <key>CFBundleDisplayName</key><string>LumiReel</string>
  <key>CFBundleIdentifier</key><string>com.osdodo.lumireel</string>
  <key>CFBundleExecutable</key><string>lumireel</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.video</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSPrincipalClass</key><string>NSApplication</string>
</dict>
</plist>
PLIST

# Ad-hoc signature so Gatekeeper treats the bundle as one unit; not notarized.
codesign --force --deep --sign - "$app"

# The image holds the app and an Applications link, so the window is a drag-to-install
# target without any Finder scripting.
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"

rm -f "$dmg"
hdiutil create -quiet -volname LumiReel -srcfolder "$stage" -format UDZO -ov "$dmg"

echo "Built $app ($version)"
echo "Built $dmg"
