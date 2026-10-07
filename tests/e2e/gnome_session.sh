#!/bin/bash
# A headless GNOME (mutter) session on a private bus, as the stand-in for a
# user's desktop, running a command inside it (tests):
#   gnome_session.sh <runtime-dir> <log-dir> <command...>
# The runtime dir must be short (Unix socket paths are limited to 108 bytes).
rt="$1" logs="$2"
shift 2
rm -rf "$rt"
mkdir -m 700 -p "$rt"
export XDG_RUNTIME_DIR="$rt"
exec dbus-run-session -- bash -c '
logs="$1"; shift
pipewire >"$logs/gnome-pipewire.log" 2>&1 & pids=$!
sleep 0.5
wireplumber >"$logs/gnome-wireplumber.log" 2>&1 & pids="$pids $!"
mutter --headless --virtual-monitor 1280x720 --wayland --no-x11 >"$logs/gnome-mutter.log" 2>&1 & pids="$pids $!"
for _ in $(seq 100); do [[ -S $XDG_RUNTIME_DIR/wayland-0 ]] && break; sleep 0.1; done
sleep 1
"$@"
kill $pids 2>/dev/null
' gnome "$logs" "$@"
