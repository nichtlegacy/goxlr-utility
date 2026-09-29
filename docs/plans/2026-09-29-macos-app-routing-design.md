# Per-app output routing and menubar volumes on macOS

**Status:** Approved 2026-09-29 (driver-based routing, native tray menu)

## Goal

From the GoXLR menubar icon, set the volume of the active GoXLR output channels
and, below them, assign each app that plays audio to a GoXLR output (System,
Game, Chat, Music, or Sample) with its own volume. Assignments persist per app
bundle and apply again whenever the app plays.

## Approach

The GoXLR virtual playback devices are our own AudioServer plug-in, so the
routing happens inside it instead of through process taps:

- CoreAudio offers plug-ins a per-client `ProcessOutput` operation that runs on
  each client's samples before the HAL mixes them. libASPL doesn't request it,
  so `GoXLRDevice` overrides `WillDoIOOperationImpl` to ask for it (in place)
  on visible playback devices and handles it in `DoIOOperationImpl`, passing
  every other operation to libASPL.
- For each client, the device looks up the client's PID in a rule table. It
  scales the samples by the rule's gain. If the rule routes the client to a
  different playback route, it adds the samples to that route's per-cycle
  scratch buffer and zeroes them, so the HAL mix of the source device no longer
  contains that app.
- In the same cycle's `WriteMix`, the source device publishes each scratch
  buffer to an injection history (one `StereoHistory` per source and target
  route, single writer: the source's IO thread).
- The target route's hidden bridge device, read by the daemon, sums its own
  history with the injection histories of every source. Each reader keeps a
  cursor per source and re-centres it if it falls too far behind, so starting
  or stopping a source doesn't add latency.

This needs no "Screen & System Audio Recording" permission, adds no aggregate
devices or taps, doesn't change what apps capture from each other, and adds
latency only for rerouted audio (the injection buffer, a few milliseconds).
It applies to apps that play to a GoXLR playback device; the system default
output is normally GoXLR System. Apps that play to another device are left
alone.

The rule table is a writable plug-in property (`gxap`, a string of
`pid:route:gain` entries), parsed off the real-time thread into fixed atomic
slots that the IO path scans without locks or allocation.

## Daemon

- A process monitor on the bridge thread reads
  `kAudioHardwarePropertyProcessObjectList` and, per process, its PID, bundle
  ID, whether it is running output, and its output devices. Helper processes
  (WebKit, Chrome, Electron) are mapped to their app through
  `responsibility_get_pid_responsible_for_pid`, falling back to the parent
  chain; names come from `NSRunningApplication`.
- Settings store rules per app bundle ID: target route (or none) and volume.
  The monitor expands them to PIDs and writes `gxap` whenever the result
  changes (and periodically, like the route mask, in case coreaudiod reloaded
  the plug-in).
- The latest app list and channel volumes are shared with the tray.

## Menubar

The existing tray menu is rebuilt each time it opens (`menuWillOpen:`):

1. Sliders for the GoXLR System, Game, Chat, and Music channels (and Sample
   when its route is enabled), sending `SetVolume` to the device.
2. One entry per app that is playing to a GoXLR output or has a rule, showing
   its current output. Its submenu picks the output and holds a volume slider.
3. The existing Configure, Open Path, and Quit items.

## Out of scope

Input routing, per-app EQ, and routing apps that play to non-GoXLR devices
(that would need process taps).

## Testing

- Unit tests for rule parsing, the scratch and injection buffers, and reader
  re-centring.
- On the Mac: route Safari and Music to different outputs and confirm on the
  GoXLR faders; change per-app volume; start and stop apps mid-playback;
  restart coreaudiod and the daemon; check coreaudiod CPU and IO overloads
  stay at today's level.
