# Full GoXLR Full audio routing on macOS

**Status:** Approved 2026-09-23; device visibility revised after the full local enumeration test

## Goal

Make the GoXLR Full's five playback pairs and all 23 capture channels available
as selectable 48 kHz macOS devices. Show only Microphone input and System,
Game, Chat, and Music outputs by default. Let the user show or hide the other
routes with switches in GoXLR Utility. Keep the existing `GoXLR Microphone`
name and Chat Mic Mix source so Discord and speech-to-text selections continue
to work. Replace Loopback as the user's active route after testing; keep its
installation and configuration for rollback.

## Device layout

Each visible device has a hidden opposite-direction bridge peer in the existing
AudioServer plug-in. The daemon opens the physical GoXLR once for 23-channel
input and once for 10-channel output. It distributes input frames to the
capture peers and combines the five playback peers into the physical output.
The only mono device is Dry Mic. Other devices are stereo. All 17 routes stay
registered in the plug-in, but the visible device of a disabled route is hidden
from macOS. Its hidden bridge peer stays hidden. The daemon opens audio units
only for enabled routes to avoid spending CPU on unused channels.

The driver exposes a writable CoreAudio plug-in property carrying the 17-bit
enabled-route mask. The daemon persists the mask in Utility settings, applies
it at startup and after changes, and refreshes it if CoreAudio reloads the
plug-in. The UI source lives in the separate `goxlr-utility-ui` repository;
the existing daemon IPC and `DaemonConfig` settings path carries the switch
state. The initial mask enables Microphone, System, Game, Chat, and Music.
Live visibility changes must be proved on macOS before relying on this path.

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
- Individual devices match the requested selectable Windows-like routes. The
  full local enumeration test exposed too many unused devices, so the approved
  choice hides them by default and activates only selected audio routes.

For device switches, reloading `coreaudiod` on every change would interrupt
audio and require administrator authentication. Fixed five devices would not
allow later channel selection. The approved choice is a live CoreAudio control
property, persisted by the Utility and presented in its editable UI source.

## Validation and migration

The expanded driver has already enumerated all 17 routes locally, with the
correct directions, 48 kHz rates, and mono Dry Mic. System output was audible
and controlled by the System fader. Next, verify live hide/show without an
audio-service restart; check that disabled routes disappear from Mac device
lists and consume no bridge audio units. Then test the five default routes and
enable a sample optional route through the UI. Check Microphone in Discord and
speech-to-text, and inspect other capture sources with known signals where
practical. Restart the bridge and verify the selection and microphone recover.

After System output succeeds, switch macOS default output and system sounds
from Loopback Chat to GoXLR System, as the user requested. Verify audible
system sound and the apps that explicitly selected Loopback devices before
ceasing to use those devices. Loopback stays installed for rollback. The local
driver and test app are not a release installer.
