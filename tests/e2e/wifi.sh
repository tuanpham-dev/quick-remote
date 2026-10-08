# A17 (a busy 1080p picture over a jittery, lossy Wi-Fi-like link keeps its
# frame rate) and A19 (forced keyframes stay throttled) on the wifi topology:
# 30 Mbit/s, ±15 ms jitter, 0.5% loss, and 2 s of 1% loss every 10 s.
#   tests/e2e/run.sh wifi wifi
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
DUR="${WIFI_SECS:-60}"
RUST_LOG=info,qr_host=debug start_host host wifihost --pin 424242 --encoder x264enc --source test:snow --size 1920x1080 --fps 60
(
	for _ in $(seq $((DUR / 10))); do
		sleep 8
		shape client eth0 rate 30mbit delay 15ms 15ms distribution normal loss 1% limit 1000
		sleep 2
		shape client eth0 rate 30mbit delay 15ms 15ms distribution normal loss 0.5% limit 1000
	done
) &
bursts=$!
client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder avdec_h264 \
	--duration "$DUR" --stats-json "$LOGDIR/wifi.json" >"$LOGDIR/wifi-client.txt" 2>&1
kill "$bursts" 2>/dev/null || true
wait "$bursts" 2>/dev/null || true

python3 - "$LOGDIR/wifi.json" "$LOGDIR/wifihost.log" >"$LOGDIR/wifi-verdict.txt" <<'PY'
import json, re, statistics, sys
from collections import Counter
rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()][5:]  # past the start
fps = [r["fps"] for r in rows]
run, longest = 0, 0
for f in fps:
    run = run + 1 if f < 30 else 0
    longest = max(longest, run)
kf = Counter()
for line in open(sys.argv[2], errors="replace"):
    if "forced keyframe" in line:
        m = re.match(r"(?:\x1b\[[0-9;]*m)*(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)", line)
        if m:
            kf[m.group(1)] += 1
print(json.dumps(dict(median_fps=statistics.median(fps), longest_under_30_s=longest, min_fps=min(fps),
                      median_kbps=statistics.median(r["kbps"] for r in rows), max_keyframes_per_s=max(kf.values(), default=0),
                      keyframes=sum(kf.values()))))
PY
cat "$LOGDIR/wifi-verdict.txt"
v() { python3 -c "import json;print(json.load(open('$LOGDIR/wifi-verdict.txt'))['$1'])"; }
check A17 "busy 1080p over jittery, lossy Wi-Fi keeps its frame rate (median $(v median_fps) fps, longest run under 30 fps $(v longest_under_30_s) s, $(v median_kbps) kbps)" \
	python3 -c "import sys; sys.exit(0 if $(v median_fps) >= 50 and $(v longest_under_30_s) <= 3 else 1)"
check A19 "forced keyframes stay throttled (at most $(v max_keyframes_per_s) in a second, $(v keyframes) in all)" \
	python3 -c "import sys; sys.exit(0 if $(v max_keyframes_per_s) <= 4 else 1)"
