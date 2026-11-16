#!/usr/bin/env bash
# Build "ADA Checker.app" (and optionally a .dmg) for macOS.
#
# Usage: scripts/bundle-macos.sh [--dmg]
# Output: target/release/bundle/ADA Checker.app
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"
APP_NAME="ADA Checker"
BIN_NAME="adachecker"
VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
OUT_DIR="$ROOT/target/release/bundle"
APP="$OUT_DIR/$APP_NAME.app"
MAKE_DMG=0
[[ "${1:-}" == "--dmg" ]] && MAKE_DMG=1

echo "==> Building release binary (v$VERSION)"
MACOSX_DEPLOYMENT_TARGET=11.0 cargo build --release

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/release/$BIN_NAME" "$APP/Contents/MacOS/$BIN_NAME"
sed "s/__VERSION__/$VERSION/g" macos/Info.plist > "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"

echo "==> Generating app icon"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
python3 -I macos/make_icon.py "$WORK/icon_1024.png"
ICONSET="$WORK/AppIcon.iconset"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
    sips -z $size $size "$WORK/icon_1024.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z $double $double "$WORK/icon_1024.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"

echo "==> Code signing (ad-hoc)"
codesign --force --deep --sign - "$APP"
codesign --verify --strict "$APP"

# Make Finder/Dock pick up the new icon if the bundle was rebuilt in place.
touch "$APP"

if [[ $MAKE_DMG == 1 ]]; then
    DMG="$OUT_DIR/ADA-Checker-$VERSION.dmg"
    echo "==> Creating $DMG"
    STAGE="$WORK/dmg"
    mkdir -p "$STAGE"
    cp -R "$APP" "$STAGE/"
    ln -s /Applications "$STAGE/Applications"
    rm -f "$DMG"
    hdiutil create -volname "$APP_NAME" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
fi

echo
echo "Done: $APP"
[[ $MAKE_DMG == 1 ]] && echo "      $DMG"
echo "Launch with: open \"$APP\""
