# Helpers for acceptance scripts (sourced inside netns.sh).
BIN="${BIN:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/target/debug}"
export GST_REGISTRY="${GST_REGISTRY:-$LOGDIR/gst-registry.bin}"
export SVT_LOG=1
# netem drops a whole GSO batch at once; send datagrams one by one.
export QR_GSO=0
RESULTS="${RESULTS:-$LOGDIR/results.txt}"

pass() { echo "PASS $1 — $2" | tee -a "$RESULTS"; }
fail() { echo "FAIL $1 — $2" | tee -a "$RESULTS"; }
blocked() { echo "ENV-BLOCKED $1 — $2" | tee -a "$RESULTS"; }
check() { # check <id> <description> <command...>
	local id="$1" what="$2"
	shift 2
	if "$@"; then pass "$id" "$what"; else fail "$id" "$what"; fi
}

# start_host <node> <name> [extra qr-host args...]
start_host() {
	local node="$1" name="$2"
	shift 2
	bg "$node" "$name" "$BIN/qr-host" --listen 0.0.0.0:9450 --config-dir "$LOGDIR/$name" --name "$name" "$@"
	for _ in $(seq 150); do
		grep -q '^PIN ' "$LOGDIR/$name.log" 2>/dev/null && return 0
		sleep 0.1
	done
	echo "host $name did not start:" >&2
	tail -20 "$LOGDIR/$name.log" >&2
	return 1
}

# client <node> <config-name> [qr-client args...]
client() {
	local node="$1" cfg="$2"
	shift 2
	on "$node" "$BIN/qr-client" --config-dir "$LOGDIR/$cfg" --name "$cfg" "$@"
}

# Last value of a JSON field across a stats file.
stat() { tail -1 "$1" | sed -E "s/.*\"$2\":\"?([^,\"}]*).*/\1/"; }
