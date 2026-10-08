source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
probe="$LOGDIR/probe.jsonl"
desk="env GDK_BACKEND=wayland python3 $here/wl_probe.py $probe"
start_host host vdhost --pin 424242 --encoder vah264lpenc --source "vdisplay:$desk" --size 1280x720 --fps 60 --audio none
printf 'sleep 2000\ndisplay 1920 1080 60 100\nsleep 2000\n' >"$LOGDIR/s1"
client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder vah264dec --audio-out none --duration 5 --script "$LOGDIR/s1" >"$LOGDIR/c1.txt" 2>&1
printf 'sleep 1500\ndisplay 1920 1080 60 100\nsleep 1500\nabs 400 300\nsleep 100\nbutton 1 down\nbutton 1 up\nsleep 500\nabs 1700 900\nsleep 100\nbutton 1 down\nbutton 1 up\nsleep 800\n' >"$LOGDIR/s2"
client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder vah264dec --audio-out none --duration 6 --script "$LOGDIR/s2" >"$LOGDIR/c2.txt" 2>&1
grep '"button"' "$probe" | python3 -c "import sys,json; print([(json.loads(l)['x'], json.loads(l)['y']) for l in sys.stdin])"
