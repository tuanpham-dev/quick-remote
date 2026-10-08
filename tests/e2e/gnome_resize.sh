# A15-wayland-resize: the GNOME desktop's monitor changes size mid-stream
# (rotated through DisplayConfig: 1280x720 → 720x1280 and back) and the
# stream follows: the client sees the new size, frames keep coming, and
# input still reaches the desktop. mutter ends a monitor's stream when the
# monitor changes and starts none recorded later, so the host replaces its
# remote-desktop session; input and clipboard move to the new one.
# Needs the render group: sg render -c "tests/e2e/run.sh gnome_resize lan"
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
if [[ ! -w /dev/dri/renderD128 ]]; then
	blocked A15-wayland-resize "the GPU render node is not accessible (needs the render group)"
	return 0
fi
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cat >"$LOGDIR/gnome-inner.sh" <<EOS
echo "DBUS_SESSION_BUS_ADDRESS=\$DBUS_SESSION_BUS_ADDRESS" >"$LOGDIR/session.env"
echo "XDG_RUNTIME_DIR=\$XDG_RUNTIME_DIR" >>"$LOGDIR/session.env"
WAYLAND_DISPLAY=wayland-0 GDK_BACKEND=wayland python3 $here/wl_probe.py $LOGDIR/probe.jsonl >"$LOGDIR/probe.err" 2>&1 &
sleep 1.5
exec env GST_REGISTRY="$GST_REGISTRY" QR_GSO=0 "$BIN/qr-host" --listen 0.0.0.0:9450 --config-dir "$LOGDIR/ghost" --name ghost \
	--pin 424242 --encoder vah264lpenc --source mutter --fps 60 --audio none
EOS
bg host ghost "$here/gnome_session.sh" "/tmp/qr-gres-$$" "$LOGDIR" bash "$LOGDIR/gnome-inner.sh"
for _ in $(seq 150); do grep -q '^PIN ' "$LOGDIR/ghost.log" 2>/dev/null && break; sleep 0.1; done
rot() { on host env $(cat "$LOGDIR/session.env") python3 "$here/gnome_rotate.py" "$1" >>"$LOGDIR/rot.log" 2>&1; }
(sleep 4; rot left; sleep 5; rot none) &
rotations=$!
# Keys after both rotations (input through the replacement session).
printf 'sleep 13000\ntap 16\ntap 23\nsleep 1000\n' >"$LOGDIR/gres.script"
client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder vah264dec --audio-out none \
	--duration 16 --script "$LOGDIR/gres.script" --stats-json "$LOGDIR/gres.json" >"$LOGDIR/gres-client.txt" 2>&1
wait "$rotations" || true
python3 - "$LOGDIR/gres.json" >"$LOGDIR/gres-summary.txt" <<'PY'
import json, sys
L = [json.loads(l) for l in open(sys.argv[1])]
print(" ".join(f'{d["width"]}x{d["height"]}@{round(d["fps"])}' for d in L))
PY
cat "$LOGDIR/gres-summary.txt"
typed=$(python3 -c "import json,sys; print(''.join(e['char'] for e in map(json.loads, open(sys.argv[1])) if e['ev'] == 'key'))" "$LOGDIR/probe.jsonl")
check A15-wayland-resize "the stream follows the GNOME monitor's size: 1280x720 → 720x1280 → 1280x720, frames all along ($(cat "$LOGDIR/gres-summary.txt")); keys after it typed '$typed'" \
	python3 - "$LOGDIR/gres-summary.txt" "$typed" <<'PY'
import sys
seq = [s.split("@") for s in open(sys.argv[1]).read().split()]
sizes = []
for size, fps in seq:
    if not sizes or sizes[-1] != size:
        sizes.append(size)
tail = [int(f) for _, f in seq[-3:]]
sys.exit(0 if sizes[-3:] == ["1280x720", "720x1280", "1280x720"] and min(tail) >= 1 and sys.argv[2] == "qi" else 1)
PY
