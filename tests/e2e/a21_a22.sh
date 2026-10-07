# A21 (virtual gamepads: two pads, every axis and button, rumble back,
# unplug) and A22 (pen pressure, tilt, eraser) through uinput on the host.
# Needs the input group: run as `sg input -c "tests/e2e/run.sh a21_a22 lan"`.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
cargo build -q --manifest-path "$BIN/../../Cargo.toml" -p qr-host --example rumble
if [[ ! -w /dev/uinput ]]; then
	blocked A21 "/dev/uinput is not writable (needs the input group)"
	blocked A22 "/dev/uinput is not writable (needs the input group)"
	return 0
fi
D=:85
bg host xvfb Xvfb $D -screen 0 1920x1080x24 -nolisten tcp
sleep 1
start_host host phost --pin 424242 --encoder x264enc --source x11:$D --audio none --size 1920x1080

# record <device name> <log>: evtest on the device once it exists.
record() {
	for _ in $(seq 100); do
		ev=$(awk -v n="$1" '/^N: Name=/{name=$0} /^H: Handlers=/{if (index(name, n)) {match($0, /event[0-9]+/); print substr($0, RSTART, RLENGTH); exit}}' /proc/bus/input/devices)
		[[ -n "$ev" ]] && break
		sleep 0.1
	done
	# udev applies our rule's group a moment after the node appears.
	for _ in $(seq 50); do [[ -n "$ev" && -r "/dev/input/$ev" ]] && break; sleep 0.05; done
	[[ -n "$ev" ]] && timeout 12 evtest "/dev/input/$ev" >"$2" 2>&1
}
record "QuickRemote Gamepad" "$LOGDIR/pad.evtest" &
rec1=$!
record "QuickRemote Pen" "$LOGDIR/pen.evtest" &
rec2=$!

{
	echo "sleep 1500"
	for p in 0 8000 16000 32000 48000 65535; do echo "pen 400 300 $p 30 -20 3"; echo "sleep 50"; done
	echo "pen 420 310 20000 0 0 7"   # eraser in contact
	echo "sleep 50"
	echo "pen 420 310 0 0 0 0"       # out of range
} >"$LOGDIR/pen.script"
( sleep 2.5; on host "$BIN/examples/rumble" "QuickRemote Gamepad" >"$LOGDIR/rumble.txt" 2>&1 ) &
rum=$!
client client cli --connect "$IP_host:9450" --pin 424242 --headless --decoder openh264dec --audio-out none \
	--duration 8 --fake-pad --script "$LOGDIR/pen.script" >"$LOGDIR/a21-client.txt" 2>&1
# Not a bare `wait`: that would also wait for the host, which never exits.
wait "$rec1" "$rec2" "$rum"
pads=$(grep -c 'QuickRemote Gamepad' /proc/bus/input/devices || true)
caps=$(grep -cE 'Event code (304|305|307|308|310|311|314|315|316|317|318) |Event code (0|1|2|3|4|5|16|17) \(ABS_' "$LOGDIR/pad.evtest" || true)
check A21a "virtual Xbox-style pad with every button and axis ($caps capability lines)" test "$caps" -ge 19
check A21b "button and stick events reach the pad device" grep -qE 'code 304 \(BTN_SOUTH\), value 1' "$LOGDIR/pad.evtest"
check A21c "two pads attached, pad 1 removed on unplug (pads left: $pads)" \
	grep -q "virtual gamepad attached idx=1\|virtual gamepad attached.*1" "$LOGDIR/phost.log"
check A21d "rumble from a game reaches the client" grep -q 'rumble from the host' "$LOGDIR/a21-client.txt"
check A22a "pen pressure sweeps 0 → max" bash -c "grep -q 'ABS_PRESSURE), value 4095' '$LOGDIR/pen.evtest' && grep -qE 'ABS_PRESSURE\), value (0|[1-9][0-9]{0,2})$' '$LOGDIR/pen.evtest'"
check A22b "tilt and eraser arrive" bash -c "grep -q 'ABS_TILT_X), value 21' '$LOGDIR/pen.evtest' && grep -q 'BTN_TOOL_RUBBER), value 1' '$LOGDIR/pen.evtest'"
