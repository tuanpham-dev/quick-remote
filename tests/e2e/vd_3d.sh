# The virtual display with a GPU app and client-chosen modes.
#   A13: vkcube (Vulkan, Wayland) in the display at 60 fps: the stream holds
#        ≥ 58 fps and capture is zero-copy (GPU to encoder, no readback).
#   A14: the client asks for 2560×1440@120, then resizes to 1920×1200@60:
#        the display takes each mode, and the stream follows.
# Needs the render group: sg render -c "tests/e2e/run.sh vd_3d lan"
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
if [[ ! -w /dev/dri/renderD128 ]]; then
	blocked A13 "the GPU render node is not accessible (needs the render group)"
	return 0
fi
start_host host v3host --pin 424242 --encoder vah264lpenc --source "vdisplay:vkcube --wsi wayland" \
	--size 1920x1080 --fps 60 --audio none --input none
cat >"$LOGDIR/vd_3d.script" <<'EOF'
sleep 6000
resize 1920 1200 60
sleep 5000
EOF
client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder vah264dec --audio-out none \
	--size 2560x1440@120 --duration 12 --script "$LOGDIR/vd_3d.script" --stats-json "$LOGDIR/vd_3d.json" \
	--latency-log "$LOGDIR/vd_3d.jsonl" >"$LOGDIR/vd_3d-client.txt" 2>&1
python3 - "$LOGDIR/vd_3d.json" >"$LOGDIR/vd_3d-summary.txt" <<'PY'
import json, sys
L = [json.loads(l) for l in open(sys.argv[1])]
first = L[2:6]
after = L[8:12]
print("first_mode=%dx%d first_fps=%.1f after_mode=%dx%d after_fps=%.1f" % (
    first[-1]["width"], first[-1]["height"], min(d["fps"] for d in first),
    after[-1]["width"], after[-1]["height"], min(d["fps"] for d in after)))
PY
cat "$LOGDIR/vd_3d-summary.txt"
started=$(sed 's/\x1b\[[0-9;]*m//g' "$LOGDIR/v3host.log" | grep -oE 'stream started stream=0 monitor=HEADLESS-1 w=[0-9]+ h=[0-9]+ fps=[0-9]+' | sed -E 's/.* w=([0-9]+) h=([0-9]+) fps=([0-9]+)/\1x\2@\3/' | tr '\n' ' ')
zc=$(grep -c "zero-copy capture" "$LOGDIR/v3host.log")
v() { sed -E "s/.*$1=([0-9x.]+).*/\1/" "$LOGDIR/vd_3d-summary.txt"; }
check A13 "vkcube in the virtual display streams at ≥ 58 fps ($(v first_fps) min at 2560×1440, $(v after_fps) after the resize), zero-copy capture ($zc of 2 starts)" \
	python3 -c "import sys; sys.exit(0 if $(v after_fps) >= 58 and $zc >= 2 else 1)"
check A14 "the display takes the client's modes: started ${started}(asked 2560x1440@120, then 1920x1200@60)" \
	bash -c "[[ '$started' == '2560x1440@120 1920x1200@60 ' && '$(v first_mode)' == 2560x1440 && '$(v after_mode)' == 1920x1200 ]]"
