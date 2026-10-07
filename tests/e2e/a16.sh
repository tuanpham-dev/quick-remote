# A16: a host with 2 virtual monitors: the client lists both, opens a
# second stream on the other one (both stream), and switches them during
# the session. R16: the virtual display follows a resize. Needs the render
# group:
#   sg render -c "tests/e2e/run.sh a16 lan"
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
if [[ ! -w /dev/dri/renderD128 ]]; then
	blocked A16 "the GPU render node is not accessible (needs the render group)"
	return 0
fi
# An animated window on each monitor.
desk='sh -c "weston-simple-egl & until swaymsg -t get_outputs | grep -q HEADLESS-2; do sleep 0.1; done; swaymsg focus output HEADLESS-2; exec weston-simple-egl"'
start_host host vhost --pin 424242 --encoder vah264lpenc --source "vdisplay:$desk" --vd-outputs 2 \
	--size 1600x900 --fps 60 --audio none --input none
cat >"$LOGDIR/a16.script" <<'EOF'
sleep 2000
monitor 1 1
sleep 3000
monitor 1 0
monitor 0 1
sleep 3000
resize 1280 720
sleep 3000
stopstream 1
sleep 2000
EOF
client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder vah264dec --audio-out none \
	--duration 14 --script "$LOGDIR/a16.script" --stats-json "$LOGDIR/a16.json" >"$LOGDIR/a16-client.txt" 2>&1
# One stats line a second; script steps at 2, 5, 8 and 11 s.
python3 - "$LOGDIR/a16.json" >"$LOGDIR/a16-summary.txt" <<'EOF'
import json, sys
L = [json.loads(l) for l in open(sys.argv[1])]
def s1(d): return d["streams"].get("1", {"monitor": -1, "fps": 0})
def ok(lo, hi, f): return all(f(d) for d in L[lo:hi])
# The seconds next to a restart may lose a few frames.
both = ok(3, 5, lambda d: d["fps"] >= 45 and s1(d)["monitor"] == 1 and s1(d)["fps"] >= 45)
swapped = ok(6, 8, lambda d: d["fps"] >= 45 and s1(d)["monitor"] == 0 and s1(d)["fps"] >= 45)
resized = ok(9, 11, lambda d: (d["width"], d["height"]) == (1280, 720) and d["fps"] >= 45)
stopped = ok(12, 14, lambda d: not d["streams"] and d["fps"] >= 45)
print(f"both={both} swapped={swapped} resized={resized} stopped={stopped}")
for i, d in enumerate(L):
    print(i, round(d["fps"]), d["width"], d["height"], {k: (v["monitor"], round(v["fps"])) for k, v in d["streams"].items()})
EOF
cat "$LOGDIR/a16-summary.txt"
mons=$(sed 's/\x1b\[[0-9;]*m//g' "$LOGDIR/a16-client.txt" | grep -o 'monitors=[^ ]* [^ ]*' | head -1 || true)
check A16 "2 virtual monitors listed ($mons); a second stream on the other monitor and a swap, both at ≥45 fps; stopping it" \
	bash -c "grep -q 'HEADLESS-2' <<<'$mons' && grep -q 'both=True swapped=True' '$LOGDIR/a16-summary.txt' && grep -q 'stopped=True' '$LOGDIR/a16-summary.txt'"
check R16 "the virtual display follows a client resize (1600x900 → 1280x720) and keeps streaming" \
	grep -q 'resized=True' "$LOGDIR/a16-summary.txt"
