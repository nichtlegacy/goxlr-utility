#!/bin/sh
set -eu

bundle=${1:?Usage: sudo install-local.sh /path/to/GoXLRVirtual.driver}
target=/Library/Audio/Plug-Ins/HAL/GoXLRVirtual.driver
identifier=com.github.goxlr-on-linux.goxlr-virtual-audio

if [ "$(id -u)" -ne 0 ]; then
    echo 'Run as root after reviewing the bundle and install path.' >&2
    exit 1
fi
if [ ! -d "$bundle" ] || [ -L "$bundle" ]; then
    echo 'Source must be a real GoXLRVirtual.driver directory.' >&2
    exit 1
fi
if [ -e "$target" ] || [ -L "$target" ]; then
    echo "Refusing to replace existing $target" >&2
    exit 1
fi
actual=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$bundle/Contents/Info.plist")
if [ "$actual" != "$identifier" ] || [ ! -f "$bundle/Contents/MacOS/GoXLRVirtual" ]; then
    echo 'The source is not the reviewed GoXLR virtual driver bundle.' >&2
    exit 1
fi
codesign --verify --deep --strict "$bundle"

mkdir -p /Library/Audio/Plug-Ins/HAL
ditto "$bundle" "$target"
chown -R root:wheel "$target"
codesign --verify --deep --strict "$target"
echo "Installed $target"
launchctl kickstart -k system/com.apple.audio.coreaudiod
