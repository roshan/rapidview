#!/bin/bash
# Assembles Markview.app from release binary + Info.plist.
# Usage: ./bundle.sh [debug|release]   (default: release)
set -euo pipefail

PROFILE="${1:-release}"
ROOT="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE_ROOT="$(cd "$ROOT/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$WORKSPACE_ROOT/target}"
APP="$TARGET_DIR/$PROFILE/Markview.app"
BIN_NAME="markview"

if [ "$PROFILE" = "release" ]; then
    (cd "$WORKSPACE_ROOT" && cargo build -p markview --release)
else
    (cd "$WORKSPACE_ROOT" && cargo build -p markview)
fi
SRC_BIN="$TARGET_DIR/$PROFILE/$BIN_NAME"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
mkdir -p "$APP/Contents/Resources"
cp "$SRC_BIN" "$APP/Contents/MacOS/markview"
cp "$ROOT/Info.plist" "$APP/Contents/Info.plist"
cp "$ROOT/assets/Markview.icns" "$APP/Contents/Resources/Markview.icns"

# Minimal PkgInfo — classic Mac bundle marker.
printf 'APPL????' > "$APP/Contents/PkgInfo"

echo "Built: $APP"
