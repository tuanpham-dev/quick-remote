# Clipboard with a GNOME desktop (headless mutter): the host uses its
# remote-desktop session's selection API; the desktop's side is another
# such session (gnome_clip.py), like any app copying and pasting. Text each
# way. Needs the render group: sg render -c "tests/e2e/run.sh gnome_clip lan"
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
if [[ ! -w /dev/dri/renderD128 ]]; then
	blocked A24-gnome "the GPU render node is not accessible (needs the render group)"
	return 0
fi
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CC="$BIN/qr-clipctl"
bg client xvfbc Xvfb :78 -screen 0 1280x720x24 -nolisten tcp
cat >"$LOGDIR/gnome-inner.sh" <<EOS
echo "DBUS_SESSION_BUS_ADDRESS=\$DBUS_SESSION_BUS_ADDRESS" >"$LOGDIR/session.env"
echo "XDG_RUNTIME_DIR=\$XDG_RUNTIME_DIR" >>"$LOGDIR/session.env"
exec env RUST_LOG=info,qr_host::mutter=debug GST_REGISTRY="$GST_REGISTRY" QR_GSO=0 "$BIN/qr-host" --listen 0.0.0.0:9450 --config-dir "$LOGDIR/ghost" --name ghost \
	--pin 424242 --encoder vah264lpenc --source mutter --fps 30 --audio none
EOS
bg host ghost "$here/gnome_session.sh" "/tmp/qr-gc-$$" "$LOGDIR" bash "$LOGDIR/gnome-inner.sh"
for _ in $(seq 150); do grep -q '^PIN ' "$LOGDIR/ghost.log" 2>/dev/null && break; sleep 0.1; done
bg client cli "$BIN/qr-client" --config-dir "$LOGDIR/cli" --name cli --connect "$IP_host:9450" --pin 424242 --headless \
	--decoder vah264dec --audio-out none --duration 30 --clipboard-display :78
sleep 4
gc() { on host env $(cat "$LOGDIR/session.env") python3 "$here/gnome_clip.py" "$@"; }
printf 'client→gnome ✓ %s' "$RANDOM" >"$LOGDIR/t1.txt"
gc get "$LOGDIR/t1.got" 8 &
g=$!
sleep 0.5
on client "$CC" :78 set-text "$(cat "$LOGDIR/t1.txt")" >/dev/null 2>&1 &
s=$!
wait "$g" || true
kill "$s" 2>/dev/null || true
check A24-gnome-a "text client → the GNOME desktop's clipboard" cmp -s "$LOGDIR/t1.txt" "$LOGDIR/t1.got"
printf 'gnome→client ✓ %s' "$RANDOM" >"$LOGDIR/t2.txt"
on client "$CC" :78 get "$LOGDIR/t2.got" 8 &
g=$!
sleep 0.5
gc set-text "$(cat "$LOGDIR/t2.txt")" >"$LOGDIR/gc-set.log" 2>&1 &
s=$!
wait "$g" || true
kill "$s" 2>/dev/null || true
check A24-gnome-b "text the GNOME desktop's clipboard → client" cmp -s "$LOGDIR/t2.txt" "$LOGDIR/t2.got"
