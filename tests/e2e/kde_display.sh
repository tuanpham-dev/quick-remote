# Display requests on a KDE desktop (headless KWin). KWin's headless
# backend can't add virtual monitors (only its normal DRM backend can, on a
# real Plasma login), so here: a virtual-monitor request falls back to the
# real monitor with a note (A3-fallback); 2× on the real monitor sets its
# scale (A7-KDE); a listed mode is used and an unlisted one explained (A6).
# A3, A4 (the client's own virtual monitor, kept a minute) need a DRM
# backend and are reported as blocked.
# Needs the render group and KWin: sg render -c "tests/e2e/run.sh kde_display lan"
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
if [[ ! -w /dev/dri/renderD128 ]] || ! command -v kwin_wayland >/dev/null || ! command -v kscreen-doctor >/dev/null; then
	blocked A3 "needs the render group, KWin and kscreen-doctor"
	return 0
fi
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
rt="/tmp/qr-kdisp-$$"
cat >"$LOGDIR/kde-inner.sh" <<EOS
echo "DBUS_SESSION_BUS_ADDRESS=\$DBUS_SESSION_BUS_ADDRESS" >"$LOGDIR/session.env"
exec env GST_REGISTRY="$GST_REGISTRY" QR_GSO=0 WAYLAND_DISPLAY=wayland-0 "$BIN/qr-host" --listen 0.0.0.0:9450 \
	--config-dir "$LOGDIR/khost" --name khost --pin 424242 --encoder vah264lpenc --source wayland --fps 60 --audio none
EOS
bg host khost "$here/kde_session.sh" "$rt" "$LOGDIR" bash "$LOGDIR/kde-inner.sh"
for _ in $(seq 150); do grep -q '^PIN ' "$LOGDIR/khost.log" 2>/dev/null && break; sleep 0.1; done
ks() { on host env $(cat "$LOGDIR/session.env") XDG_RUNTIME_DIR="$rt" WAYLAND_DISPLAY=wayland-0 kscreen-doctor -j 2>/dev/null; }
# run <name> <seconds> <display args>: one session that sends one request,
# with the screen layout captured 2 s after it.
run() {
	local name="$1" secs="$2"
	shift 2
	printf 'sleep 2000\ndisplay %s\nsleep %s\n' "$*" "$((secs * 1000 - 2500))" >"$LOGDIR/$name.script"
	(sleep 4.5; ks >"$LOGDIR/$name.ks.json") &
	local probe=$!
	client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder vah264dec --audio-out none \
		--duration "$secs" --script "$LOGDIR/$name.script" --stats-json "$LOGDIR/$name.json" >"$LOGDIR/$name-client.txt" 2>&1
	wait "$probe" || true
}
run virt 7 1600 900 60 100 0
run s200 7 1280 720 60 200 1
run real1 6 1024 768 60 100 1
run real2 6 1280 720 60 100 1

python3 - "$LOGDIR" <<'PY' >"$LOGDIR/kde_display-summary.txt"
import json, re, sys
d = sys.argv[1]
def outs(n):
    try:
        return {o["name"]: o for o in json.load(open(f"{d}/{n}.ks.json"))["outputs"]}
    except Exception:
        return {}
def stats(n):
    rows = [json.loads(l) for l in open(f"{d}/{n}.json") if l.strip()]
    return sorted({f'{r["width"]}x{r["height"]}' for r in rows[3:]})
def notes(n):
    t = re.sub(r"\x1b\[[0-9;]*m", "", open(f"{d}/{n}-client.txt").read())
    return re.findall(r'display info.*?note=(.*)$', t, re.M)
v0 = outs("s200").get("Virtual-0", {})
print(json.dumps(dict(
    virt_notes=notes("virt")[-1:], virt_stream=stats("virt"), virt_outputs=sorted(outs("virt")),
    s200_scale=v0.get("scale"), s200_stream=stats("s200"),
    real1_notes=notes("real1")[-1:], real2_notes=notes("real2")[-1:], real2_stream=stats("real2"))))
PY
cat "$LOGDIR/kde_display-summary.txt"
q() { python3 -c "import json;d=json.load(open('$LOGDIR/kde_display-summary.txt'));print($1)"; }
blocked A3 "a KDE virtual monitor needs KWin's DRM backend (a real Plasma login); headless KWin can't add outputs"
blocked A4 "keeping a KDE virtual monitor across reconnects needs KWin's DRM backend (a real Plasma login)"
check A3-fallback "a virtual-monitor request on headless KWin uses the real monitor and says why ($(q "d['virt_notes']"), outputs $(q "d['virt_outputs']"), stream $(q "d['virt_stream']"))" \
	python3 -c "import json,sys; d=json.load(open('$LOGDIR/kde_display-summary.txt')); sys.exit(0 if d['virt_notes'] and \"can't add virtual monitors\" in d['virt_notes'][0] and d['virt_outputs']==['Virtual-0'] and d['virt_stream'] else 1)"
check A7-KDE "2× on the real monitor sets its scale ($(q "d['s200_scale']")) and the stream keeps its pixel size ($(q "d['s200_stream']"))" \
	python3 -c "import json,sys; d=json.load(open('$LOGDIR/kde_display-summary.txt')); sys.exit(0 if d['s200_scale']==2 and d['s200_stream']==['1280x720'] else 1)"
check A6 "the real monitor: an unlisted size is explained ($(q "d['real1_notes']")), a listed one is used (stream $(q "d['real2_stream']"), note $(q "d['real2_notes']"))" \
	python3 -c "import json,sys; d=json.load(open('$LOGDIR/kde_display-summary.txt')); sys.exit(0 if d['real1_notes'] and 'available' in d['real1_notes'][0] and d['real2_stream']==['1280x720'] and d['real2_notes']==['\"\"'] else 1)"
