#!/bin/bash
# Assembles Rapid View.app from release binary + Info.plist.
# Usage: ./bundle.sh [debug|release]   (default: release)
set -euo pipefail

PROFILE="${1:-release}"
ROOT="$(cd "$(dirname "$0")" && pwd)"
# Respect CARGO_TARGET_DIR so we read the binary cargo actually wrote.
# Without this, a shell-level override (e.g. ~/.cargo points elsewhere) leaves
# a stale binary in $ROOT/target/ that silently ships into the .app.
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
APP="$TARGET_DIR/$PROFILE/Rapid View.app"
BIN_NAME="rapid-view"

if [ "$PROFILE" = "release" ]; then
    cargo build --release
else
    cargo build
fi
SRC_BIN="$TARGET_DIR/$PROFILE/$BIN_NAME"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
mkdir -p "$APP/Contents/Resources"
cp "$SRC_BIN" "$APP/Contents/MacOS/rapid-view"
cp "$ROOT/Info.plist" "$APP/Contents/Info.plist"
cp "$ROOT/assets/RapidView.icns" "$APP/Contents/Resources/RapidView.icns"

# Minimal PkgInfo — classic Mac bundle marker.
printf 'APPL????' > "$APP/Contents/PkgInfo"

echo "Built: $APP"
