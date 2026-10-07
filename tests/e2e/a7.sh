# A7: --stats-json names the codec and encoder: H.264 on VA-API here, and a
# software encoder when VA-API is hidden (an unknown VA driver name).
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
run7() { # run7 <name> <port> [env...]
	local name="$1" port="$2"
	shift 2
	env "$@" GST_REGISTRY="$LOGDIR/$name-registry.bin" "$BIN/qr-host" --probe >"$LOGDIR/$name-probe.txt" 2>&1
	bg host "$name" env "$@" GST_REGISTRY="$LOGDIR/$name-registry.bin" "$BIN/qr-host" --listen 0.0.0.0:$port \
		--config-dir "$LOGDIR/$name" --name "$name" --pin 424242 --source test:ball --size 1280x720 --fps 30 --audio none
	for _ in $(seq 150); do grep -q '^PIN ' "$LOGDIR/$name.log" 2>/dev/null && break; sleep 0.1; done
	client client "cli-$name" --connect "$IP_host:$port" --pin 424242 --headless --audio-out none --duration 4 \
		--stats-json "$LOGDIR/$name.json" >"$LOGDIR/$name-client.txt" 2>&1
}
if [[ -r /dev/dri/renderD128 && -w /dev/dri/renderD128 ]]; then
	run7 a7va 9450
	va="$(jstat "$LOGDIR/a7va.json" codec)/$(jstat "$LOGDIR/a7va.json" encoder)"
	check A7a "with VA-API the stats name H.264 on a VA encoder ($va)" bash -c "[[ '$va' == h264/va* ]]"
else
	blocked A7a "the GPU render node is not accessible (needs the render group)"
fi
run7 a7sw 9451 LIBVA_DRIVER_NAME=quickremote-none
sw="$(jstat "$LOGDIR/a7sw.json" codec)/$(jstat "$LOGDIR/a7sw.json" encoder)"
check A7b "with VA-API hidden it falls back to a software encoder ($sw)" \
	bash -c "[[ '$sw' =~ ^(h264|h265|av1)/(x264enc|x265enc|openh264enc|svtav1enc)$ ]] && ! grep -q '^va' '$LOGDIR/a7sw-probe.txt'"
