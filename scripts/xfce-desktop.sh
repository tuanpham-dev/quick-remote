#!/bin/bash
# A full XFCE desktop on the host's own virtual display:
#   qr-host --source "vdisplay:/path/to/xfce-desktop.sh"
#
# The maintained form of this is built into the host:
#   qr-host --source vdisplay --desktop xfce4-session [--keyboard-layout de --dpi 120]
# (crates/qr-host/src/xdesktop.rs), which also forwards the pointer's shape.
# This script stays for `vdisplay:<script>` users and as a reference.
#
# XFCE runs on a rootful, fullscreen Xwayland inside the virtual display.
# A watcher keeps the X screen at the size of its window and XFCE's UI
# scale in step with the display's, so resizing the client window or
# changing the pixel ratio carries through.
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
exec /usr/bin/google-chrome-stable --use-angle=vulkan --enable-features=Vulkan,VulkanFromANGLE,DefaultANGLEVulkan "$@"
EOF
chmod +x "$dir/bin/google-chrome-stable"
ln -sf google-chrome-stable "$dir/bin/google-chrome"
if [[ -f /usr/share/applications/google-chrome.desktop ]]; then
	sed "s|/usr/bin/google-chrome-stable|$dir/bin/google-chrome-stable|g" /usr/share/applications/google-chrome.desktop \
		>"$dir/share/applications/google-chrome.desktop"
fi

export DISPLAY="$D" XDG_SESSION_TYPE=x11 XDG_CURRENT_DESKTOP=XFCE
export PATH="$dir/bin:$PATH" XDG_DATA_DIRS="$dir/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
export SOCK="${SWAYSOCK:-}" LOGF="$log"
unset WAYLAND_DISPLAY

# Keeps the X screen at the size of the Xwayland window's buffer, and
# XFCE's scale at the buffer scale.
follow() {
	local last="" w h k mode
	while sleep 0.5; do
		# The Xwayland window's buffer: the desktop (logical) size times
		# the integer scale sway gives Xwayland (the output scale rounded
		# up; 1 below 1×, where sway shrinks the picture to fewer pixels).
		read -r w h k < <(SWAYSOCK=$SOCK swaymsg -t get_outputs -r 2>/dev/null |
			python3 -c 'import json,math,sys; o=json.load(sys.stdin)[0]; r=o["rect"]; k=max(1,math.ceil(o["scale"]-1e-6)); print(r["width"]*k, r["height"]*k, k)')
		[[ -z ${w:-} || "$w $h $k" == "$last" ]] && continue
		last="$w $h $k"
		mode="${w}x${h}"
		if ! xrandr | grep -q "^ *$mode "; then
			xrandr --newmode "$mode" 0 "$w" 0 0 0 "$h" 0 0 0 2>/dev/null
			xrandr --addmode XWAYLAND0 "$mode"
		fi
		xrandr --output XWAYLAND0 --mode "$mode"
		xfconf-query -c xsettings -p /Gdk/WindowScalingFactor -n -t int -s "$k"
		xfconf-query -c xsettings -p /Xft/DPI -n -t int -s 96
		echo "$(date +%T) X screen $mode scale $k" >>"$LOGF"
	done
}

exec dbus-run-session bash -c "$(declare -f follow); follow & exec startxfce4" >>"$log" 2>&1
