# Local macOS virtual audio build

The GoXLR Full exposes five stereo playback pairs and 23 capture channels through
17 selectable 48 kHz macOS devices. Microphone, System, Game, Chat, and Music
are visible by default; the other routes can be shown in Utility settings.
Dry Mic is mono. The channel map is in
`docs/plans/2026-09-23-macos-full-audio-design.md`.

The audio path needs both the HAL plug-in and the daemon bridge. The daemon runs
from a separate `GoXLR Audio Bridge.app` inside the Utility app so macOS treats
its microphone permission separately and opening `GoXLR Utility.app` still
opens the UI. The helper bundle includes `NSMicrophoneUsageDescription`.

## Build

Clone the `macos-ui-stability` branch of
[`nichtlegacy/goxlr-utility-ui-wrapper-app`](https://github.com/nichtlegacy/goxlr-utility-ui-wrapper-app/tree/macos-ui-stability)
alongside this repository, then run:

```sh
./ci/build-macos-local ../goxlr-utility-ui-wrapper-app /tmp/goxlr-local-build
```

The optional third argument is a local code-signing identity. Without it, the
script uses an ad-hoc signature. A stable signing identity is preferable for
microphone permission across rebuilds. The script builds the Rust utility and
UI wrapper, builds and tests the CMake driver, then verifies both signed bundles.
It writes `GoXLR Utility.app` and `GoXLRVirtual.driver` to the output directory.
The daemon serves the checked-in `daemon/web-content` UI; rebuild that directory
from the separate `goxlr-ui` repository when changing the UI source.

## Verify driver changes

Before installing a driver change, run the CMake tests for overlapping capture
reads and short playback streams on the candidate build. Use the
[audio measurement guide](tools/README.md) to record a live baseline with one and
two capture clients and with a short sound on an idle playback route. Repeat the
measurements after installation with the same routes and signal.

The [2026-11-22 incident note](incidents/2026-11-22-dry-mic.md) records the Dry Mic
failure, the update race, and the checks for silent macOS alerts.

## Local installation

Back up any existing app, HAL driver, and GoXLR LaunchAgent before replacing
them. Stop the running GoXLR LaunchAgent, install the built app in
`/Applications` and the driver in `/Library/Audio/Plug-Ins/HAL`, and restart
`coreaudiod`. Install `ci/macos/audio-bridge.launchagent.plist` as
`~/Library/LaunchAgents/com.github.goxlr-on-linux.goxlr-utility.plist`, then
bootstrap it in the user's GUI launchd domain. Grant **GoXLR Audio Bridge**
microphone access when macOS asks. The targeted `install-local.sh` and
`uninstall-local.sh` scripts remain available for first-time driver-only tests;
`install-local.sh` intentionally refuses to replace an existing driver.

Run driver-only updates of an existing installation from the logged-in user's
terminal.

```sh
./macos/virtual-audio/update-local.sh /tmp/goxlr-local-build/GoXLRVirtual.driver
```

The script requests sudo for the driver swap and CoreAudio restart. It saves the
previous driver under `/Library/Audio/Plug-Ins/GoXLRVirtual-backups` and waits for
the LaunchAgent to finish unloading before starting it again in the GUI domain.
Re-running it with the same signed driver starts the agent if needed.

Moving an app from another output to a GoXLR playback device uses a CoreAudio
process tap. Grant **System Audio Recording** access to GoXLR Audio Bridge when
macOS asks. An app already on its selected GoXLR device uses the driver's volume
rule without a tap. If tap creation fails, the app stays on its original output.

The existing release `.pkg` still lacks the HAL driver and nested helper app.
Use this local build path until that installer is updated. Existing legacy
Aggregate devices can remain enabled in settings and appear alongside the new
virtual devices; they are not required for these routes. Loopback can remain
installed as a fallback but is not part of this audio path.
