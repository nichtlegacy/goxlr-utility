# macOS virtual GoXLR devices

**Status:** Approved on 2026-09-23 for Microphone, Chat, and Music

## Evidence

On macOS 27 the GoXLR Full exposes 10 output and 23 input channels. The Utility can identify its CoreAudio device by matching a checked USB VID/PID and location ID to the CoreAudio UID. Temporary Chat and Music aggregates played on their corresponding GoXLR faders. A temporary Chat Mic aggregate reached Discord, but Discord's microphone test sounded severely broken while the existing two-channel Loopback Microphone sounded substantially better. The aggregate still advertised all 23 physical input channels. A command-line capture returned silence even from the built-in MacBook microphone, so it cannot explain the Discord failure.

## Decision

Build one macOS Audio Server Plug-in for the three requested virtual devices. Each visible device has exactly two channels, 48 kHz, and a stable UID. A userspace bridge owned by GoXLR Utility routes these pairs to the physical GoXLR:

| Visible macOS device | Direction | Physical GoXLR channels |
| --- | --- | --- |
| GoXLR Microphone | Input | Chat Mic 3/4 |
| GoXLR Chat | Output | Chat 5/6 |
| GoXLR Music | Output | Music 7/8 |

The plugin uses MIT-licensed libASPL. Its audio callbacks use preallocated bounded buffers and do not lock, allocate, log, or perform USB operations. A bridge outside `coreaudiod` opens the physical CoreAudio device and routes audio through the plugin's internal pairs. It uses the VID/PID-to-UID match before opening the device. It does not request exclusive/hog mode, so the existing Loopback setup can remain available during evaluation. When the GoXLR is absent, the bridge outputs silence and retries discovery without changing the macOS default devices.

This is one driver with three endpoints, not three independent drivers. Adding other GoXLR channels later means adding explicit channel mappings and endpoints to the same bridge and plugin. The first implementation has no UI for arbitrary channel maps.

## Alternatives considered

- Keep Chat and Music aggregates and add only a virtual microphone. This requires less initial code but leaves two different device models and different behavior across apps.
- Use an external virtual audio driver such as BlackHole with a bridge. That avoids building a plugin but adds a separate installation and does not provide integrated GoXLR device identities.

The existing `goxlr-aspl` repository describes itself as an unusable proof of concept, has no license in that repository, and assumes a different physical input count. Do not copy its code. libASPL itself is MIT-licensed.

## Delivery and validation

Build the plugin and bridge locally first. Do not replace the installed Utility or install a HAL plugin during this development step. Before installation, inspect the exact bundle, install path, and uninstall path. Installation or removal of a HAL plugin reloads macOS audio services and can interrupt live calls or playback.

Validation proceeds from pure channel-mapping and buffer tests to a plugin build, then to an explicit local installation and end-to-end test on the connected GoXLR. Check that each macOS device advertises two channels at 48 kHz, that the two playback devices reach only their corresponding faders, that Discord and speech-to-text receive clean Microphone audio, and that unplug/replug and Utility restart restore routing. Keep Loopback and current system defaults until these checks pass. The package must remove only its own driver bundle on rollback.
