# GoXLR audio measurements

The meter records the physical GoXLR Full and the virtual Dry Mic, Line-In, and
Microphone inputs at 48 kHz. It writes one float WAV per channel, callback sample
and host timestamps, and a log with frame counts, gaps, and a snapshot of
virtual input clients on macOS 14.2 or later. Keep recordings and logs in a
scratch directory outside Git.

## Build the meter

Run from the repository root. You need the macOS Swift toolchain, Python 3, and
NumPy for the comparator. Enable Dry Mic and Line-In in Utility settings and
confirm the physical GoXLR is visible to CoreAudio.

```sh
tools="$PWD/macos/virtual-audio/tools"
work=$(mktemp -d /tmp/goxlr-audio-check.XXXXXX)
app="$work/GoXLRMeter.app"
"$tools/build-meter.sh" "$app"
```

The build uses ad-hoc signing by default. Pass a local signing identity as the
second argument to keep the same signature identity across builds. On first
launch, allow **GoXLR Meter** to access the microphone. This is separate from the
GoXLR Audio Bridge permission.

## Measure capture with one and two clients

For the one-client run, close other consumers of the routes under test. Keep a
varying signal on the channel to avoid counting silence as evidence.

```sh
python3 "$tools/run-meter.py" "$app" 5 "$work/one"

python3 "$tools/run-meter.py" "$app" 14 "$work/two-companion" &
companion=$!
sleep 2
python3 "$tools/run-meter.py" "$app" 5 "$work/two"
wait "$companion"
```

The launcher uses separate app instances. Check `run.txt` in each capture for
the actual client PIDs, callback sizes, and timestamp gaps. On a healthy run,
Dry Mic and Line-In should have one reader in `one` and two in `two`. Record
other clients such as Discord separately. If the companion is waiting for a
first-run permission prompt or the two PIDs are missing, repeat the run once
the app has permission.

The app has a watchdog at the requested duration plus 15 seconds. The launcher
bounds `open -W` at duration plus 20 seconds, since Launch Services can keep
waiting after recording. A run requires the meter's completion message and uses
a fresh output directory.

Compare the physical and virtual streams from the same run. Channel numbers are
zero-based.

```sh
for run in one two; do
    python3 "$tools/cmp.py" --bridge-interpolation \
        "$work/$run/physical-ch22.wav" "$work/$run/virtual-drymic-ch0.wav"
    python3 "$tools/cmp.py" --bridge-interpolation \
        "$work/$run/physical-ch18.wav" "$work/$run/virtual-linein-ch0.wav"
    python3 "$tools/cmp.py" --bridge-interpolation \
        "$work/$run/physical-ch2.wav" "$work/$run/virtual-microphone-ch0.wav"
done
```

The comparator aligns by integer lag and reports tolerant and bit-exact matches,
runs of 16 or more successive mismatches, and exact 416/96 self-repeats in
nonconstant 512-frame blocks. Check for timestamp gaps, periodic repeats, and
alignment changes between runs. The optional bridge comparison fits a single
constant interpolation phase to account for clock correction. It cannot
reconstruct a phase that changes during the recording. Inspect the raw
comparison as well.

## Measure a short sound on the physical GoXLR

Run the CMake tests before installing driver changes. The playback callback
test covers an idle output and one with a background client, including delivery
after the short client stops. For the live check, choose a virtual GoXLR output
as the macOS default and save the selection to restore afterwards. Ensure the
output is idle before testing the first short sound. The following commands
record and check **GoXLR Game**.

```sh
afconvert -f WAVE -d LEF32@48000 -c 1 /System/Library/Sounds/Tink.aiff "$work/tink.wav"
python3 "$tools/run-meter.py" "$app" 8 "$work/tink-idle" &
meter=$!
sleep 2
afplay -v 0.2 /System/Library/Sounds/Tink.aiff
wait "$meter"
python3 "$tools/cmp.py" --tone "$work/tink.wav" "$work/tink-idle/physical-ch10.wav"
```

Repeat in a fresh directory with a continuous, quiet stream playing on that
same output. Compare the correlation, onset, and gain of the matched tone.
Mixing background audio can lower the correlation. Expect to find the tone
in both runs near the actual playback time. Use physical channel 8 for System,
10 for Game, 12 for Chat, 14 for Music, and 16 for Sample. Check channel 0 as
well when the tested route is sent to Broadcast Mix. Restore the previous
default output and test a native macOS alert separately.
