# Clipboard with the virtual display (wlr-data-control on the host side):
# text and a PNG each way, byte-identical. The display's side is driven by
# another data-control client (wlclipctl), the way clipboard managers and
# wl-copy work. (Xwayland can't serve here: in the test's user namespace
# wlroots refuses /tmp/.X11-unix, which looks owned by nobody.)
# Needs the render group: sg render -c "tests/e2e/run.sh vd_clip lan"
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
if [[ ! -w /dev/dri/renderD128 ]]; then
	blocked A24-vd "the GPU render node is not accessible (needs the render group)"
	return 0
fi
CC="$BIN/qr-clipctl"
cargo build -q --manifest-path "$BIN/../../Cargo.toml" -p qr-host --example wlclipctl
WC="$BIN/examples/wlclipctl"
bg client xvfbc Xvfb :78 -screen 0 1280x720x24 -nolisten tcp
GST_REGISTRY="$GST_REGISTRY" gst-launch-1.0 -q videotestsrc num-buffers=1 pattern=snow ! video/x-raw,width=600,height=600 \
	! pngenc compression-level=0 ! filesink location="$LOGDIR/a.png"
GST_REGISTRY="$GST_REGISTRY" gst-launch-1.0 -q videotestsrc num-buffers=1 pattern=snow ! video/x-raw,width=600,height=596 \
	! pngenc compression-level=0 ! filesink location="$LOGDIR/b.png"
# The desktop records the display's Wayland socket.
desk="sh -c \"echo \$XDG_RUNTIME_DIR/\$WAYLAND_DISPLAY >$LOGDIR/wsocket; exec sleep infinity\""
start_host host clhost --pin 424242 --encoder vah264lpenc --source "vdisplay:$desk" --size 1280x720 --audio none --input none
for _ in $(seq 50); do [[ -s $LOGDIR/wsocket ]] && break; sleep 0.1; done
W="$(cat "$LOGDIR/wsocket")"
bg client cli "$BIN/qr-client" --config-dir "$LOGDIR/cli" --name cli --connect "$IP_host:9450" --pin 424242 --headless \
	--decoder vah264dec --audio-out none --duration 40 --clipboard-display :78
sleep 4
# xfer <set-tool> <set-target> <get-tool> <get-target> <set-cmd> <arg> <out>
# (tools: "$CC" on X displays, "$WC" on Wayland sockets; both on their node)
xfer() {
	"${@:3:1}" "$4" get "$7" 8 &
	local getter=$!
	sleep 0.5
	"${@:1:1}" "$2" "$5" "$6" >/dev/null 2>&1 &
	local setter=$!
	wait "$getter" || true
	kill "$setter" 2>/dev/null || true
}
cc_client() { on client "$CC" "$@"; }
wc_host() { on host "$WC" "$@"; }
printf 'client→display ✓ %s' "$RANDOM" >"$LOGDIR/t1.txt"
xfer cc_client :78 wc_host "$W" set-text "$(cat "$LOGDIR/t1.txt")" "$LOGDIR/t1.got"
check A24-vd-a "text client → the virtual display's clipboard" cmp -s "$LOGDIR/t1.txt" "$LOGDIR/t1.got"
printf 'display→client ✓ %s' "$RANDOM" >"$LOGDIR/t2.txt"
xfer wc_host "$W" cc_client :78 set-text "$(cat "$LOGDIR/t2.txt")" "$LOGDIR/t2.got"
check A24-vd-b "text the virtual display's clipboard → client" cmp -s "$LOGDIR/t2.txt" "$LOGDIR/t2.got"
xfer cc_client :78 wc_host "$W" set-png "$LOGDIR/a.png" "$LOGDIR/p1.got"
check A24-vd-c "$(($(stat -c %s "$LOGDIR/a.png") / 1024)) KiB PNG client → display, byte-identical" cmp -s "$LOGDIR/a.png" "$LOGDIR/p1.got"
xfer wc_host "$W" cc_client :78 set-png "$LOGDIR/b.png" "$LOGDIR/p2.got"
check A24-vd-d "$(($(stat -c %s "$LOGDIR/b.png") / 1024)) KiB PNG display → client, byte-identical" cmp -s "$LOGDIR/b.png" "$LOGDIR/p2.got"
