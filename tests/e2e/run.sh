#!/bin/bash
# Runs acceptance scripts in their topologies and prints a summary.
#   tests/e2e/run.sh [phase1|a1_a2|...]
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
export LOGDIR="${LOGDIR:-$(mktemp -d /tmp/qr-e2e.XXXXXX)}"
export RESULTS="$LOGDIR/results.txt"
: >"$RESULTS"
cargo build -q -p qr-host -p qr-client --manifest-path "$here/../../Cargo.toml" || exit 1
run() { "$here/netns.sh" "$1" "$here/$2.sh" || echo "ERROR script $2 exited $?" | tee -a "$RESULTS"; }
case "${1:-phase1}" in
phase1)
	run lan a1_a2
	[[ -f "$here/a6.sh" ]] && run lan a6
	[[ -f "$here/a10_a11.sh" ]] && run wan a10_a11
	;;
*) run "${2:-lan}" "$1" ;;
esac
echo "--- summary ($LOGDIR)"
sort "$RESULTS" | uniq
! grep -qE '^(FAIL|ERROR)' "$RESULTS"
