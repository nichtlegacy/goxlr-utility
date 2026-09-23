# Full macOS GoXLR Audio Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose all GoXLR Full capture and playback channels as selectable macOS audio devices and retire Loopback from active routing after verification.

**Architecture:** Extend the existing AudioServer plug-in with 12 capture and 5 playback devices, each paired with a hidden bridge device. Keep one physical 23-channel input and one physical 10-channel output AudioUnit in the Rust daemon; dispatch and combine frames by the fixed hardware channel map.

**Tech Stack:** C++17, libASPL, CoreAudio HAL, Rust, coreaudio-rs, CMake, Cargo.

---

### Task 1: Mono-capable driver history

**Files:** `macos/virtual-audio/StereoHistory.hpp`, `macos/virtual-audio/Driver.cpp`, `macos/virtual-audio/tests/StereoHistory.cpp`.

- [ ] Extend the history constructor to accept `channels` of 1 or 2. Each ring slot may still pack at most two Float32 samples into its existing `uint64_t`; use `i * channels` for interleaved frame addressing and read/write only the configured channel count. Keep the generation protocol unchanged.
- [ ] Add a mono test beside the stereo tests: push `{1, 2, 3}` to a one-channel history, read three frames, and assert `{1, 2, 3}`. Run `ctest --test-dir /tmp/goxlr-virtual-audio-build --output-on-failure` to see the new test fail before implementing mono support, then pass afterward.
- [ ] Pass the route's channel count through `LoopbackHandler`, `stereoStream` (rename to `audioStream`), and `addDevice`. The callback frame count is `bytesCount / (channels * sizeof(float))`.

### Task 2: Complete virtual device table

**File:** `macos/virtual-audio/Driver.cpp`.

- [ ] Expand `Route` with `channels` and register the exact approved table in `docs/plans/2026-09-23-macos-full-audio-design.md`. Preserve the existing UIDs `GoXLRVirtual::Microphone`, `GoXLRVirtual::Chat`, and `GoXLRVirtual::Music`; use `GoXLRVirtual::<Name>` for new routes. `DryMic` has one channel and every other route has two.
- [ ] Build for arm64 and x86_64 using the existing CMake project. Verify both bundles with `codesign --verify --deep --strict` and inspect their device lists after a local installation.

### Task 3: Complete daemon channel dispatch

**File:** `daemon/src/platform/macos/audio_bridge.rs`.

- [ ] Add two fixed route tables. Capture offsets are `0,2,4,6,8,10,12,14,16,18,20,22` with Dry Mic at 22; playback offsets are `0,2,4,6,8`. Their UIDs must match Task 2's hidden bridge UIDs.
- [ ] Extend virtual ID discovery to require the 12 capture peers and 5 playback peers. Continue polling if the driver is absent, so a daemon started before the HAL driver can recover when devices appear.
- [ ] In the one physical input callback, send each frame's pair to its capture ring; for Dry Mic send `[frame[22], 0.0]`. In each capture peer's output callback, write one or two channels as declared by its route. Retain the current backlog and drift correction.
- [ ] In the one physical output callback, fill five route buffers and copy them to their 10-channel output offsets. Reuse fixed buffers across callbacks and avoid allocation, logging, or locks on the audio thread.
- [ ] Extend mapping tests to assert all 23 input indices, all 10 output indices, mono Dry Mic, and unchanged Microphone/Chat/Music assignments. Run the targeted Rust tests and an arm64 release build.

### Task 4: Local system validation and Loopback migration

**Files:** `macos/virtual-audio/README.md`; locally built bundle and test app only.

- [ ] Record the existing defaults and preserve the currently installed driver bundle for rollback. Stop the test daemon, replace only the GoXLRVirtual driver using the existing scoped uninstall/install scripts, then launch a newly signed local test app with microphone permission.
- [ ] Confirm 17 visible devices, the 12 input/5 output directions, 48 kHz, Mono Dry Mic, and stable old UIDs. Test low-level tones separately on System, Game, Chat, Music, and Sample; check each GoXLR path/fader. Check Microphone in Discord and speech-to-text, plus the other capture sources using available known signals.
- [ ] Restart the daemon and confirm recovery. Compare short CPU/RSS samples against the three-device baseline.
- [ ] Only after System audio succeeds, switch macOS default output and system sound from Loopback Chat to GoXLR System and verify real playback. Move explicit app selections from Loopback where found. Leave Loopback installed as rollback and document any unverified channels.
