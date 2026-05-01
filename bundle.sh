#!/bin/bash
# Assembles Rapid View.app from release binary + Info.plist.
# Usage: ./bundle.sh [debug|release]   (default: release)
set -euo pipefail

PROFILE="${1:-release}"
ROOT="$(cd "$(dirname "$0")" && pwd)"
APP="$ROOT/target/$PROFILE/Rapid View.app"
BIN_NAME="rapid-view"

if [ "$PROFILE" = "release" ]; then
    cargo build --release
    SRC_BIN="$ROOT/target/release/$BIN_NAME"
else
    cargo build
    SRC_BIN="$ROOT/target/debug/$BIN_NAME"
fi

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
mkdir -p "$APP/Contents/Resources"
cp "$SRC_BIN" "$APP/Contents/MacOS/rapid-view"
cp "$ROOT/Info.plist" "$APP/Contents/Info.plist"
cp "$ROOT/assets/RapidView.icns" "$APP/Contents/Resources/RapidView.icns"

# Minimal PkgInfo — classic Mac bundle marker.
printf 'APPL????' > "$APP/Contents/PkgInfo"

echo "Built: $APP"
