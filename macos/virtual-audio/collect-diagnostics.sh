#!/usr/bin/env bash
# Captures what CoreAudio and the GoXLR daemon were doing, for when audio hangs.
# Usage: collect-diagnostics.sh [minutes]   (default: last 5 minutes)
set -uo pipefail

minutes=${1:-5}
out="$HOME/Desktop/goxlr-audio-$(date +%Y%m%d-%H%M%S)"
logs="$HOME/Library/Application Support/org.GoXLR-on-Linux.GoXLR-Utility/logs"
mkdir -p "$out"

# Stack samples show where the daemon (tray, HTTP, bridge) and the UI are stuck.
for pid in $(pgrep -f "MacOS/goxlr-daemon") $(pgrep -f "MacOS/goxlr-utility-ui"); do
    sample "$pid" 3 -f "$out/sample-$pid.txt" >/dev/null 2>&1 &
done
wait
curl -s -m 5 -o /dev/null -w "GetStatus %{http_code} %{time_total}s\n" -X POST \
    -H 'Content-Type: application/json' -d '"GetStatus"' http://localhost:14564/api/command \
    > "$out/http.txt"

uptime > "$out/uptime.txt"
ps -Ao pid,%cpu,%mem,etime,comm -r | head -40 > "$out/top.txt"
ps -Ao pid,%cpu,etime,comm | grep -i -E "audio|goxlr|finetune|discord|astro" | grep -v grep > "$out/audio-processes.txt"
for i in 1 2 3 4 5 6 7 8 9 10; do
    ps -o %cpu= -p "$(pgrep -x coreaudiod)" | tr -d ' '
    sleep 1
done > "$out/coreaudiod-cpu.txt"
memory_pressure | tail -1 > "$out/memory.txt"
sysctl vm.swapusage >> "$out/memory.txt"
system_profiler SPAudioDataType > "$out/audio-devices.txt" 2>/dev/null

/usr/bin/log show --last "${minutes}m" --style compact \
    --predicate 'process == "coreaudiod" OR process == "audioanalyticsd" OR process CONTAINS "GoXLRVirtual"' \
    > "$out/coreaudio.log" 2>/dev/null
grep -E "restarting IO|Overload thread|stopping with error|Initialize failed" "$out/coreaudio.log" \
    > "$out/coreaudio-errors.log"
tail -n 3000 "$logs/goxlr-daemon.log" > "$out/goxlr-daemon.log" 2>/dev/null

echo "Saved to $out"
