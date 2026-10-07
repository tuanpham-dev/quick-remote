# A8: 1080p60 over the simulated gigabit LAN, GPU encode and decode, 2 min:
# capture → decoded p50 ≤ 12 ms, p95 ≤ 20 ms. Needs the render group:
#   sg render -c "tests/e2e/run.sh a8 lan"
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
SECS="${A8_SECS:-120}"
if [[ ! -w /dev/dri/renderD128 ]]; then
	blocked A8 "the GPU render node is not accessible (needs the render group)"
	return 0
fi
start_host host ghost --pin 424242 --encoder vah264lpenc --source test:ball --size 1920x1080 --fps 60 --audio none
client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder vah264dec --audio-out none \
	--duration "$SECS" --latency-log "$LOGDIR/a8.jsonl" --stats-json "$LOGDIR/a8.json" >"$LOGDIR/a8-client.txt" 2>&1
python3 "$(dirname "${BASH_SOURCE[0]}")/frames.py" "$LOGDIR/a8.jsonl" --from 5 | tee "$LOGDIR/a8-summary.json"
r() { python3 -c "import json;print(json.load(open('$LOGDIR/a8-summary.json'))['$1'])"; }
check A8 "1080p60 on the simulated gigabit LAN, VA-API both ends, ${SECS}s: p50 $(r p50_ms) ms, p95 $(r p95_ms) ms (targets 12 / 20)" \
	python3 -c "import sys; sys.exit(0 if $(r p50_ms) <= 12 and $(r p95_ms) <= 20 and $(r frames) > 50 * ($SECS - 6) else 1)"
