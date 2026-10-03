# A24: clipboard text and a 4 MB PNG each way, byte-identical; nothing
# crosses with sync off.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
CC="$BIN/qr-clipctl"
bg host xvfbh Xvfb :77 -screen 0 1280x720x24 -nolisten tcp
bg client xvfbc Xvfb :78 -screen 0 1280x720x24 -nolisten tcp
sleep 1
GST_REGISTRY="$GST_REGISTRY" gst-launch-1.0 -q videotestsrc num-buffers=1 pattern=snow ! video/x-raw,width=1180,height=1180 \
	! pngenc compression-level=0 ! filesink location="$LOGDIR/big.png"
GST_REGISTRY="$GST_REGISTRY" gst-launch-1.0 -q videotestsrc num-buffers=1 pattern=snow ! video/x-raw,width=1180,height=1176 \
	! pngenc compression-level=0 ! filesink location="$LOGDIR/big2.png"
start_host host chost --pin 424242 --encoder x264enc --source x11::77 --audio none --size 1280x720
bg client cli "$BIN/qr-client" --config-dir "$LOGDIR/cli" --name cli --connect "$IP_host:9450" --pin 424242 --headless \
	--decoder openh264dec --audio-out none --duration 40 --clipboard-display :78
sleep 4

# xfer <from-node> <from-display> <to-node> <to-display> <set-cmd> <arg> <out>
xfer() {
	on "$3" "$CC" "$4" get "$7" 8 &
	local getter=$!
	sleep 0.5
	on "$1" "$CC" "$2" "$5" "$6" >/dev/null 2>&1 &
	local setter=$!
	wait "$getter" || true
	kill "$setter" 2>/dev/null || true
}
printf 'client→host ✓ %s' "$RANDOM" >"$LOGDIR/t1.txt"
xfer client :78 host :77 set-text "$(cat "$LOGDIR/t1.txt")" "$LOGDIR/t1.got"
check A24a "text client → host" cmp -s "$LOGDIR/t1.txt" "$LOGDIR/t1.got"
printf 'host→client ✓ %s' "$RANDOM" >"$LOGDIR/t2.txt"
xfer host :77 client :78 set-text "$(cat "$LOGDIR/t2.txt")" "$LOGDIR/t2.got"
check A24b "text host → client" cmp -s "$LOGDIR/t2.txt" "$LOGDIR/t2.got"
size=$(stat -c %s "$LOGDIR/big.png")
xfer client :78 host :77 set-png "$LOGDIR/big.png" "$LOGDIR/p1.got"
check A24c "$((size / 1024)) KiB PNG client → host, byte-identical" cmp -s "$LOGDIR/big.png" "$LOGDIR/p1.got"
# A different image: re-copying the one just received is suppressed as an
# echo for 2 s (clipboard managers re-take ownership with the same data).
xfer host :77 client :78 set-png "$LOGDIR/big2.png" "$LOGDIR/p2.got"
check A24d "$((size / 1024)) KiB PNG host → client, byte-identical" cmp -s "$LOGDIR/big2.png" "$LOGDIR/p2.got"

# Sync off: a client started with --no-clipboard sends nothing.
kill "$PID_cli" 2>/dev/null; sleep 1
bg client cli2 "$BIN/qr-client" --config-dir "$LOGDIR/cli" --name cli --connect "$IP_host:9450" --headless \
	--decoder openh264dec --audio-out none --duration 20 --clipboard-display :78 --no-clipboard
sleep 4
xfer client :78 host :77 set-text "should not cross" "$LOGDIR/t3.got"
check A24e "with sync off nothing crosses" test ! -s "$LOGDIR/t3.got"
