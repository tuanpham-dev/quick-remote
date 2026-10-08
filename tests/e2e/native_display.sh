# A15: the window client's display controls against the sway virtual
# display: the desktop follows the window (resized with xdotool), Ctrl+wheel
# zooms the view, and settings kept in display.json (a fixed 1600×900) take
# effect and hold through a window resize. The client runs in a window on
# an Xvfb display.
# Needs the render group: sg render -c "tests/e2e/run.sh native_display lan"
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
if [[ ! -w /dev/dri/renderD128 ]] || ! command -v xdotool >/dev/null; then
	blocked A15-native "needs the render group and xdotool"
	return 0
fi
start_host host nhost --pin 424242 --encoder vah264lpenc --source vdisplay --size 1280x720 --fps 60 --audio none
bg client xvfbn Xvfb :87 -screen 0 1920x1080x24 -nolisten tcp
sleep 1
sizes() { sed 's/\x1b\[[0-9;]*m//g' "$LOGDIR/nhost.log" | grep -o "stream started stream=0 monitor=[^ ]* w=[0-9]* h=[0-9]*" | sed -E 's/.* w=([0-9]+) h=([0-9]+)/\1x\2/' | tr '\n' ' '; }
win() { on client env DISPLAY=:87 xdotool search --sync --name "nhost|QuickRemote" | head -1; }

# Follow the window, then zoom.
bg client ncli env DISPLAY=:87 "$BIN/qr-client" --config-dir "$LOGDIR/ncli" --name ncli --connect "$IP_host:9450" --pin 424242 --audio-out none
sleep 6
w=$(win)
on client env DISPLAY=:87 xdotool windowsize "$w" 1000 600
sleep 2
follow_sizes=$(sizes)
on client env DISPLAY=:87 xdotool windowfocus --sync "$w" mousemove --window "$w" 500 300 keydown ctrl click 4 click 4 click 4 keyup ctrl || true
sleep 1
zoom=$(sed 's/\x1b\[[0-9;]*m//g' "$LOGDIR/ncli.log" | grep -o "zoom=[0-9.]*" | tail -1 | cut -d= -f2 || true)
for p in $(pgrep -x qr-client); do tr '\0' ' ' <"/proc/$p/cmdline" 2>/dev/null | grep -q -- "--name ncli" && kill "$p"; done
sleep 2

# Kept settings: a fixed 1600×900.
printf '{"mode":"fixed","fixed_w":1600,"fixed_h":900,"hz":60,"ratio":1.0,"target":"virtual","view":"fit"}' >"$LOGDIR/ncli/display.json"
before=$(sizes | wc -w)
bg client ncli2 env DISPLAY=:87 "$BIN/qr-client" --config-dir "$LOGDIR/ncli" --name ncli --connect "$IP_host:9450" --audio-out none
sleep 6
w=$(win)
on client env DISPLAY=:87 xdotool windowsize "$w" 900 500
sleep 2
fixed_sizes=$(sizes | cut -d' ' -f$((before + 1))-)
for p in $(pgrep -x qr-client); do tr '\0' ' ' <"/proc/$p/cmdline" 2>/dev/null | grep -q -- "--name ncli" && kill "$p"; done

echo "follow: $follow_sizes | zoom: $zoom | fixed run: $fixed_sizes"
check A15-native-follow "the desktop follows the client window (stream sizes: $follow_sizes)" \
	bash -c "grep -q '1000x600' <<<'$follow_sizes'"
check A15-native-zoom "Ctrl+wheel zooms the view (zoom $zoom)" python3 -c "import sys; sys.exit(0 if ${zoom:-0} > 1.2 else 1)"
check A15-native-kept "settings kept in display.json apply, and a fixed size holds through a window resize (stream sizes: $fixed_sizes)" \
	bash -c "s='$fixed_sizes'; [[ \"\${s% }\" == *1600x900 ]] && [[ \$s != *900x500* ]]"
