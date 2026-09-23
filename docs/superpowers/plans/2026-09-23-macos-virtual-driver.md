# macOS Virtual GoXLR Devices Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build locally a single macOS audio driver and Utility bridge that expose Microphone, Chat, and Music as real stereo devices.

**Architecture:** An MIT-licensed libASPL Audio Server Plug-in publishes three visible 48 kHz stereo devices with stable UIDs. Its hidden companion streams connect to a GoXLR Utility bridge, which maps the streams to the physical device discovered by checked USB VID/PID and location ID. The current Loopback and aggregate configurations remain usable until the driver passes an end-to-end test.

**Tech Stack:** C++17, libASPL, CMake, CoreAudio HAL and AudioUnit, Rust daemon, macOS 27.

---

**Local build status (2026-09-23):** Both macOS driver architectures build and
their bundles pass strict ad hoc signature verification. The ring-buffer test
passes with AddressSanitizer, UndefinedBehaviorSanitizer, and ThreadSanitizer;
the daemon tests and arm64 release build pass, and the x86_64 daemon checks.
No HAL bundle has been installed. Device enumeration, live stream formats,
Discord audio, hotplug recovery, and package integration remain to be tested.
`coreaudio-rs` can resize its input callback buffer if CoreAudio changes the
hardware period; remove that allocation path before a production release.

## File map

- `macos/virtual-audio/CMakeLists.txt`: build the Audio Server Plug-in bundle with a pinned libASPL revision.
- `macos/virtual-audio/Info.plist.in`: bundle identifier, entry point, and macOS HAL metadata.
- `macos/virtual-audio/StereoHistory.hpp`: bounded lock-free frame history with one writer and per-client read cursors.
- `macos/virtual-audio/Driver.cpp`: three visible devices, their hidden companion streams, and real-time I/O handlers.
- `macos/virtual-audio/tests/StereoHistory.cpp`: wraparound, underflow, overrun, and two-reader tests.
- `daemon/src/platform/macos/audio_bridge.rs`: physical GoXLR and virtual-device AudioUnits; routing and lifecycle.
- `daemon/src/platform/macos/core_audio.rs`: expose UID-to-device-ID lookup and the existing checked USB match to the bridge.
- `daemon/src/platform/macos/runtime.rs`: run the bridge independently of the optional legacy aggregate manager.
- `ci/build-macos-package` and macOS package metadata: include the built driver bundle and its license, with an exact uninstall path.

## Task 1: Pin and build a silent driver

- [ ] Add `macos/virtual-audio/CMakeLists.txt` with libASPL fetched from `https://github.com/gavv/libASPL.git` at commit `633e0f70203edd87d320fc5a3cae901e1363aac5`; use C++17 and build one bundle target `GoXLRVirtual.driver` linked with libASPL, CoreAudio, and CoreFoundation. Carry libASPL's MIT license in the package.
- [ ] Add `Info.plist.in` with bundle ID `com.github.goxlr-on-linux.goxlr-virtual-audio`, entry point `GoXLRVirtualEntryPoint`, and the AudioServerPlugIn type.
- [ ] In `Driver.cpp`, create fixed visible UIDs `GoXLRVirtual::Microphone`, `GoXLRVirtual::Chat`, and `GoXLRVirtual::Music`, each with one two-channel stream at exactly 48 kHz. Create a hidden partner with suffix `::Bridge` for each route; pair the visible and bridge sides inside the plugin. Do not expose other GoXLR routes.
- [ ] Run `rtk proxy cmake -S macos/virtual-audio -B /tmp/goxlr-virtual-audio-build -DCMAKE_BUILD_TYPE=Debug`, then `rtk proxy cmake --build /tmp/goxlr-virtual-audio-build`. Verify the bundle has the expected Info.plist and `arm64` binary. No system install in this task.

## Task 2: Transport stereo frames within the plugin

