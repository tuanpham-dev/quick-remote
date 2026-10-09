#!/bin/bash
# A full XFCE desktop on the host's own virtual display:
#   qr-host --source "vdisplay:/path/to/xfce-desktop.sh"
#
# XFCE runs on a rootful, fullscreen Xwayland inside the virtual display.
# A watcher keeps the X screen at the display's pixel size and turns its
# scale (the client's pixel ratio) into XFCE's UI scale, so resizing the
# client window or changing the pixel ratio carries through.
#
# Chrome started in this session uses Vulkan: Chrome's OpenGL path crashes
# its GPU process on rootful Xwayland (WebGL then reports "not supported").
set -u
D="${QR_XFCE_DISPLAY:-:20}"
dir="$(mktemp -d "${TMPDIR:-/tmp}/qr-xfce.XXXXXX")"
log="$dir/log"

Xwayland "$D" -fullscreen -hidpi -noreset >>"$log" 2>&1 &
for _ in $(seq 100); do [[ -S /tmp/.X11-unix/X${D#:} ]] && break; sleep 0.1; done

# Chrome with Vulkan, for this session only: a wrapper first on PATH, and
# menu entries that point at it.
mkdir -p "$dir/bin" "$dir/share/applications"
cat >"$dir/bin/google-chrome-stable" <<'EOF'
#!/bin/sh
exec /usr/bin/google-chrome-stable --use-angle=vulkan --enable-features=Vulkan "$@"
EOF
chmod +x "$dir/bin/google-chrome-stable"
ln -sf google-chrome-stable "$dir/bin/google-chrome"
if [[ -f /usr/share/applications/google-chrome.desktop ]]; then
	sed "s|/usr/bin/google-chrome-stable|$dir/bin/google-chrome-stable|g" /usr/share/applications/google-chrome.desktop \
		>"$dir/share/applications/google-chrome.desktop"
fi

export DISPLAY="$D" XDG_SESSION_TYPE=x11 XDG_CURRENT_DESKTOP=XFCE
export PATH="$dir/bin:$PATH" XDG_DATA_DIRS="$dir/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
export QR_SWAYSOCK="${SWAYSOCK:-}" QR_XFCE_LOG="$log"
unset WAYLAND_DISPLAY

# Keeps the X screen at the output's pixel size; 2× becomes XFCE's window
# scaling, other ratios its DPI.
follow() {
	local last="" w h scale mode
	while sleep 0.5; do
		read -r w h scale < <(SWAYSOCK=$QR_SWAYSOCK swaymsg -t get_outputs -r 2>/dev/null |
			python3 -c 'import json,sys; o=json.load(sys.stdin)[0]; m=o["current_mode"]; print(m["width"], m["height"], o["scale"])')
		[[ -z ${w:-} || "$w $h $scale" == "$last" ]] && continue
		last="$w $h $scale"
		mode="${w}x${h}"
		if ! xrandr | grep -q "^ *$mode "; then
			xrandr --newmode "$mode" 0 "$w" 0 0 0 "$h" 0 0 0 2>/dev/null
			xrandr --addmode XWAYLAND0 "$mode"
		fi
		xrandr --output XWAYLAND0 --mode "$mode"
		if [[ $scale == 2* ]]; then
			xfconf-query -c xsettings -p /Gdk/WindowScalingFactor -n -t int -s 2
			xfconf-query -c xsettings -p /Xft/DPI -n -t int -s 96
		else
			xfconf-query -c xsettings -p /Gdk/WindowScalingFactor -n -t int -s 1
			xfconf-query -c xsettings -p /Xft/DPI -n -t int -s "$(python3 -c "print(round(96*$scale))")"
		fi
		echo "$(date +%T) X screen $mode scale $scale" >>"$QR_XFCE_LOG"
	done
}

exec dbus-run-session bash -c "$(declare -f follow); follow & exec startxfce4" >>"$log" 2>&1
