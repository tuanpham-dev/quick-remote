# A15 (X11): an existing X session with two monitors (Xephyr with two
# Xinerama heads, inside an Xvfb, as the stand-in; Debian's Xvfb ignores
# RandR monitors): the client lists both, and switching moves the capture
# to the other head (its blue window). Xephyr's heads are separate
# framebuffers, and reading the first one's window back gives black, so the
# first picture is only checked to be not the second head's.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bg host xvfb Xvfb :80 -screen 0 2600x800x24 -nolisten tcp
sleep 1
D=:79
bg host xephyr env DISPLAY=:80 Xephyr $D +xinerama -screen 1280x720 -screen 1280x720 -nolisten tcp
sleep 2
bg host red env GDK_BACKEND=x11 DISPLAY=$D python3 "$here/x11_color.py" red 0 0 1280 720
bg host blue env GDK_BACKEND=x11 DISPLAY=$D python3 "$here/x11_color.py" blue 1280 0 1280 720
sleep 1.5
start_host host x11host --pin 424242 --encoder x264enc --source x11:$D --fps 30 --audio none
dump() { # dump <name> [script]
	client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder openh264dec --audio-out none \
		--duration 5 ${2:+--script "$2"} --dump-frame "$LOGDIR/$1.raw" >"$LOGDIR/$1-client.txt" 2>&1
}
printf 'sleep 1500\nmonitor 1\n' >"$LOGDIR/a15.script"
dump first
dump switched "$LOGDIR/a15.script"
python3 - "$LOGDIR" >"$LOGDIR/a15-summary.txt" <<'PY'
import json, sys
import numpy as np
d = sys.argv[1]
def color(name):
    m = json.load(open(f"{d}/{name}.json"))
    b = np.fromfile(f"{d}/{name}.raw", dtype=np.uint8)
    w, h = m["width"], m["height"]
    off, st = m["offsets"], m["strides"]
    cx, cy = w // 4, h // 4  # chroma plane coordinates of the centre
    if m["format"] == "Nv12":
        u = b[off[1] + cy * st[1] + 2 * cx]; v = b[off[1] + cy * st[1] + 2 * cx + 1]
    else:
        u = b[off[1] + cy * st[1] + cx]; v = b[off[2] + cy * st[2] + cx]
    return f"{w}x{h}:" + ("red" if v > 200 and u < 140 else "blue" if u > 200 and v < 140 else f"u{u}v{v}")
print(f"first={color('first')} switched={color('switched')}")
PY
cat "$LOGDIR/a15-summary.txt"
mons=$(sed 's/\x1b\[[0-9;]*m//g' "$LOGDIR/first-client.txt" | grep -o 'monitors=[^ ]* [^ ]*' | head -1 || true)
check A15-x11 "two monitors listed ($mons); the stream starts on the first head and switching shows the second (blue): $(cat "$LOGDIR/a15-summary.txt")" \
	bash -c "grep -qE 'first=1280x720:[^b][a-z0-9]* switched=1280x720:blue' '$LOGDIR/a15-summary.txt' && grep -q 'head-1' <<<'$mons'"
