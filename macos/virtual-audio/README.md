# Local macOS virtual driver build

This development build maps the GoXLR Full's five stereo playback pairs and
23 capture channels to 17 selectable 48 kHz macOS devices. By default, only
Microphone, System, Game, Chat, and Music are visible. The other devices can
be shown or hidden live in GoXLR Utility settings. Dry Mic is mono; the other
11 capture devices are stereo. The channel map is in
`docs/plans/2026-09-23-macos-full-audio-design.md`. GoXLR Utility's local
daemon bridge is also required for audio to flow. Use the daemon built from this
branch for a local test, after stopping the running installed daemon.

Launch the daemon from a signed `.app` bundle with
`NSMicrophoneUsageDescription` in its `Info.plist`, then grant that app
Microphone access in macOS Privacy & Security. A daemon launched directly from
a terminal may inherit the terminal app's permission instead. Without access,
the bridge can start while the physical GoXLR input supplies only silence.

Build and inspect without installing:

```sh
cmake -S macos/virtual-audio -B /tmp/goxlr-virtual-audio-build -DCMAKE_BUILD_TYPE=Debug
cmake --build /tmp/goxlr-virtual-audio-build
ctest --test-dir /tmp/goxlr-virtual-audio-build --output-on-failure
plutil -p /tmp/goxlr-virtual-audio-build/GoXLRVirtual.driver/Contents/Info.plist
file /tmp/goxlr-virtual-audio-build/GoXLRVirtual.driver/Contents/MacOS/GoXLRVirtual
```

After a separate review and approval for the system change, install only this
bundle with `sudo macos/virtual-audio/install-local.sh
/tmp/goxlr-virtual-audio-build/GoXLRVirtual.driver`. The script refuses to
overwrite an existing bundle and restarts `coreaudiod`, which interrupts active
audio. To remove exactly this driver, use
`sudo macos/virtual-audio/uninstall-local.sh`; it checks the bundle identifier
before removal and also restarts `coreaudiod`.

Keep Loopback and the current default devices during testing. The release
installer does not yet ship this driver; package integration follows a clean
end-to-end Discord and speech-to-text test.
