#!/usr/bin/env bash
# Builds Teamspace.app from a compiled binary.
#
# Usage: scripts/bundle-macos.sh <binary> <version> <output-dir>
#
# CODESIGN_IDENTITY selects the signing identity; it defaults to "-" (ad-hoc).
# A real identity also enables the hardened runtime and a secure timestamp,
# which notarization requires.
set -euo pipefail

if [ "$#" -ne 3 ]; then
  echo "usage: $0 <binary> <version> <output-dir>" >&2
  exit 2
fi
binary=$1
version=$2
output=$3
identity=${CODESIGN_IDENTITY:--}

root=$(cd "$(dirname "$0")/.." && pwd)
app="$output/Teamspace.app"
contents="$app/Contents"
resources="$contents/Resources"

rm -rf "$app"
mkdir -p "$contents/MacOS" "$resources/licenses"
cp "$binary" "$contents/MacOS/teamspace"
chmod 755 "$contents/MacOS/teamspace"

sed "s/__VERSION__/$version/g" "$root/packaging/macos/Info.plist" > "$contents/Info.plist"
plutil -lint "$contents/Info.plist" >/dev/null

# Icon: render the standard iconset sizes from the 1024 px master.
iconset=$(mktemp -d)/teamspace.iconset
mkdir -p "$iconset"
master="$root/assets/app-icon/teamspace-1024.png"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$master" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" "$master" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$resources/teamspace.icns"
rm -rf "$(dirname "$iconset")"

cp "$root/LICENSE" "$resources/licenses/LICENSE"
cp "$root/assets/fonts/OFL.txt" "$resources/licenses/Inter-OFL.txt"
cp "$root/assets/icons/LICENSE" "$resources/licenses/Fluent-UI-System-Icons-LICENSE"

sign_args=(--force --sign "$identity")
if [ "$identity" != "-" ]; then
  sign_args+=(--options runtime --timestamp)
fi
codesign "${sign_args[@]}" "$app"
codesign --verify --strict --verbose=2 "$app"
echo "$app"
