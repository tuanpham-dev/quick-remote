# A15-kde-resize: the KDE desktop's monitor changes size mid-stream (KWin's
# virtual output has one mode, so it's rotated: 1280x720 → 720x1280 and
# back) and the stream follows: the client sees the new size, frames keep
# coming. Needs the render group: sg render -c "tests/e2e/run.sh kde_resize lan"
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
if [[ ! -w /dev/dri/renderD128 ]] || ! command -v kscreen-doctor >/dev/null || ! command -v kwin_wayland >/dev/null; then
	blocked A15-kde-resize "needs the render group, KWin and kscreen-doctor (apt install kwin-wayland libkscreen-bin)"
	return 0
fi
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cat >"$LOGDIR/kde-inner.sh" <<EOS
WAYLAND_DISPLAY=wayland-0 gst-launch-1.0 -q videotestsrc pattern=ball is-live=true \
	! video/x-raw,width=640,height=360,framerate=60/1 ! waylandsink fullscreen=true >"$LOGDIR/anim.log" 2>&1 &
( while [ ! -e "$LOGDIR/go" ]; do sleep 0.1; done
  sleep 4; WAYLAND_DISPLAY=wayland-0 kscreen-doctor output.Virtual-0.rotation.left >"$LOGDIR/rot1.log" 2>&1
  sleep 5; WAYLAND_DISPLAY=wayland-0 kscreen-doctor output.Virtual-0.rotation.none >"$LOGDIR/rot2.log" 2>&1 ) &
sleep 1.5
exec env GST_REGISTRY="$GST_REGISTRY" QR_GSO=0 WAYLAND_DISPLAY=wayland-0 "$BIN/qr-host" --listen 0.0.0.0:9450 \
	--config-dir "$LOGDIR/khost" --name khost --pin 424242 --encoder vah264lpenc --source wayland --fps 60 --audio none
EOS
bg host khost "$here/kde_session.sh" "/tmp/qr-kres-$$" "$LOGDIR" bash "$LOGDIR/kde-inner.sh"
for _ in $(seq 150); do grep -q '^PIN ' "$LOGDIR/khost.log" 2>/dev/null && break; sleep 0.1; done
touch "$LOGDIR/go" # rotations 4 and 9 s from now
client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder vah264dec --audio-out none \
	--duration 16 --stats-json "$LOGDIR/kres.json" >"$LOGDIR/kres-client.txt" 2>&1
python3 - "$LOGDIR/kres.json" >"$LOGDIR/kres-summary.txt" <<'PY'
import json, sys
L = [json.loads(l) for l in open(sys.argv[1])]
print(" ".join(f'{d["width"]}x{d["height"]}@{round(d["fps"])}' for d in L))
PY
cat "$LOGDIR/kres-summary.txt"
check A15-kde-resize "the stream follows the KDE monitor's size: 1280x720 → 720x1280 → 1280x720, frames all along ($(cat "$LOGDIR/kres-summary.txt"))" \
	python3 - "$LOGDIR/kres-summary.txt" <<'PY'
import sys
seq = [s.split("@") for s in open(sys.argv[1]).read().split()]
sizes = []
for size, fps in seq:
    if not sizes or sizes[-1] != size:
        sizes.append(size)
tail = [int(f) for _, f in seq[-3:]]
sys.exit(0 if sizes[-3:] == ["1280x720", "720x1280", "1280x720"] and min(tail) >= 40 else 1)
PY
