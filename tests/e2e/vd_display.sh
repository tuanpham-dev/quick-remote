# Display requests on the host's own virtual display (sway): A1/A2 host
# side (the stream takes the requested size), A7-sway (a 2× request sets
# the output scale, the stream is twice the logical size, and a click in
# stream pixels lands at the matching logical point), and pause/resume
# (no frames while paused, a keyframe soon after resuming).
# Needs the render group: sg render -c "tests/e2e/run.sh vd_display lan"
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
if [[ ! -w /dev/dri/renderD128 ]]; then
	blocked A1-display "the GPU render node is not accessible (needs the render group)"
	return 0
fi
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
probe="$LOGDIR/probe.jsonl"
desk="sh -c \"echo \\\$SWAYSOCK >$LOGDIR/swaysock; exec env GDK_BACKEND=wayland python3 $here/wl_probe.py $probe\""
start_host host vdhost --pin 424242 --encoder vah264lpenc --source "vdisplay:$desk" --size 1280x720 --fps 60 --audio none
cat >"$LOGDIR/vd_display.script" <<'EOS'
sleep 2500
display 1600 900 60 100
sleep 2500
display 1600 900 60 200
sleep 2500
abs 800 450
sleep 100
button 1 down
button 1 up
sleep 500
pause
sleep 2500
resume
sleep 2000
EOS
client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder vah264dec --audio-out none \
	--duration 15 --script "$LOGDIR/vd_display.script" --stats-json "$LOGDIR/vd_display.json" >"$LOGDIR/vd_display-client.txt" 2>&1
sock=$(cat "$LOGDIR/swaysock")
on host env SWAYSOCK="$sock" swaymsg -t get_outputs -r >"$LOGDIR/outputs.json" 2>&1 || true
python3 - "$LOGDIR/vd_display.json" "$LOGDIR/outputs.json" "$probe" "$LOGDIR/vd_display-client.txt" >"$LOGDIR/vd_display-summary.txt" <<'PY'
import json, re, sys
rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
outs = json.load(open(sys.argv[2]))
o = outs[0] if outs else {}
clicks = [(e["x"], e["y"]) for e in map(json.loads, open(sys.argv[3])) if e["ev"] == "button"]
infos = re.findall(r"display info.*?width=(\d+) height=(\d+) scale_pct=(\d+)", re.sub(r"\x1b\[[0-9;]*m", "", open(sys.argv[4]).read()))
sizes = [f'{r["width"]}x{r["height"]}' for r in rows]
# Seconds: request 1 at 2.5 s, 2× at 5 s, click at 7.5 s, pause 8.1–10.6 s, resume.
paused = [r["fps"] for r in rows[9:10]]
after = [r["fps"] for r in rows[12:14]]
print(json.dumps(dict(sizes=sizes, infos=infos, scale=o.get("scale"), mode=o.get("current_mode", {}).get("width"),
                      click=clicks[:1], fps_paused=paused, fps_after=after)))
PY
cat "$LOGDIR/vd_display-summary.txt"
q() { python3 -c "import json;d=json.load(open('$LOGDIR/vd_display-summary.txt'));print($1)"; }
check A1-display "a display request resizes the virtual display (stream sizes $(q "' '.join(sorted(set(d['sizes'])))"), DisplayInfo $(q "d['infos']"))" \
	python3 -c "import sys; sys.exit(0 if '1600x900' in '$(q "' '.join(d['sizes'])")' and ('1600','900','200') in [tuple(x) for x in $(q "d['infos']")] else 1)"
check A7-sway "2× sets the output scale ($(q "d['scale']")) at 1600 px wide, and a click at stream (800, 450) lands at logical $(q "d['click']")" \
	python3 -c "import sys; d=__import__('json').load(open('$LOGDIR/vd_display-summary.txt')); sys.exit(0 if d['scale']==2.0 and d['mode']==1600 and d['click'] and abs(d['click'][0][0]-400)<=2 and abs(d['click'][0][1]-225)<=2 else 1)"
check A11-host "pause stops the video, resume brings it back (fps paused $(q "d['fps_paused']"), after $(q "d['fps_after']"))" \
	python3 -c "import sys; d=__import__('json').load(open('$LOGDIR/vd_display-summary.txt')); sys.exit(0 if max(d['fps_paused'])<1 and min(d['fps_after'])>=1 else 1)"
