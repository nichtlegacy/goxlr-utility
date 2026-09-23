# macOS GoXLR audio channels implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restore reliable GoXLR discovery on macOS 27, then determine whether the existing aggregate devices can replace Loopback for Microphone, Chat, and Music.

**Architecture:** Match an `IOUSBHostDevice` with GoXLR VID/PID to a CoreAudio UID using the USB `locationID`, which is embedded as a hexadecimal component in the observed `AppleUSBAudioEngine` UID. Keep aggregate creation and the user's Loopback configuration unchanged until a controlled channel test.

**Tech Stack:** Rust 2024, IOKit, CoreAudio HAL, macOS 27, existing `coreaudio-sys` and `io-kit-sys` dependencies.

---

### Task 1: Match USB location to audio UID

**Files:**
- Modify: `daemon/src/platform/macos/core_audio.rs`

- [x] Add a pure matcher for the observed CoreAudio UID format. Compare the full location component, rather than a substring or a display name:

```rust
fn uid_matches_location(uid: &str, location: u32) -> bool {
    let Some((prefix, _stream)) = uid.rsplit_once(':') else {
        return false;
    };
    let Some((prefix, component)) = prefix.rsplit_once(':') else {
        return false;
    };
    prefix.starts_with("AppleUSBAudioEngine:")
        && u32::from_str_radix(component, 16) == Ok(location)
}
```

- [x] Add focused unit cases in the same file: `1144400` matches `0x1144400`; `114440` does not; a Loopback UID containing the string does not; a second location `0x1120000` does not select the first device.
- [x] Run `cargo test -p goxlr-daemon matches_only_the_exact_usb_location_component` and expect the matcher case to pass.

### Task 2: Discover the physical GoXLR unambiguously

**Files:**
- Modify: `daemon/src/platform/macos/core_audio.rs`

- [x] Replace the `IOAudioEngine` service matcher with `IOUSBHostDevice`. Read `idVendor`, `idProduct`, and `locationID` only from each USB host device. Accept VID `0x1220` with PID `0x8fe0` or `0x8fe4`.
- [x] Enumerate HAL device IDs with `kAudioHardwarePropertyDevices` and read each device UID using the existing `get_uid_for_id` helper. Select one UID with `uid_matches_location`; return an error on two matches, and do not select by the name `GoXLR`.
- [x] Release IOKit iterators and objects on every path. Skip malformed USB entries without panicking. Use the verified PID to set the display name to `GoXLR` or `GoXLR Mini`.
- [x] Run `cargo fmt --check`, `cargo check -p goxlr-daemon`, and the focused matcher test. Expect success.

### Task 3: Validate the aggregate route before a driver

**Files:**
- No production file changes unless the test identifies a concrete defect.

- [x] Verify on macOS 27 that IOKit reports `0x1220:0x8fe0` and a location ID matching exactly one live CoreAudio UID. Record only the non-sensitive identifiers.
- [x] Preserve the current `macos_handle_aggregates = false` setting and Loopback configuration. Use a temporary, explicitly named aggregate test device that is destroyed after inspection.
- [ ] Inspect whether Chat exposes physical outputs 5/6, Music 7/8, and Chat Mic physical inputs 3/4 as usable two-channel endpoints. Then test each with channel-specific audio, without changing the system defaults.
- [ ] If all three work, document the user-path test and propose migration. If one fails, record the exact failure and create a separate implementation plan for a three-device AudioServer plug-in; do not install the current ASPL proof of concept.

### Task 4: Measure launch performance

**Files:**
- No production file changes until a measured hot path is identified.

- [ ] Measure cold-open time and CPU/RSS with the Utility running on the user's Mac. Capture the time to daemon readiness and UI visibility independently.
- [ ] Reproduce any slow path before changing launcher, polling, or sampler code; rerun the same measurement after a focused fix.
