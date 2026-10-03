# A1 (discovery), A2 (pairing, reconnect, wrong PIN, rate limit) on the LAN topology.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
start_host host studio --pin 424242 --encoder x264enc --source test:smpte --size 640x360 --fps 30

t0=$(date +%s.%N)
client client cli --list --list-secs 3 >"$LOGDIR/list.txt" 2>&1
check A1 "host listed on the LAN within 3 s" grep -q "studio" "$LOGDIR/list.txt"

common=(--connect "$IP_host:9450" --headless --decoder openh264dec --duration 2 --stats-json "$LOGDIR/a2.json")
client client cli "${common[@]}" >"$LOGDIR/a2-nopin.txt" 2>&1 && r=0 || r=$?
check A2a "unpaired client without a PIN is refused" grep -q "enter the PIN" "$LOGDIR/a2-nopin.txt"
client client cli "${common[@]}" --pin 111111 >"$LOGDIR/a2-wrong.txt" 2>&1 || true
check A2b "wrong PIN is refused" grep -q "doesn't match" "$LOGDIR/a2-wrong.txt"
client client cli "${common[@]}" --pin 424242 >"$LOGDIR/a2-pair.txt" 2>&1 || true
check A2c "right PIN pairs and the session streams" grep -q '"fps":[1-9]' "$LOGDIR/a2.json"
rm -f "$LOGDIR/a2.json"
client client cli "${common[@]}" >"$LOGDIR/a2-again.txt" 2>&1 || true
check A2d "reconnect needs no PIN" grep -q '"fps":[1-9]' "$LOGDIR/a2.json"

# Rate limit: a fresh client guesses wrong 5 times, then the right PIN is ignored.
for i in 1 2 3 4 5; do client client guesser "${common[@]}" --pin 00000$i >/dev/null 2>&1 || true; done
client client guesser "${common[@]}" --pin 424242 >"$LOGDIR/a2-limited.txt" 2>&1 || true
check A2e "5 wrong PINs in a minute: address ignored" grep -q "Too many wrong PINs" "$LOGDIR/a2-limited.txt"
