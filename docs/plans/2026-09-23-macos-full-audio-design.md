# Full GoXLR Full audio routing on macOS

**Status:** Approved 2026-09-23

## Goal

Expose the GoXLR Full's five playback pairs and all 23 capture channels as
selectable 48 kHz macOS devices. Keep the existing `GoXLR Microphone` name and
its Chat Mic Mix source so Discord and speech-to-text selections continue to
work. Replace Loopback as the user's active route after testing; keep its
installation and configuration for rollback.

## Device layout

Each visible device has a hidden opposite-direction bridge peer in the existing
AudioServer plug-in. The daemon opens the physical GoXLR once for 23-channel
input and once for 10-channel output. It distributes input frames to the
capture peers and combines the five playback peers into the physical output.
The only mono device is Dry Mic. Other devices are stereo.

| Visible macOS device | Direction | Physical channels, 1-based |
| --- | --- | --- |
| GoXLR Broadcast Mix | Input | capture 1–2 |
| GoXLR Microphone | Input | capture 3–4, Chat Mic Mix |
| GoXLR Sampler Capture | Input | capture 5–6 |
| GoXLR Chat Mic | Input | capture 7–8 |
| GoXLR System Capture | Input | capture 9–10 |
| GoXLR Game Capture | Input | capture 11–12 |
| GoXLR Chat Capture | Input | capture 13–14 |
| GoXLR Music Capture | Input | capture 15–16 |
| GoXLR Sample Capture | Input | capture 17–18 |
| GoXLR Line-In | Input | capture 19–20 |
| GoXLR Console | Input | capture 21–22 |
| GoXLR Dry Mic | Input, mono | capture 23 |
| GoXLR System | Output | playback 1–2 |
| GoXLR Game | Output | playback 3–4 |
| GoXLR Chat | Output | playback 5–6 |
| GoXLR Music | Output | playback 7–8 |
| GoXLR Sample | Output | playback 9–10 |

The existing Microphone, Chat, and Music UIDs remain unchanged. Capture names
include `Capture` where they would otherwise collide with playback names.
The bridge converts these 1-based labels to zero-based frame indices.

## Alternatives

- One 23-input/10-output virtual device is compact, but Discord and many Mac
  apps cannot choose a channel pair.
- A hybrid of common stereo devices and a multichannel device reduces the
  device list, but leaves the remaining channels awkward to use.
- Individual devices match the requested selectable Windows-like routes. This
  adds more CoreAudio devices and callbacks, so callback cost and drift must be
  measured during the local test. This is the approved choice.

## Validation and migration

Keep the current driver and Loopback routes while building and testing the
expanded driver locally. Check enumeration, direction, channel count, UID
stability, and exact channel mapping. Play low-level tones separately through
System, Game, Chat, Music, and Sample; confirm the matching GoXLR path and
fader. Check Microphone in Discord and speech-to-text, and inspect the other
capture sources with known signals where practical. Restart the bridge and
verify routes recover without another microphone permission prompt.

After System output succeeds, switch macOS default output and system sounds
from Loopback Chat to GoXLR System, as the user requested. Verify audible
system sound and the apps that explicitly selected Loopback devices before
ceasing to use those devices. Loopback stays installed for rollback. The local
driver and test app are not a release installer.
