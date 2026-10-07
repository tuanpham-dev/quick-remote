# Web client certificate rotation: the WebTransport certificate (pinned by
# hash, at most 14 days) is replaced while the host runs, and a browser
# opening the old link (stale #c=) still connects: the page asks the host
# for the current hash (/wt-hash). The rotation period is shortened to 6 s.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if ! command -v google-chrome >/dev/null; then
	blocked WEB-ROTATE "google-chrome is not installed"
	return 0
fi
QR_WEB_ROTATE_SECS=6 start_host host wrhost --pin 424242 --source test:ball --size 640x360 --fps 30 --audio none \
	--web-listen 127.0.0.1:9482
for _ in $(seq 100); do grep -q '^WEB-URL ' "$LOGDIR/wrhost.log" && break; sleep 0.1; done
url=$(grep -o 'WEB-URL [^ ]*' "$LOGDIR/wrhost.log" | head -1 | cut -d' ' -f2)
h1=$(on host curl -s http://127.0.0.1:9482/wt-hash)
on host env CHROME_ARGS=--no-sandbox CDP_PORT=9333 node "$here/web-smoke.mjs" "$url&pin=424242" >"$LOGDIR/web1.json" 2>&1 || true
sleep 8
h2=$(on host curl -s http://127.0.0.1:9482/wt-hash)
on host env CHROME_ARGS=--no-sandbox CDP_PORT=9334 node "$here/web-smoke.mjs" "$url&pin=424242" >"$LOGDIR/web2.json" 2>&1 || true
dec() { python3 -c "import json,sys; print(json.loads(open(sys.argv[1]).readline()).get('decoded', 0))" "$1" 2>/dev/null || echo 0; }
d1=$(dec "$LOGDIR/web1.json")
d2=$(dec "$LOGDIR/web2.json")
check WEB-ROTATE "the WebTransport certificate rotated ($h1 → $h2) and the old link still connects (decoded $d1, then $d2 frames)" \
	bash -c "[[ '$h1' != '$h2' && $d1 -gt 30 && $d2 -gt 30 ]]"
