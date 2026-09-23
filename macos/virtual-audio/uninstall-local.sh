#!/bin/sh
set -eu

target=/Library/Audio/Plug-Ins/HAL/GoXLRVirtual.driver
identifier=com.github.goxlr-on-linux.goxlr-virtual-audio

if [ "$(id -u)" -ne 0 ]; then
    echo 'Run as root after reviewing the removal path.' >&2
    exit 1
fi
if [ ! -d "$target" ] || [ -L "$target" ]; then
    echo "No regular GoXLR virtual driver bundle at $target" >&2
    exit 1
fi
actual=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$target/Contents/Info.plist")
if [ "$actual" != "$identifier" ]; then
    echo 'Refusing to remove a bundle with a different identifier.' >&2
    exit 1
fi

rm -rf -- "$target"
launchctl kickstart -k system/com.apple.audio.coreaudiod
echo "Removed $target"
