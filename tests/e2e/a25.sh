# A25: 2 GB each way during a 1080p60 stream; hashes match, frame rate holds,
# and a cancelled upload leaves nothing behind.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
SIZE_MB="${A25_MB:-2048}"
mkdir -p "$LOGDIR/out" "$LOGDIR/in" "$LOGDIR/recv"
head -c "${SIZE_MB}M" /dev/urandom >"$LOGDIR/up.bin"
head -c "${SIZE_MB}M" /dev/urandom >"$LOGDIR/down.bin"
head -c 512M /dev/urandom >"$LOGDIR/up2.bin"
start_host host fhost --pin 424242 --encoder x264enc --source test:smpte --size 1920x1080 --fps 60 --audio none \
	--files-dir "$LOGDIR/in" --outbox "$LOGDIR/out"
cp "$LOGDIR/down.bin" "$LOGDIR/out/"
client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder openh264dec --audio-out none \
	--duration "${A25_SECS:-75}" --send-file "$LOGDIR/up.bin" --recv-dir "$LOGDIR/recv" \
	--stats-json "$LOGDIR/a25.json" >"$LOGDIR/a25-client.txt" 2>&1
for n in host client; do echo "$n $(on $n nstat -az UdpRcvbufErrors UdpSndbufErrors 2>/dev/null | tail -2 | awk '{print $1"="$2}' | tr '\n' ' ')"; done | tee "$LOGDIR/udp-errors.txt"
up_ok=$(cmp -s "$LOGDIR/up.bin" "$LOGDIR/in/up.bin" && echo yes || echo no)
down_ok=$(cmp -s "$LOGDIR/down.bin" "$LOGDIR/recv/down.bin" && echo yes || echo no)
check A25a "${SIZE_MB} MB upload arrives byte-identical ($up_ok)" test "$up_ok" = yes
check A25b "${SIZE_MB} MB download (host outbox) arrives byte-identical and leaves the outbox ($down_ok)" \
	test "$down_ok" = yes -a ! -e "$LOGDIR/out/down.bin"
minfps=$(python3 -c "
import json
r=[json.loads(l) for l in open('$LOGDIR/a25.json')][2:-2]
print(min(x['fps'] for x in r))")
rmem=$(cat /proc/sys/net/core/rmem_max)
if [[ "$rmem" -lt 8388608 ]]; then
	blocked A25c "frame rate during transfers (min ${minfps}): net.core.rmem_max is $rmem, so bursts overflow UDP buffers ($(cat "$LOGDIR/udp-errors.txt" | tr '\n' ' ')); run scripts/setup-root.sh"
else
	check A25c "frame rate stays ≥ 58 during the transfers (min ${minfps})" python3 -c "import sys; sys.exit(0 if $minfps >= 58 else 1)"
fi

client client cli --connect "$IP_host:9450" --headless --decoder openh264dec --audio-out none --duration 10 \
	--send-file "$LOGDIR/up2.bin" --cancel-after 100000000 >"$LOGDIR/a25-cancel.txt" 2>&1
left=$(ls "$LOGDIR/in" | grep -c up2 || true)
check A25d "a cancelled upload leaves no file or .part behind ($left left)" test "$left" -eq 0
rm -f "$LOGDIR"/*.bin "$LOGDIR"/in/*.bin "$LOGDIR"/recv/*.bin
