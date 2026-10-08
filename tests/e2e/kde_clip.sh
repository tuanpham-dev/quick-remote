# Clipboard with a KDE desktop (headless KWin): wlr-data-control on the
# host side, as on wlroots. Text and a PNG each way, byte-identical; the
# desktop's side is driven by another data-control client (wlclipctl), the
# way Klipper and wl-copy work.
# Needs the render group: sg render -c "tests/e2e/run.sh kde_clip lan"
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
if [[ ! -w /dev/dri/renderD128 ]] || ! command -v kwin_wayland >/dev/null; then
	blocked A24-kde "needs the render group and KWin (apt install kwin-wayland)"
	return 0
fi
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CC="$BIN/qr-clipctl"
cargo build -q --manifest-path "$BIN/../../Cargo.toml" -p qr-host --example wlclipctl
WC="$BIN/examples/wlclipctl"
bg client xvfbc Xvfb :78 -screen 0 1280x720x24 -nolisten tcp
GST_REGISTRY="$GST_REGISTRY" gst-launch-1.0 -q videotestsrc num-buffers=1 pattern=snow ! video/x-raw,width=600,height=600 \
	! pngenc compression-level=0 ! filesink location="$LOGDIR/a.png"
GST_REGISTRY="$GST_REGISTRY" gst-launch-1.0 -q videotestsrc num-buffers=1 pattern=snow ! video/x-raw,width=600,height=596 \
	! pngenc compression-level=0 ! filesink location="$LOGDIR/b.png"
cat >"$LOGDIR/kde-inner.sh" <<EOS
echo \$XDG_RUNTIME_DIR/wayland-0 >"$LOGDIR/wsocket"
exec env GST_REGISTRY="$GST_REGISTRY" QR_GSO=0 WAYLAND_DISPLAY=wayland-0 "$BIN/qr-host" --listen 0.0.0.0:9450 \
	--config-dir "$LOGDIR/clhost" --name clhost --pin 424242 --encoder vah264lpenc --source wayland --audio none
EOS
bg host clhost "$here/kde_session.sh" "/tmp/qr-kclip-$$" "$LOGDIR" bash "$LOGDIR/kde-inner.sh"
for _ in $(seq 150); do grep -q '^PIN ' "$LOGDIR/clhost.log" 2>/dev/null && break; sleep 0.1; done
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
check A24-kde-a "text client → the KDE desktop's clipboard" cmp -s "$LOGDIR/t1.txt" "$LOGDIR/t1.got"
printf 'display→client ✓ %s' "$RANDOM" >"$LOGDIR/t2.txt"
xfer wc_host "$W" cc_client :78 set-text "$(cat "$LOGDIR/t2.txt")" "$LOGDIR/t2.got"
check A24-kde-b "text the KDE desktop's clipboard → client" cmp -s "$LOGDIR/t2.txt" "$LOGDIR/t2.got"
xfer cc_client :78 wc_host "$W" set-png "$LOGDIR/a.png" "$LOGDIR/p1.got"
check A24-kde-c "$(($(stat -c %s "$LOGDIR/a.png") / 1024)) KiB PNG client → KDE, byte-identical" cmp -s "$LOGDIR/a.png" "$LOGDIR/p1.got"
xfer wc_host "$W" cc_client :78 set-png "$LOGDIR/b.png" "$LOGDIR/p2.got"
check A24-kde-d "$(($(stat -c %s "$LOGDIR/b.png") / 1024)) KiB PNG KDE → client, byte-identical" cmp -s "$LOGDIR/b.png" "$LOGDIR/p2.got"
