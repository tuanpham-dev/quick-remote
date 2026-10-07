# Input into the virtual display (Wayland virtual pointer and keyboard),
# checked in a GTK window on the second monitor through a second stream:
# A19 (key position and text "héllo 漢字"), A20 (1000 relative +1 steps sum
# to +1000), and absolute clicks landing on the right monitor. Needs the
# render group:
#   sg render -c "tests/e2e/run.sh vd_input lan"
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
if [[ ! -w /dev/dri/renderD128 ]]; then
	blocked A19-vd "the GPU render node is not accessible (needs the render group)"
	return 0
fi
probe="$LOGDIR/probe.jsonl"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# sway puts a window from the config's exec on the workspace it was launched
# from, so the probe is launched again with swaymsg once HEADLESS-2 has focus.
desk="sh -c \"until swaymsg -t get_outputs | grep -q HEADLESS-2; do sleep 0.1; done; swaymsg focus output HEADLESS-2; swaymsg exec 'env GDK_BACKEND=wayland python3 $here/wl_probe.py $probe'\""
start_host host vihost --pin 424242 --encoder vah264lpenc --source "vdisplay:$desk" --vd-outputs 2 \
	--size 1280x720 --fps 60 --audio none --input auto
cat >"$LOGDIR/vd_input.script" <<'EOF'
sleep 2500
monitor 1 1
sleep 1500
abs 200 150 1
sleep 100
button 1 down
button 1 up
sleep 200
tap 16
text héllo 漢字
sleep 300
abs 100 100 1
sleep 200
rel 1 0 1000
sleep 600
EOF
client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder vah264dec --audio-out none \
	--duration 9 --script "$LOGDIR/vd_input.script" >"$LOGDIR/vd_input-client.txt" 2>&1
python3 - "$probe" >"$LOGDIR/vd_input-summary.txt" <<'EOF'
import json, sys
L = [json.loads(l) for l in open(sys.argv[1])]
clicks = [(e["x"], e["y"]) for e in L if e["ev"] == "button"]
keys = [e for e in L if e["ev"] == "key"]
i = next((n for n, k in enumerate(keys) if k["keyval"] == "q"), None)
typed = "".join(k["char"] for k in keys[i + 1:]) if i is not None else ""
moves = [(e["x"], e["y"]) for e in L if e["ev"] == "motion"]
print(f"click={clicks[:1]} q={i is not None} typed={typed!r} last_motion={moves[-1] if moves else None}")
EOF
cat "$LOGDIR/vd_input-summary.txt"
s() { grep -q "$1" "$LOGDIR/vd_input-summary.txt"; }
check A19-vd "the Q key and text \"héllo 漢字\" arrive in a GTK app on the virtual display ($(cut -d' ' -f3- "$LOGDIR/vd_input-summary.txt" | cut -d' ' -f1-3))" \
	bash -c "grep -q \"q=True typed='héllo 漢字'\" '$LOGDIR/vd_input-summary.txt'"
check A20-vd "1000 relative +1 steps from (100, 100) end at (1100, 100) on the second monitor" s 'last_motion=(1100, 100)'
check A16-input "a click at (200, 150) in the second stream lands there on the second monitor" s 'click=\[(200, 150)\]'
