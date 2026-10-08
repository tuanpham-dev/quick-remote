# Display requests on a GNOME desktop (headless mutter): A5 (the client's
# own virtual monitor at the requested size, primary when every monitor is
# virtual, streamed), A7-GNOME (2× gives it scale 2 at the same pixel
# size; mutter allows a scale only while the logical size stays at least
# about 800×480, hence 2560×1440), A4-GNOME (a reconnect within the minute reuses it; a minute after
# the last session it is gone) and A6-GNOME (the real monitor: a listed
# mode is used, an unlisted one explained).
# Needs the render group: sg render -c "tests/e2e/run.sh gnome_display lan"
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
if [[ ! -w /dev/dri/renderD128 ]]; then
	blocked A5 "the GPU render node is not accessible (needs the render group)"
	return 0
fi
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cat >"$LOGDIR/gnome-inner.sh" <<EOS
echo "DBUS_SESSION_BUS_ADDRESS=\$DBUS_SESSION_BUS_ADDRESS" >"$LOGDIR/session.env"
echo "XDG_RUNTIME_DIR=\$XDG_RUNTIME_DIR" >>"$LOGDIR/session.env"
exec env GST_REGISTRY="$GST_REGISTRY" QR_GSO=0 "$BIN/qr-host" --listen 0.0.0.0:9450 --config-dir "$LOGDIR/ghost" --name ghost \
	--pin 424242 --encoder vah264lpenc --source mutter --fps 60 --audio none
EOS
bg host ghost "$here/gnome_session.sh" "/tmp/qr-gdisp-$$" "$LOGDIR" bash "$LOGDIR/gnome-inner.sh"
for _ in $(seq 150); do grep -q '^PIN ' "$LOGDIR/ghost.log" 2>/dev/null && break; sleep 0.1; done
lay() { on host env $(cat "$LOGDIR/session.env") python3 "$here/gnome_layout.py" 2>/dev/null; }
run() {
	local name="$1" secs="$2"
	shift 2
	printf 'sleep 2000\ndisplay %s\nsleep %s\n' "$*" "$((secs * 1000 - 2500))" >"$LOGDIR/$name.script"
	(sleep 5; lay >"$LOGDIR/$name.lay.json") &
	local probe=$!
	client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder vah264dec --audio-out none \
		--duration "$secs" --script "$LOGDIR/$name.script" --stats-json "$LOGDIR/$name.json" >"$LOGDIR/$name-client.txt" 2>&1
	wait "$probe" || true
}
run v100 8 1600 900 60 100 0
run v200 8 2560 1440 60 200 0
created_before=$(grep -c "mutter virtual monitor" "$LOGDIR/ghost.log" || true)
run again 8 2560 1440 60 200 0
created_after=$(grep -c "mutter virtual monitor" "$LOGDIR/ghost.log" || true)
run real1 7 1024 768 60 100 1
run real2 7 1280 720 60 100 1
sleep 65
lay >"$LOGDIR/later.lay.json"

python3 - "$LOGDIR" <<'PY' >"$LOGDIR/gnome_display-summary.txt"
import json, re, sys
d = sys.argv[1]
def lay(n):
    try:
        return json.load(open(f"{d}/{n}.lay.json"))
    except Exception:
        return []
def stats(n):
    rows = [json.loads(l) for l in open(f"{d}/{n}.json") if l.strip()]
    return sorted({f'{r["width"]}x{r["height"]}' for r in rows[4:]})
def notes(n):
    t = re.sub(r"\x1b\[[0-9;]*m", "", open(f"{d}/{n}-client.txt").read())
    return re.findall(r'display info.*?note=(.*)$', t, re.M)[-1:]
def virt(n):
    return [(m["name"], m["mode"], m["scale"], m["primary"]) for m in lay(n) if m["name"] != "Meta-0"]
print(json.dumps(dict(v100=virt("v100"), v100_stream=stats("v100"), v100_notes=notes("v100"),
                      v200=virt("v200"), v200_stream=stats("v200"), v200_notes=notes("v200"), again=virt("again"),
                      real1_notes=notes("real1"), real2_notes=notes("real2"), real2_stream=stats("real2"),
                      later=virt("later"))))
PY
cat "$LOGDIR/gnome_display-summary.txt"
q() { python3 -c "import json;d=json.load(open('$LOGDIR/gnome_display-summary.txt'));print($1)"; }
ok() { python3 -c "import json,sys; d=json.load(open('$LOGDIR/gnome_display-summary.txt')); sys.exit(0 if ($1) else 1)"; }
check A5 "a virtual monitor of 1600×900 is created, primary, and streamed ($(q "d['v100']"), stream $(q "d['v100_stream']"), note $(q "d['v100_notes']"))" \
	ok "d['v100'] and d['v100'][0][1]=='1600x900' and d['v100'][0][3] and d['v100_stream']==['1600x900']"
check A7-GNOME "2× gives the virtual monitor scale 2 at 2560×1440 pixels ($(q "d['v200']"), stream $(q "d['v200_stream']"), note $(q "d['v200_notes']"))" \
	ok "d['v200'] and d['v200'][0][2]==2 and d['v200'][0][1]=='2560x1440' and d['v200_stream']==['2560x1440']"
check A4a-GNOME "a reconnect within the minute reuses the virtual monitor, same size and scale ($(q "d['again']"); record/remove log lines $created_before then $created_after)" \
	ok "d['again']==d['v200'] and $created_before==$created_after"
check A4b-GNOME "a minute after the last session the virtual monitor is gone (left: $(q "d['later']"))" ok "not d['later']"
check A6-GNOME "the real monitor: an unlisted size is explained ($(q "d['real1_notes']")), a listed one is used (stream $(q "d['real2_stream']"), note $(q "d['real2_notes']"))" \
	ok "d['real1_notes'] and 'available' in d['real1_notes'][0] and d['real2_stream']==['1280x720'] and 'available' not in ''.join(d['real2_notes'])"
