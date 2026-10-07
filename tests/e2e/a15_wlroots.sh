# A15 (wlroots): an existing wlroots desktop (a headless sway started by the
# test, as the stand-in for the user's sway/Hyprland session): the host
# attaches with --source wayland:<socket>, streams it through screencopy and
# types into a GTK app there through the virtual input protocols.
# Needs the render group: sg render -c "tests/e2e/run.sh a15_wlroots lan"
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
if [[ ! -w /dev/dri/renderD128 ]]; then
	blocked A15-wlroots "the GPU render node is not accessible (needs the render group)"
	return 0
fi
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
rt="/tmp/qr-wl-$$"
rm -rf "$rt"
mkdir -m 700 -p "$rt"
probe="$LOGDIR/probe.jsonl"
cat >"$LOGDIR/sway.conf" <<EOS
output HEADLESS-1 mode 1280x720@60Hz position 0 0
exec env GDK_BACKEND=wayland python3 $here/wl_probe.py $probe
EOS
bg host sway env -u WAYLAND_DISPLAY -u DISPLAY XDG_RUNTIME_DIR="$rt" WLR_BACKENDS=headless WLR_RENDERER=gles2 \
	LIBSEAT_BACKEND=noop WLR_HEADLESS_OUTPUTS=1 sway -c "$LOGDIR/sway.conf"
for _ in $(seq 100); do [[ -S $rt/wayland-1 ]] && break; sleep 0.1; done
sleep 2
start_host host wlhost --pin 424242 --encoder vah264lpenc --source "wayland:$rt/wayland-1" --fps 60 --audio none
cat >"$LOGDIR/a15l.script" <<'EOS'
sleep 2000
abs 400 300
sleep 100
button 1 down
button 1 up
sleep 200
tap 16
text héllo
sleep 300
EOS
client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder vah264dec --audio-out none \
	--duration 5 --script "$LOGDIR/a15l.script" --latency-log "$LOGDIR/a15l.jsonl" >"$LOGDIR/a15l-client.txt" 2>&1
python3 - "$probe" "$LOGDIR/a15l.jsonl" >"$LOGDIR/a15l-summary.txt" <<'PY'
import json, sys
L = [json.loads(l) for l in open(sys.argv[1])]
clicks = [(e["x"], e["y"]) for e in L if e["ev"] == "button"]
typed = "".join(e["char"] for e in L if e["ev"] == "key")
frames = sum(1 for _ in open(sys.argv[2]))
print(f"frames={frames} click={clicks[:1]} typed={typed!r}")
PY
cat "$LOGDIR/a15l-summary.txt"
rm -rf "$rt"
check A15-wlroots "an existing wlroots desktop streams through screencopy ($(grep -o 'frames=[0-9]*' "$LOGDIR/a15l-summary.txt")) and takes input: $(cut -d' ' -f2- "$LOGDIR/a15l-summary.txt")" \
	bash -c "grep -q \"click=\\[(400, 300)\\] typed='qhéllo'\" '$LOGDIR/a15l-summary.txt' && [[ \$(grep -o 'frames=[0-9]*' '$LOGDIR/a15l-summary.txt' | cut -d= -f2) -ge 3 ]]"
