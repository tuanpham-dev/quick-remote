# A10 (internet profile under 10 Mbit/s, 50 ms RTT, 1% loss; squeeze and
# recovery) and A11 (FEC repairs random loss) on the WAN topology.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
DUR="${A10_SECS:-180}"
RUST_LOG=info,qr_host=debug start_host host wanhost --pin 424242 --encoder x264enc --source test:snow --size 1280x720 --fps 60

st="$LOGDIR/a10.json"
lat="$LOGDIR/a10-frames.jsonl"
(
	sleep $((DUR / 3)); shape host eth0 rate 1mbit delay 25ms loss 0.5% limit 10000
	sleep $((DUR / 6)); shape host eth0 rate 10mbit delay 25ms loss 0.5% limit 10000
) &
shaper=$!
client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder openh264dec \
	--duration "$DUR" --stats-json "$st" --latency-log "$lat" >"$LOGDIR/a10-client.txt" 2>&1
wait "$shaper"

python3 "$(dirname "${BASH_SOURCE[0]}")/frames.py" "$lat" >"$LOGDIR/a10-frames.json"
gap=$(python3 -c "import json;print(json.load(open('$LOGDIR/a10-frames.json'))['max_gap_ms'])")
check A10a "longest gap between frames ≤ 500 ms (got ${gap} ms)" python3 -c "import sys; sys.exit(0 if $gap <= 500 else 1)"

python3 - "$st" "$DUR" >"$LOGDIR/a10-verdict.txt" <<'PY'
import json, sys, statistics
rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
dur = int(sys.argv[2])
third, sixth = dur // 3, dur // 6
def window(a, b): return rows[a:b]
first = window(5, third)
kb = [r["kbps"] for r in first]
med = statistics.median(kb)
stable = sum(1 for k in kb if abs(k - med) <= 0.35 * med) / len(kb)
squeeze = window(third + 5, third + sixth)
after = window(third + sixth + 10, dur)
fps0 = statistics.median(r["fps"] for r in first)
fps_sq = min(r["fps"] for r in squeeze)
kb_sq = statistics.median(r["kbps"] for r in squeeze)
kb_after = statistics.median(r["kbps"] for r in after)
print(json.dumps(dict(median_kbps=med, stable_share=round(stable, 2), fps_before=fps0, fps_min_squeezed=fps_sq,
                      kbps_squeezed=kb_sq, kbps_after=kb_after, fps_after=statistics.median(r["fps"] for r in after),
                      fec_repaired=rows[-1]["fec_repaired"], frames_lost=rows[-1]["frames_lost"], loss_pct=statistics.median(r["loss_pct"] for r in rows))))
PY
cat "$LOGDIR/a10-verdict.txt"
v() { python3 -c "import json;d=json.load(open('$LOGDIR/a10-verdict.txt'));print(d['$1'])"; }
check A10b "bitrate settles below the 10 Mbit/s link and stays within ±35% after 5 s ($(v median_kbps) kbps, $(v stable_share) of seconds in band)" \
	python3 -c "import sys; sys.exit(0 if $(v median_kbps) <= 10000 and $(v stable_share) >= 0.8 else 1)"
check A10c "squeezing the link to 1 Mbit/s drops the frame rate first ($(v fps_before) → $(v fps_min_squeezed) fps) and recovers ($(v kbps_after) kbps, $(v fps_after) fps)" \
	python3 -c "import sys; sys.exit(0 if $(v fps_min_squeezed) <= 0.6 * $(v fps_before) and $(v kbps_after) > 1.3 * $(v kbps_squeezed) and $(v fps_after) >= 0.9 * $(v fps_before) else 1)"
check A11a "FEC repairs shards under ~1% loss ($(v fec_repaired) shards repaired, $(v frames_lost) frames lost)" \
	python3 -c "import sys; sys.exit(0 if $(v fec_repaired) > 0 else 1)"
