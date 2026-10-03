# A12: sharp-text mode makes a still page of 9-pixel text crisp (SSIM ≥ 0.98
# against the source) on a link where it would otherwise stay soft.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
cargo build -q --manifest-path "$BIN/../../Cargo.toml" -p qr-media --example testref
"$BIN/examples/testref" text 1280 720 "$LOGDIR/ref.i420"
start_host host shost --pin 424242 --encoder x264enc --source test:text --size 1280x720 --fps 60 --audio none \
	--profile internet --bitrate 1200 --max-kbps 1200
run() { # run <name> [extra client args]
	local n="$1"
	shift
	client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder openh264dec --audio-out none \
		--duration 6 --dump-frame "$LOGDIR/$n.yuv" "$@" >"$LOGDIR/$n.txt" 2>&1
	python3 "$(dirname "${BASH_SOURCE[0]}")/ssim.py" "$LOGDIR/$n.yuv" "$LOGDIR/ref.i420" 1280 720
}
off=$(run soft)
on=$(run sharp --sharp)
echo "SSIM without sharp-text: $off, with: $on"
check A12 "still 9-px text with sharp-text on reaches SSIM ≥ 0.98 (got $on; $off without)" \
	python3 -c "import sys; sys.exit(0 if $on >= 0.98 else 1)"
# Note: a still page also converges without the mode (x264 refines it over
# successive frames); the refresh matters right after motion stops.
