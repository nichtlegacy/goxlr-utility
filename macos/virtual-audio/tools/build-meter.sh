#!/bin/bash
set -euo pipefail

app=${1:?Usage: build-meter.sh /path/to/GoXLRMeter.app [signing identity]}
identity=${2:--}
tools=$(cd "$(dirname "$0")" && pwd)
if [[ "$app" != *.app || -e "$app" || -L "$app" ]]; then
    echo 'Choose a new .app bundle path.' >&2
    exit 1
fi

mkdir -p "$app/Contents/MacOS"
cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>CFBundleIdentifier</key><string>dev.nichtlegacy.goxlr-meter</string>
    <key>CFBundleName</key><string>GoXLR Meter</string>
    <key>CFBundleExecutable</key><string>meter</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>LSUIElement</key><true/>
    <key>NSMicrophoneUsageDescription</key><string>Record GoXLR hardware and virtual inputs for comparison.</string>
</dict></plist>
PLIST

swiftc -O "$tools/meter.swift" -o "$app/Contents/MacOS/meter"
codesign --force --sign "$identity" --timestamp=none "$app"
codesign --verify --strict "$app"
