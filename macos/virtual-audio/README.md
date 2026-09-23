# Local macOS virtual driver build

This development build exposes three 48 kHz stereo devices: GoXLR Microphone
(input), GoXLR Chat (output), and GoXLR Music (output). GoXLR Utility's local
daemon bridge is also required for audio to flow. Use the daemon built from this
branch for a local test, after stopping the running installed daemon.

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
