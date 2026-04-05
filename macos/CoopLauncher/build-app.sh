#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")" && pwd)"
APP_NAME="Coop Launcher"
EXECUTABLE_NAME="CoopLauncher"
APP_BUNDLE="$ROOT_DIR/dist/$APP_NAME.app"
BUILD_DIR="$ROOT_DIR/.build/release"
EXECUTABLE_PATH="$BUILD_DIR/$EXECUTABLE_NAME"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "Coop Launcher can only be built on macOS." >&2
  exit 1
fi

cd "$ROOT_DIR"
swift build -c release

rm -rf "$APP_BUNDLE"
mkdir -p "$APP_BUNDLE/Contents/MacOS" "$APP_BUNDLE/Contents/Resources"

cp "$ROOT_DIR/Resources/Info.plist" "$APP_BUNDLE/Contents/Info.plist"
cp "$EXECUTABLE_PATH" "$APP_BUNDLE/Contents/MacOS/$EXECUTABLE_NAME"
chmod +x "$APP_BUNDLE/Contents/MacOS/$EXECUTABLE_NAME"

codesign --force --deep --sign - "$APP_BUNDLE"
codesign --verify --deep --strict "$APP_BUNDLE"

echo "Built: $APP_BUNDLE"
echo "Next: open \"$APP_BUNDLE\""
