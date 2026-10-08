# A15 (Wayland): a GNOME desktop (headless mutter on a private session bus
# as the stand-in) through mutter's remote-desktop API: the stream shows the
# desktop, and input reaches a GTK app there. No permission is ever asked
# (mutter's API has no dialog; the portal's restore token isn't needed).
# Needs the render group: sg render -c "tests/e2e/run.sh a15_wayland lan"
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
if [[ ! -w /dev/dri/renderD128 ]]; then
	blocked A15-wayland "the GPU render node is not accessible (needs the render group)"
	return 0
fi
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
probe="$LOGDIR/probe.jsonl"
cat >"$LOGDIR/gnome-inner.sh" <<EOS
QR_PROBE_IM=1 WAYLAND_DISPLAY=wayland-0 GDK_BACKEND=wayland python3 $here/wl_probe.py $probe >"$LOGDIR/probe.err" 2>&1 &
sleep 1.5
exec env GST_REGISTRY="$GST_REGISTRY" QR_GSO=0 "$BIN/qr-host" --listen 0.0.0.0:9450 --config-dir "$LOGDIR/ghost" --name ghost \
	--pin 424242 --encoder vah264lpenc --source mutter --fps 60 --audio none
EOS
bg host ghost "$here/gnome_session.sh" "/tmp/qr-a15-$$" "$LOGDIR" bash "$LOGDIR/gnome-inner.sh"
for _ in $(seq 150); do grep -q '^PIN ' "$LOGDIR/ghost.log" 2>/dev/null && break; sleep 0.1; done
cat >"$LOGDIR/a15w.script" <<'EOS'
sleep 2500
abs 400 300
sleep 100
button 1 down
button 1 up
sleep 200
tap 16
text héllo 漢字
sleep 300
abs 100 100
sleep 200
rel 1 0 300
sleep 800
EOS
client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder vah264dec --audio-out none \
	--duration 7 --script "$LOGDIR/a15w.script" --latency-log "$LOGDIR/a15w.jsonl" >"$LOGDIR/a15w-client.txt" 2>&1
python3 - "$probe" "$LOGDIR/a15w.jsonl" >"$LOGDIR/a15w-summary.txt" <<'PY'
import json, sys
L = [json.loads(l) for l in open(sys.argv[1])]
clicks = [(e["x"], e["y"]) for e in L if e["ev"] == "button"]
typed = "".join(e["char"] if e["ev"] == "key" else e["text"] for e in L if e["ev"] in ("key", "commit"))
moves = [(e["x"], e["y"]) for e in L if e["ev"] == "motion"]
frames = sum(1 for _ in open(sys.argv[2]))
print(f"frames={frames} click={clicks[:1]} typed={typed!r} last_motion={moves[-1] if moves else None}")
PY
cat "$LOGDIR/a15w-summary.txt"
mons=$(sed 's/\x1b\[[0-9;]*m//g' "$LOGDIR/a15w-client.txt" | grep -o 'monitors=[^ ]*' | head -1 || true)
s() { grep -q "$1" "$LOGDIR/a15w-summary.txt"; }
check A15-wayland "a GNOME desktop streams through mutter's remote-desktop API ($mons, $(grep -o 'frames=[0-9]*' "$LOGDIR/a15w-summary.txt")), no permission asked" \
	bash -c "grep -q 'Meta-0' <<<'$mons' && [[ \$(grep -o 'frames=[0-9]*' '$LOGDIR/a15w-summary.txt' | cut -d= -f2) -ge 3 ]]"
check A15-wayland-input "input reaches a GTK app on it: click at (400, 300), keys \"q\" and text \"héllo 漢字\" (outside the layout: by code point), 300 relative steps from (100, 100)" \
	bash -c "grep -q \"click=\\[(400, 300)\\] typed='qhéllo 漢字' last_motion=(400, 100)\" '$LOGDIR/a15w-summary.txt'"
