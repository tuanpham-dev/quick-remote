# A15-kde-fps: an animated KDE desktop (a full-screen test pattern on
# headless KWin) streams at the requested 60 fps through KWin's screencast.
# Needs the render group: sg render -c "tests/e2e/run.sh kde_fps lan"
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
if [[ ! -w /dev/dri/renderD128 ]] || ! command -v kwin_wayland >/dev/null; then
	blocked A15-kde-fps "needs the render group and KWin (apt install kwin-wayland)"
	return 0
fi
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cat >"$LOGDIR/kde-inner.sh" <<EOS
WAYLAND_DISPLAY=wayland-0 gst-launch-1.0 -q videotestsrc pattern=ball is-live=true \
	! video/x-raw,width=1280,height=720,framerate=60/1 ! waylandsink fullscreen=true >"$LOGDIR/anim.log" 2>&1 &
sleep 1.5
exec env GST_REGISTRY="$GST_REGISTRY" QR_GSO=0 WAYLAND_DISPLAY=wayland-0 "$BIN/qr-host" --listen 0.0.0.0:9450 \
	--config-dir "$LOGDIR/khost" --name khost --pin 424242 --encoder vah264lpenc --source wayland --fps 60 --audio none
EOS
bg host khost "$here/kde_session.sh" "/tmp/qr-kfps-$$" "$LOGDIR" bash "$LOGDIR/kde-inner.sh"
for _ in $(seq 150); do grep -q '^PIN ' "$LOGDIR/khost.log" 2>/dev/null && break; sleep 0.1; done
client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder vah264dec --audio-out none \
	--duration 8 --stats-json "$LOGDIR/kfps.json" --latency-log "$LOGDIR/kfps.jsonl" >"$LOGDIR/kfps-client.txt" 2>&1
python3 - "$LOGDIR/kfps.json" "$LOGDIR/kfps.jsonl" >"$LOGDIR/kfps-summary.txt" <<'PY'
import json, sys
L = [json.loads(l) for l in open(sys.argv[1])][3:]  # past the start
fps = sorted(round(d["fps"]) for d in L)
lat = sorted(json.loads(l).get("total_us", 0) / 1000 for l in open(sys.argv[2]))
lat = [x for x in lat if x]
p50 = lat[len(lat) // 2] if lat else -1
print(f"min_fps={fps[0] if fps else 0} fps={fps} p50_ms={p50:.1f}")
PY
cat "$LOGDIR/kfps-summary.txt"
check A15-kde-fps "an animated KDE desktop streams at ≥50 of 60 fps every second ($(cat "$LOGDIR/kfps-summary.txt"))" \
	python3 -c "import sys; sys.exit(0 if int('$(grep -o 'min_fps=[0-9]*' "$LOGDIR/kfps-summary.txt" | cut -d= -f2)' or 0) >= 50 else 1)"