- [ ] In `tests/StereoHistory.cpp`, cover a two-channel history with these exact cases: 128 frames written/read in order; read from empty history fills zero; a lagged cursor skips overwritten complete frames; wraparound keeps left/right pairs together; two readers independently receive the same microphone frames.
- [ ] Implement `StereoHistory.hpp` with a power-of-two frame capacity, an atomic published frame count, and atomic packed stereo slots with generation markers. `push(const float* interleaved, size_t frames)` and `read(Cursor&, float* interleaved, size_t frames)` must never allocate, lock, log, or call the OS. `read` fills missing frames with zero and advances only its own cursor. A lagged cursor resumes at the oldest still-available complete frame; generation checks reject a slot being overwritten during a read.
- [ ] In `Driver.cpp`, give each visible/hidden pair one history. `OnWriteMixedOutput` publishes stereo samples; `OnReadClientInput` reads using a cursor stored in a custom `aspl::Client` created by `OnAddClient` on the control thread. Use a packed 32-bit float, 48-kHz, two-channel `AudioStreamBasicDescription` for every stream. Do not invoke device-management setters from an I/O callback.
- [ ] Run the ring unit test and the plugin build with address/undefined-behavior sanitizers in a local test target. Expect every case to pass; keep the real-time code free of `std::mutex`.

## Task 3: Resolve the physical and virtual devices

- [ ] Reuse `get_goxlr_devices()` from `core_audio.rs`; its USB VID/PID and location-ID comparison is the only way to choose the physical GoXLR. If none or more than one suitable physical device is present, leave the bridge stopped and report one bounded status line.
- [ ] Add `get_audio_device_id_for_uid(uid: &str) -> Result<AudioDeviceID>` based on `kAudioHardwarePropertyDeviceForUID`. Use it for the three `GoXLRVirtual::*::Bridge` UIDs, never display names or device-list indices. Verify a missing plugin returns `None` to the bridge without changing current audio devices.
- [ ] Add a macOS-only test for the three fixed route descriptors: `Microphone = input 3/4`, `Chat = output 5/6`, `Music = output 7/8`, using one-based physical channel numbers. Check the zero-based buffer offsets `2/3`, `4/5`, and `6/7` respectively.

## Task 4: Bridge physical and virtual audio

- [ ] Add one physical input AudioUnit for all 23 GoXLR channels and one physical output AudioUnit for all 10 channels, both at 48 kHz. Add one companion AudioUnit per plugin route. CoreAudio callbacks use preallocated buffers and bounded single-producer/single-consumer queues; no `Mutex`, heap allocation, logging, or USB calls inside callbacks.
- [ ] On physical capture, copy channels 3/4 to the Microphone companion output. On physical render, initialize all 10 channels to zero, then place Chat samples in 5/6 and Music samples in 7/8. Leave other channels silent in this bridge so the existing CoreAudio mixer can continue serving other clients.
- [ ] Start the bridge only after all required virtual UIDs and the checked physical UID resolve. Stop AudioUnits in reverse order on shutdown, release buffers, and retry resolution after unplug/replug. Do not use CoreAudio hog mode or change system defaults.
- [ ] Test the pure interleaved-frame mapper with distinguishable values in every source channel and assert exact destinations and zeroed unused channels. Run `rtk proxy env CARGO_HOME=/tmp/goxlr-cargo RUSTUP_HOME=/tmp/goxlr-rustup /tmp/goxlr-cargo/bin/cargo test -p goxlr-daemon` on macOS.

## Task 5: Integrate and package without installing

- [ ] Launch the bridge from `runtime.rs` independently of `macos_handle_aggregates`, preserving the legacy aggregate setting and all existing GoXLR control tasks. If the plugin is absent, exit the bridge task cleanly and leave audio unchanged.
- [ ] Add the driver bundle to the macOS package build and include a post-install registration/reload step only after verifying the bundle path and uninstall script. The uninstall path must be exactly `/Library/Audio/Plug-Ins/HAL/GoXLRVirtual.driver`; never remove another HAL bundle.
- [ ] Build the daemon and driver for `aarch64-apple-darwin` and `x86_64-apple-darwin`. Inspect bundle metadata, architecture, embedded dependency licenses, package file list, and rollback script. Do not install the driver or replace the running Utility as part of this plan's local-build phase.

## Task 6: Mac validation after a separately approved installation

- [ ] Record the existing default input/output and Loopback configuration, install only the reviewed bundle, and verify the three visible UIDs advertise two channels at 48 kHz. Verify the hidden companions are absent from normal app device menus.
- [ ] Play separate channel-specific tones and check that only the Chat or Music fader controls its corresponding tone. In Discord compare the new Microphone with the current Loopback Microphone using the same settings; repeat with speech-to-text.
- [ ] Unplug/replug the GoXLR and restart Utility; check that the three device UIDs remain stable and their audio routes recover. Compare CPU and launch readiness against the pre-change measurements. Remove the driver by its exact path if a test fails and verify Loopback still works.
