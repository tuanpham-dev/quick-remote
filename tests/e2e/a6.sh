# A6: take the link down for 3 s mid-session while a key is held. The host
# lets go of the key while the client is silent, and the session resumes on
# its own (no PIN) within 5 s of the link coming back.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
D=:78
bg host xvfb Xvfb $D -screen 0 1280x720x24 -nolisten tcp
sleep 1
on host setxkbmap -display $D us
bg host xev env LC_ALL=C.UTF-8 xev -display $D -geometry 1270x700+0+0 -event keyboard
sleep 1
start_host host a6host --pin 424242 --encoder x264enc --source x11:$D --size 1280x720 --fps 30 --audio none
cat >"$LOGDIR/a6.script" <<'EOF'
abs 100 60
sleep 2000
key 30 down
sleep 9000
key 30 up
EOF
client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder openh264dec --audio-out none \
	--duration 16 --script "$LOGDIR/a6.script" --stats-json "$LOGDIR/a6.json" >"$LOGDIR/a6-client.txt" 2>&1 &
cpid=$!
t0=$(date +%s.%N)
sleep 5
on host ip link set eth0 down
sleep 3
on host ip link set eth0 up
t_up=$(python3 -c "print($(date +%s.%N) - $t0)")
wait "$cpid"
crc=$?
python3 - "$LOGDIR/xev.log" "$LOGDIR/a6.json" "$t_up" >"$LOGDIR/a6-summary.txt" <<'PY'
import json, re, sys
xev = open(sys.argv[1], errors="replace").read()
events = re.findall(r"(KeyPress|KeyRelease) event.*?time (\d+).*?keycode \d+ \(keysym 0x61, a\)", xev, re.S)
# Auto-repeat shows as a release and a press with the same time: skip
# those pairs; the real release is the first release not followed by one.
ev = [(k, int(t)) for k, t in events]
press = [t for i, (k, t) in enumerate(ev) if k == "KeyPress" and not (i > 0 and ev[i - 1] == ("KeyRelease", t))]
release = [t for i, (k, t) in enumerate(ev) if k == "KeyRelease" and not (i + 1 < len(ev) and ev[i + 1] == ("KeyPress", t))]
held_ms = (release[0] - press[0]) if press and release else -1
# One stats line a second from the session start; the link came back at
# t_up seconds after the client started (connecting takes about a second).
rows = [json.loads(l) for l in open(sys.argv[2])]
t_up = float(sys.argv[3])
back = next((i for i, r in enumerate(rows) if i >= t_up - 1 and r["fps"] > 0), None)
resume_s = (back + 1 - t_up) if back is not None else -1
print(f"presses={len(press)} held_ms={held_ms} resume_s={resume_s:.1f} lines={len(rows)}")
PY
cat "$LOGDIR/a6-summary.txt"
v() { sed -E "s/.*$1=([-0-9.]+).*/\1/" "$LOGDIR/a6-summary.txt"; }
check A6a "the held key is released while the link is down (held $(v held_ms) ms; the client lets go at 9000)" \
	python3 -c "import sys; sys.exit(0 if 0 < $(v held_ms) < 6000 and $(v presses) == 1 else 1)"
check A6b "the session resumes on its own within 5 s of the link coming back ($(v resume_s) s), no PIN, client exit $crc" \
	python3 -c "import sys; sys.exit(0 if 0 <= $(v resume_s) <= 5 and $crc == 0 else 1)"
