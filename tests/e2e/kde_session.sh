#!/bin/bash
# A headless KDE (KWin) session on a private bus, as the stand-in for a
# user's Plasma desktop, running a command inside it (tests):
#   kde_session.sh <runtime-dir> <log-dir> <command...>
# The runtime dir must be short (Unix socket paths are limited to 108 bytes).
# KWin's screencast plugin comes from the system (kwin-common), else from
# scripts/kwin-test-plugin.sh's cache. KWIN_WAYLAND_NO_PERMISSION_CHECKS=1
# opens KWin's restricted protocols to every client; otherwise a program
# needs a desktop file listing them, looked up in the session's own
# XDG_DATA_HOME (<runtime-dir>/data). KDE finds applications through the
# XDG menu, which Plasma ships: a minimal one when the system has none.
rt="$1" logs="$2"
shift 2
rm -rf "$rt"
mkdir -m 700 -p "$rt"
cache="${XDG_CACHE_HOME:-$HOME/.cache}/quickremote/kwin-plugins"
if ! compgen -G "/usr/lib/*/qt6/plugins/kwin/plugins/screencast.so" >/dev/null && [[ -d $cache ]]; then
	export QT_PLUGIN_PATH="$cache${QT_PLUGIN_PATH:+:$QT_PLUGIN_PATH}"
fi
mkdir -p "$rt/data/applications" "$rt/cache" "$rt/config/menus"
export XDG_DATA_HOME="$rt/data" XDG_CACHE_HOME="$rt/cache"
if [[ ! -f /etc/xdg/menus/applications.menu ]]; then
	cat >"$rt/config/menus/applications.menu" <<'MENU'
<!DOCTYPE Menu PUBLIC "-//freedesktop//DTD Menu 1.0//EN" "http://www.freedesktop.org/standards/menu-spec/1.0/menu.dtd">
<Menu><Name>Applications</Name><DefaultAppDirs/><DefaultDirectoryDirs/><Include><All/></Include></Menu>
MENU
	export XDG_CONFIG_DIRS="$rt/config:${XDG_CONFIG_DIRS:-/etc/xdg}"
fi
export XDG_RUNTIME_DIR="$rt"
exec dbus-run-session -- bash -c '
logs="$1"; shift
pipewire >"$logs/kde-pipewire.log" 2>&1 & pids=$!
sleep 0.5
wireplumber >"$logs/kde-wireplumber.log" 2>&1 & pids="$pids $!"
QT_QPA_PLATFORM=offscreen kwin_wayland --virtual --width 1280 --height 720 --socket wayland-0 --no-lockscreen \
	--no-global-shortcuts --no-kactivities >"$logs/kde-kwin.log" 2>&1 & pids="$pids $!"
for _ in $(seq 100); do [[ -S $XDG_RUNTIME_DIR/wayland-0 ]] && break; sleep 0.1; done
sleep 1
"$@"
kill $pids 2>/dev/null
' kde "$logs" "$@"
