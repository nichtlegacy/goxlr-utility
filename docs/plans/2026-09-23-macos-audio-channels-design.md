# macOS GoXLR audio channels

**Status:** Approved for the first three channels on 2026-09-23

## Goal

Expose the GoXLR Full's Chat and Music playback pairs and Chat Mic capture pair as reliable macOS devices. Keep the user's Loopback configuration usable until the replacement passes an end-to-end test. Investigate startup performance separately with measurements.

## Current state

- The physical GoXLR reports 10 output and 23 input channels at 48 kHz on macOS 27.
- The Utility can create aggregate devices, but `get_goxlr_devices` searches `IOAudioEngine`. macOS 27 exposes the GoXLR as an `IOUSBHostDevice`; the audio device's CoreAudio UID contains its USB `locationID` in hexadecimal.
- Aggregate management is disabled in the user's settings. Loopback currently maps Chat to physical outputs 5/6 and Microphone from physical inputs 3/4. The existing Music device captures selected applications. Loopback devices run at 44.1 kHz.
- The upstream `goxlr-aspl` project calls itself an unusable proof of concept. Its helper still uses `IOAudioEngine` and selects only the first detected GoXLR.

## Decision and alternatives

First, restore unambiguous USB-to-CoreAudio device discovery in the Utility. Enumerate USB devices by VID/PID, read `locationID`, and match only a CoreAudio UID whose location component equals that ID. Reject absent or ambiguous matches. This meets the maintainer's identity requirement and lets us test the existing aggregate mechanism before adding a driver.

An AudioServer plug-in with a userspace bridge is the second option if macOS 27 aggregates do not expose or route the three stereo pairs correctly. It would require installation, lifecycle management, real-time-safe buffering, and packaging. Reusing the current ASPL code without addressing those gaps is not acceptable. Loopback remains the fallback during testing.

## Validation

1. Verify USB VID/PID and location ID against the live CoreAudio UID on macOS 27. Test that two different location IDs cannot select the same audio device.
2. Build the changed daemon and test aggregate creation without changing system defaults. Check Chat 5/6, Music 7/8, and Chat Mic 3/4 with a channel-specific signal.
3. Only after those checks, compare Discord and speech-to-text input, restart/replug behavior, and measured CPU/start time. If the aggregate route fails, design the three-device AudioServer plug-in implementation.

## Rollback

Leave `macos_handle_aggregates` disabled and the existing Loopback devices in place until the replacement is proven. Any temporary aggregate test device must be destroyed after the test.

## macOS 27 result

The USB-to-CoreAudio matcher found the connected GoXLR Full. Temporary Chat and Music aggregates played 48 kHz stereo test tones; the user confirmed that their corresponding GoXLR faders changed the tones. CoreAudio still reports 10 outputs and 23 inputs for these aggregates. FFmpeg received zero samples from the Chat Mic aggregate, the existing Loopback Microphone, and the built-in MacBook microphone, so command-line capture could not assess the microphone. In Discord the Chat Mic aggregate produced a moving input meter, but its microphone test sounded severely broken. The existing Loopback Microphone sounded substantially better in the same test. All temporary devices were removed and system defaults stayed unchanged. The approved next step is a single Audio Server Plug-in exposing three true stereo devices, documented in `2026-09-23-macos-virtual-driver-design.md`.
