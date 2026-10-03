# A4: across two NATs a client reaches a paired host by ID with no port
# forwarding — Direct through cone NATs, Relayed through symmetric ones —
# and video flows. A3 (on the relayed path): the relay sees no plaintext.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
MODE="${NAT_MODE:-cone}"
bg inet relay "$BIN/qr-relay" --listen 0.0.0.0:4500 --public-ip "$IP_relay" --config-dir "$LOGDIR/relay"
sleep 1
start_host host nhost --pin 424242 --encoder x264enc --source test:smpte --size 1280x720 --fps 30 --audio none \
	--rendezvous "$IP_relay:4500" --files-dir "$LOGDIR/in"
for _ in $(seq 50); do grep -q '^REGISTERED' "$LOGDIR/nhost.log" && break; sleep 0.1; done
HID=$(sed -n 's/^HOST-ID //p' "$LOGDIR/nhost.log")
check "A4-$MODE-reg" "host registers with the rendezvous server through its NAT" grep -q '^REGISTERED' "$LOGDIR/nhost.log"

canary="QR-PLAINTEXT-CANARY-7731"
{ for _ in $(seq 2000); do echo "$canary"; done; } >"$LOGDIR/canary.txt"
if [[ "$MODE" == sym ]]; then
	on inet python3 "$(dirname "${BASH_SOURCE[0]}")/sniff.py" 14 "$LOGDIR/relay.pcap" >"$LOGDIR/sniffed.txt" &
	sniffer=$!
	extra=(--send-file "$LOGDIR/canary.txt")
else
	extra=()
fi
client client cli --connect "$HID" --rendezvous "$IP_relay:4500" --pin 424242 --headless --decoder openh264dec \
	--audio-out none --duration 10 --stats-json "$LOGDIR/a4.json" "${extra[@]}" >"$LOGDIR/a4-client.txt" 2>&1
path=$(tail -1 "$LOGDIR/a4.json" | sed -E 's/.*"path":"([^"]*)".*/\1/')
fps=$(tail -1 "$LOGDIR/a4.json" | sed -E 's/.*"fps":([0-9.]+).*/\1/')
want=Direct
[[ "$MODE" == sym ]] && want=Relayed
check "A4-$MODE" "$MODE NAT on both sides connects $want (got $path) and streams (${fps} fps)" \
	python3 -c "import sys; sys.exit(0 if '$path' == '$want' and $fps > 20 else 1)"

if [[ "$MODE" == sym ]]; then
	wait "$sniffer"
	check A3a "the file with the canary crossed the relay" cmp -s "$LOGDIR/canary.txt" "$LOGDIR/in/canary.txt"
	check A3b "the relay saw $(cat "$LOGDIR/sniffed.txt") packets and none hold the canary in plaintext" \
		bash -c "! grep -qa '$canary' '$LOGDIR/relay.pcap'"
	# A3's forged-host case (a client pinned to another key aborts) is
	# covered by crates/qr-net/tests/handshake.rs::wrong_host_key_aborts.
fi
