# A15 (KDE): a Plasma desktop (headless KWin on a private session bus as the
# stand-in) through KWin's screencast and fake input: `--source wayland`
# detects KWin, the stream shows the desktop, and input reaches a GTK app
# there. No permission is asked. KWin's permission checks are on: the host
# lists its protocols in a desktop file of its own on first start
# (A15-kde-auth), and KWin lets it in without a restart.
# Needs the render group: sg render -c "tests/e2e/run.sh a15_kde lan"
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
if [[ ! -w /dev/dri/renderD128 ]]; then
	blocked A15-kde "the GPU render node is not accessible (needs the render group)"
	return 0
fi
if ! command -v kwin_wayland >/dev/null; then
	blocked A15-kde "KWin is not installed (apt install kwin-wayland)"
	return 0
fi
if ! compgen -G "/usr/lib/*/qt6/plugins/kwin/plugins/screencast.so" >/dev/null &&
	[[ ! -f "${XDG_CACHE_HOME:-$HOME/.cache}/quickremote/kwin-plugins/kwin/plugins/screencast.so" ]]; then
	blocked A15-kde "KWin's screencast plugin is missing (kwin-common, or scripts/kwin-test-plugin.sh)"
	return 0
fi
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
probe="$LOGDIR/probe.jsonl"
cat >"$LOGDIR/kde-inner.sh" <<EOS
WAYLAND_DISPLAY=wayland-0 GDK_BACKEND=wayland python3 $here/wl_probe.py $probe >"$LOGDIR/probe.err" 2>&1 &
sleep 1.5
exec env GST_REGISTRY="$GST_REGISTRY" QR_GSO=0 WAYLAND_DISPLAY=wayland-0 "$BIN/qr-host" --listen 0.0.0.0:9450 \
	--config-dir "$LOGDIR/khost" --name khost --pin 424242 --encoder vah264lpenc --source wayland --fps 60 --audio none
EOS
bg host khost "$here/kde_session.sh" "/tmp/qr-a15k-$$" "$LOGDIR" bash "$LOGDIR/kde-inner.sh"
for _ in $(seq 150); do grep -q '^PIN ' "$LOGDIR/khost.log" 2>/dev/null && break; sleep 0.1; done
cat >"$LOGDIR/a15k.script" <<'EOS'
sleep 2500
abs 400 300
sleep 100
button 1 down
button 1 up
sleep 200
tap 16
text Hello, world!
sleep 300
abs 100 100
sleep 200
rel 1 0 300
sleep 800
EOS
client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder vah264dec --audio-out none \
	--duration 7 --script "$LOGDIR/a15k.script" --latency-log "$LOGDIR/a15k.jsonl" >"$LOGDIR/a15k-client.txt" 2>&1
python3 - "$probe" "$LOGDIR/a15k.jsonl" >"$LOGDIR/a15k-summary.txt" <<'PY'
import json, sys
L = [json.loads(l) for l in open(sys.argv[1])]
clicks = [(e["x"], e["y"]) for e in L if e["ev"] == "button"]
typed = "".join(e["char"] for e in L if e["ev"] == "key")
moves = [(e["x"], e["y"]) for e in L if e["ev"] == "motion"]
frames = sum(1 for _ in open(sys.argv[2]))
print(f"frames={frames} click={clicks[:1]} typed={typed!r} last_motion={moves[-1] if moves else None}")
PY
cat "$LOGDIR/a15k-summary.txt"
mons=$(sed 's/\x1b\[[0-9;]*m//g' "$LOGDIR/a15k-client.txt" | grep -o 'monitors=[^ ]*' | head -1 || true)
check A15-kde "a KDE desktop streams through KWin's screencast, detected from --source wayland ($mons, $(grep -o 'frames=[0-9]*' "$LOGDIR/a15k-summary.txt"))" \
	bash -c "grep -q 'KDE desktop' '$LOGDIR/khost.log' && [[ \$(grep -o 'frames=[0-9]*' '$LOGDIR/a15k-summary.txt' | cut -d= -f2) -ge 3 ]]"
check A15-kde-auth "with KWin's permission checks on, the host wrote its desktop file and got the protocols ($(sed 's/\x1b\[[0-9;]*m//g' "$LOGDIR/khost.log" | grep -o 'file=[^ ]*' | head -1))" \
	bash -c "grep -q 'listed our protocols in a desktop file' '$LOGDIR/khost.log' && grep -q 'KWin session' '$LOGDIR/khost.log'"
check A15-kde-input "input reaches a GTK app on it: click at (400, 300), keys \"q\" and text \"Hello, world!\", 300 relative steps from (100, 100)" \
	bash -c "grep -q \"click=\\[(400, 300)\\] typed='qHello, world!' last_motion=(400, 100)\" '$LOGDIR/a15k-summary.txt'"
