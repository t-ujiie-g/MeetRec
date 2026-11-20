#!/bin/bash
# MeetRec.app を組み立てて署名し、zip にする。
#   packaging/macos/bundle.sh <実行ファイル> <出力ディレクトリ>
# 環境変数 CODESIGN_IDENTITY を指定すると Developer ID で署名する（既定は ad-hoc "-"）。
set -euo pipefail
BIN="$1"
OUT="${2:-dist}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
APP="$OUT/MeetRec.app"
IDENTITY="${CODESIGN_IDENTITY:--}"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/meetrec"
chmod +x "$APP/Contents/MacOS/meetrec"
sed "s/__VERSION__/$VERSION/g" "$ROOT/packaging/macos/Info.plist" > "$APP/Contents/Info.plist"

if [ "$IDENTITY" = "-" ]; then
  codesign --force --sign - "$APP"
else
  codesign --force --options runtime --timestamp \
    --entitlements "$ROOT/packaging/macos/entitlements.plist" \
    --sign "$IDENTITY" "$APP"
fi
codesign --verify --verbose "$APP"

(cd "$OUT" && ditto -c -k --keepParent MeetRec.app "MeetRec-macos-$VERSION.zip")
ls -la "$OUT"
