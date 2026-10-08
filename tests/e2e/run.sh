#!/bin/bash
# Runs acceptance scripts in their topologies and prints a summary.
#   tests/e2e/run.sh [phase1|a1_a2|...]
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
mkdir -p "$here/../../out"
export LOGDIR="${LOGDIR:-$(mktemp -d "$here/../../out/e2e.XXXXXX")}"
export RESULTS="$LOGDIR/results.txt"
: >"$RESULTS"
cargo build -q -p qr-host -p qr-client --manifest-path "$here/../../Cargo.toml" || exit 1
# Each script gets its own log directory (client configs inside would
# otherwise remember another script's host at the same address).
base="$LOGDIR"
run() {
	local dir="$base/$2.$1"
	mkdir -p "$dir"
	LOGDIR="$dir" "$here/netns.sh" "$1" "$here/$2.sh" || echo "ERROR script $2 exited $?" | tee -a "$RESULTS"
}
case "${1:-phase1}" in
phase1)
	run lan a1_a2
	[[ -f "$here/a6.sh" ]] && run lan a6
	run lan a7
	run lan a19_a20_a23
	run lan a15_x11
	run lan a15_wayland
	run lan gnome_resize
	run lan gnome_display
	run lan a15_wlroots
	run lan a15_kde
	run lan kde_fps
	run lan kde_clip
	run lan kde_resize
	run lan kde_display
	run wifi wifi
	run lan web_rotate
	[[ -f "$here/a10_a11.sh" ]] && run wan a10_a11
	# Virtual display (ENV-BLOCKED without the render group).
	run lan a16
	run lan vd_input
	run lan vd_audio
	run lan vd_3d
	run lan vd_clip
	run lan vd_display
	run lan native_display
	run lan gnome_clip
	# The web client's display controls, in headless Chrome (no netns).
	node "$here/web_display.mjs" 2>&1 | grep -E '^(PASS|FAIL|ENV-BLOCKED)' | tee -a "$RESULTS" || echo "ERROR script web_display" | tee -a "$RESULTS"
	;;
phase2)
	NAT_MODE=cone run nat-cone a4
	NAT_MODE=sym run nat-sym a4
	;;
*) run "${2:-lan}" "$1" ;;
esac
echo "--- summary ($base)"
sort "$RESULTS" | uniq
! grep -qE '^(FAIL|ERROR)' "$RESULTS"
