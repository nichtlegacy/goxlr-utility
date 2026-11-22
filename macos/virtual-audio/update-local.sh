#!/bin/bash
set -euo pipefail

bundle=${1:?Usage: update-local.sh /path/to/GoXLRVirtual.driver}
plugins=/Library/Audio/Plug-Ins
target="$plugins/HAL/GoXLRVirtual.driver"
identifier=com.github.goxlr-on-linux.goxlr-virtual-audio
label=com.github.goxlr-on-linux.goxlr-utility
uid=$(id -u)
domain="gui/$uid"
service="$domain/$label"
agent="$HOME/Library/LaunchAgents/$label.plist"
binary=Contents/MacOS/GoXLRVirtual

if [[ "$uid" == 0 ]]; then
    echo 'Run as the logged-in user. The driver update requests sudo when needed.' >&2
    exit 1
fi
if [[ ! -f "$bundle/$binary" || ! -f "$target/$binary" || ! -f "$agent" ]]; then
    echo 'The build, installed driver and GoXLR LaunchAgent must all exist.' >&2
    exit 1
fi
actual=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$bundle/Contents/Info.plist")
if [[ "$actual" != "$identifier" ]]; then
    echo 'The source bundle must be the GoXLR virtual audio driver.' >&2
    exit 1
fi
/usr/bin/codesign --verify --deep --strict "$bundle"
/bin/launchctl print "$domain" >/dev/null

agent_loaded() {
    /bin/launchctl print "$service" >/dev/null 2>&1
}

start_agent() {
    local message
    # bootout can return while launchd is still removing the job.
    for ((i = 0; i < 40; i++)); do
        if agent_loaded; then
            if message=$(/bin/launchctl kickstart "$service" 2>&1); then
                return
            fi
        elif message=$(/bin/launchctl bootstrap "$domain" "$agent" 2>&1); then
            return
        fi
        sleep 0.25
    done
    printf '%s\n' "$message" >&2
    return 1
}

if /usr/bin/cmp -s "$bundle/$binary" "$target/$binary" &&
    /usr/bin/codesign --verify --deep --strict "$target"; then
    echo 'This driver build is already installed.'
    start_agent
    exit
fi

sudo -v
sudo -n /bin/mkdir -p "$plugins/GoXLRVirtual-backups"
backup_dir=$(sudo -n /usr/bin/mktemp -d "$plugins/GoXLRVirtual-backups/update.XXXXXXXX")
sudo -n /bin/chmod 755 "$backup_dir"
stage="$backup_dir/staged.driver"
backup="$backup_dir/GoXLRVirtual.driver"
sudo -n /usr/bin/ditto "$bundle" "$stage"
sudo -n /usr/sbin/chown -R root:wheel "$stage"
sudo -n /usr/bin/codesign --verify --deep --strict "$stage"

reload_audio() {
    sudo -n /bin/launchctl kickstart -k system/com.apple.audio.coreaudiod ||
        sudo -n /usr/bin/killall -TERM coreaudiod
}

restore_on_error() {
    local status=$?
    trap - EXIT
    if [[ -d "$backup" ]]; then
        if [[ -d "$target" ]]; then
            sudo -n /bin/mv "$target" "$stage" || exit "$status"
        fi
        sudo -n /bin/mv "$backup" "$target" || exit "$status"
        reload_audio || true
    fi
    start_agent || true
    exit "$status"
}
trap restore_on_error EXIT

if agent_loaded; then
    /bin/launchctl bootout "$service"
    for ((i = 0; i < 60; i++)); do
        if ! agent_loaded; then
            break
        fi
        sleep 0.25
    done
    if agent_loaded; then
        echo 'Timed out waiting for launchd to remove the GoXLR LaunchAgent.' >&2
        exit 1
    fi
fi

sudo -n /bin/mv "$target" "$backup"
sudo -n /bin/mv "$stage" "$target"
/usr/bin/cmp -s "$bundle/$binary" "$target/$binary"
reload_audio
trap - EXIT

printf 'Installed driver. Previous driver saved to %s\n' "$backup"
start_agent
