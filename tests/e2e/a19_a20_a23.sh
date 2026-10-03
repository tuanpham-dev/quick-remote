# A19 (keys by position, text), A20 (exact relative motion), A23 (input
# latency) against an X display on the host node, on the LAN topology.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
cargo build -q --manifest-path "$BIN/../../Cargo.toml" -p qr-host --example xpointer
XP="$BIN/examples/xpointer"
D=:77
bg host xvfb Xvfb $D -screen 0 1920x1080x24 -nolisten tcp
sleep 1
on host setxkbmap -display $D us
# xev records what each key press types (xterm can't run in a user
# namespace: it fails to chown its pty).
bg host xev env LC_ALL=C.UTF-8 xev -display $D -geometry 1900x1000+0+0 -event keyboard
sleep 1
QR_INPUT_TRACE="$LOGDIR/host-trace.txt" start_host host xhost --pin 424242 --encoder x264enc --source x11:$D --audio none

{
	echo "abs 100 60"
	echo "sleep 500"
	echo "tap 16"   # physical Q position
	echo "tap 28"
	echo "text héllo 漢字"
	echo "tap 28"
	echo "abs 10 500"
	echo "sleep 2500"
	echo "rel 1 0 1000"
	echo "sleep 2000"
	echo "abs 100 60"
	for _ in $(seq 2000); do echo "key 30 down"; echo "key 30 up"; echo "sleep 2"; done
	echo "tap 28"
} >"$LOGDIR/script.txt"

(sleep 3; on host "$XP" $D >"$LOGDIR/p0.txt"; sleep 3.5; on host "$XP" $D >"$LOGDIR/p1.txt") &
probe=$!
client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder openh264dec --audio-out none \
	--duration 25 --script "$LOGDIR/script.txt" --input-trace "$LOGDIR/client-trace.txt" >"$LOGDIR/a19-client.txt" 2>&1
wait "$probe"

# Rebuild the typed text from xev's KeyPress lookups.
python3 - "$LOGDIR/xev.log" >"$LOGDIR/typed.txt" <<'PY'
import re, sys
out, press = [], False
for line in open(sys.argv[1], encoding="utf-8", errors="replace", newline=""):
    if line.startswith("KeyPress"):
        press = True
    elif line.startswith("KeyRelease"):
        press = False
    m = re.search(r'XLookupString gives \d+ bytes: (?:\(([0-9a-f ]+)\))?', line)  # Enter's "\r" splits the line
    if press and m:
        if m.group(1):
            b = bytes(int(x, 16) for x in m.group(1).split())
            out.append(b.decode("utf-8", "replace"))
        press = False
sys.stdout.write("".join(out).replace("\r", "\n"))
PY
l1=$(sed -n 1p "$LOGDIR/typed.txt")
l2=$(sed -n 2p "$LOGDIR/typed.txt")
check A19a "physical Q key types q on a US host (got '$l1')" test "$l1" = "q"
check A19b "text mode types 'héllo 漢字' exactly (got '$l2')" test "$l2" = "héllo 漢字"
read -r x0 y0 <"$LOGDIR/p0.txt"
read -r x1 y1 <"$LOGDIR/p1.txt"
check A20 "1000 relative steps of +1 move the pointer exactly +1000 ($x0,$y0 → $x1,$y1)" test "$((x1 - x0))" -eq 1000 -a "$y1" -eq "$y0"

python3 - "$LOGDIR/client-trace.txt" "$LOGDIR/host-trace.txt" >"$LOGDIR/a23.txt" <<'PY'
import sys
def load(p):
    out = {}
    for l in open(p):
        k, t = l.split()
        out.setdefault(k, []).append(int(t))
    return out
c, h = load(sys.argv[1]), load(sys.argv[2])
d = []
for k in ("key:30:d", "key:30:u"):
    for a, b in zip(c.get(k, []), h.get(k, [])):
        d.append(b - a)
d.sort()
print(len(d), d[len(d) // 2] / 1000, d[int(len(d) * 0.95)] / 1000, d[-1] / 1000)
PY
read -r n p50 p95 mx <"$LOGDIR/a23.txt"
check A23 "input reaches the host in ≤ 2 ms + one-way delay at p95 ($n events: p50 $p50 ms, p95 $p95 ms, max $mx ms)" \
	python3 -c "import sys; sys.exit(0 if $n >= 3000 and $p95 <= 2.1 else 1)"
