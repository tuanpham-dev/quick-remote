#!/bin/bash
# A28 on the Android emulator (API 34): the app connects to a host on this
# machine (10.0.2.2 from inside the emulator), decodes, and its touch,
# keyboard and gamepad input reach the host. Standalone (no netns: the
# emulator can't run inside one):
#   tests/e2e/android.sh            (uses sg kvm / sg input for the groups)
# Results go to out/android.XXXX/results.txt.
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$here/../.."
mkdir -p "$root/out"
LOGDIR="$(mktemp -d "$root/out/android.XXXXXX")"
RESULTS="$LOGDIR/results.txt"
: >"$RESULTS"
pass() { echo "PASS $1 — $2" | tee -a "$RESULTS"; }
fail() { echo "FAIL $1 — $2" | tee -a "$RESULTS"; }
blocked() { echo "ENV-BLOCKED $1 — $2" | tee -a "$RESULTS"; }
check() { local id="$1" what="$2"; shift 2; if "$@"; then pass "$id" "$what"; else fail "$id" "$what"; fi; }
# Runs a command with a group this session may lack (the account has it).
# `exec`: the PID of a backgrounded `with` is the command's own.
with() { local g="$1"; shift; if id -nG | tr ' ' '\n' | grep -qx "$g"; then "$@"; else sg "$g" -c "exec $(printf '%q ' "$@")"; fi; }

export ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/sdk}"
export JAVA_HOME="${JAVA_HOME:-$(ls -d "$HOME"/opt/jdk* 2>/dev/null | head -1)}"
adb="$ANDROID_HOME/platform-tools/adb"
emu="$ANDROID_HOME/emulator/emulator"
image="system-images;android-34;google_apis;x86_64"
apk="$root/android/app/build/outputs/apk/debug/app-debug.apk"
if [[ ! -x $emu || ! -d $ANDROID_HOME/system-images/android-34 ]]; then
	blocked A28 "no emulator or API 34 image (sdkmanager \"emulator\" \"$image\")"
	exit 0
fi
if ! with kvm test -w /dev/kvm; then
	blocked A28 "no KVM access (the kvm group; scripts/setup-root.sh)"
	exit 0
fi
[[ -f $apk ]] || "$root/scripts/build-android.sh" >"$LOGDIR/build.log" 2>&1 || { fail A28 "the APK didn't build ($LOGDIR/build.log)"; exit 1; }
cargo build -q -p qr-host --manifest-path "$root/Cargo.toml" || exit 1

pids=()
cleanup() {
	"$adb" -s emulator-5580 emu kill >/dev/null 2>&1 || true
	# The host runs under `sg`, a level below the PIDs recorded here. Match
	# qr-host processes only (a pattern match on every command line would
	# also hit whatever shell started this script).
	for p in $(pgrep -x qr-host); do
		tr '\0' ' ' <"/proc/$p/cmdline" 2>/dev/null | grep -qE -- "--name android-(host|vd)" && kill "$p" 2>/dev/null
	done
	for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
	sleep 1
	for p in "${pids[@]}"; do kill -9 "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

# The host: an X display with xev recording keys and buttons.
D=:84
Xvfb $D -screen 0 1280x720x24 -nolisten tcp >"$LOGDIR/xvfb.log" 2>&1 &
pids+=($!)
sleep 1
setxkbmap -display $D us
LC_ALL=C.UTF-8 xev -display $D -geometry 1270x700+0+0 -event keyboard -event button >"$LOGDIR/xev.log" 2>&1 &
pids+=($!)
sleep 1
# The input group gives the host uinput pads (the gamepad test).
GST_REGISTRY="$LOGDIR/gst.bin" with input "$root/target/debug/qr-host" --listen 0.0.0.0:9455 --config-dir "$LOGDIR/host" \
	--name android-host --pin 424242 --encoder x264enc --source x11:$D --fps 30 --audio none >"$LOGDIR/host.log" 2>&1 &
pids+=($!)

# The emulator.
"$ANDROID_HOME/cmdline-tools/latest/bin/avdmanager" create avd -n qr34 -k "$image" -d pixel_5 --force <<<"no" >"$LOGDIR/avd.log" 2>&1
with kvm "$emu" -avd qr34 -port 5580 -no-window -no-audio -no-boot-anim -no-snapshot -gpu swiftshader_indirect \
	>"$LOGDIR/emulator.log" 2>&1 &
pids+=($!)
A() { "$adb" -s emulator-5580 "$@"; }
A wait-for-device
for _ in $(seq 240); do [[ "$(A shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" == 1 ]] && break; sleep 1; done
A shell input keyevent KEYCODE_WAKEUP
# Screen on and unlocked: behind the lock screen the session view gets no
# surface, so nothing is decoded.
# Root: raw multi-touch events go straight to the touch device.
A root >/dev/null 2>&1
A wait-for-device
for _ in $(seq 30); do [[ "$(A shell id -u 2>/dev/null | tr -d '\r')" == 0 ]] && break; sleep 1; done
A shell svc power stayon true
A shell wm dismiss-keyguard
A shell input keyevent KEYCODE_MENU
# The emulator's network comes up after boot: wait until the host answers.
for _ in $(seq 60); do A shell ping -c 1 -W 1 10.0.2.2 >/dev/null 2>&1 && break; sleep 1; done
A install -r "$apk" >"$LOGDIR/install.log" 2>&1 || { fail A28 "install failed ($LOGDIR/install.log)"; exit 1; }
A logcat -c
A logcat -v time >"$LOGDIR/logcat-live.txt" 2>&1 &
pids+=($!)
# No system overlays over the session: the full-screen tip, "isn't
# responding" dialogs from a busy emulator; no animations.
A shell settings put secure immersive_mode_confirmations confirmed
A shell settings put global hide_error_dialogs 1
for k in window_animation_scale transition_animation_scale animator_duration_scale; do A shell settings put global $k 0; done
A shell am broadcast -a android.intent.action.CLOSE_SYSTEM_DIALOGS >/dev/null 2>&1
# Landscape, held there (the session view follows rotation).
A shell settings put system accelerometer_rotation 0
A shell wm user-rotation lock 1
A shell am start -n dev.quickremote/.MainActivity --es connect 10.0.2.2:9455 --es pin 424242 >/dev/null
for _ in $(seq 60); do A logcat -d -s QuickRemote:I | grep -q "stats fps=[1-9]" && break; sleep 1; done
# The launcher holds portrait while it is on top: landscape again now.
A shell wm user-rotation lock 1
sleep 3

# Input: a key, a tap (trackpad mode: a click) and a gamepad button.
A shell input keyevent KEYCODE_Q
sleep 0.5
# The middle of the (landscape) screen.
read -r pw ph < <(A shell wm size | grep -o '[0-9]*x[0-9]*' | tail -1 | tr 'x' ' ')
lw=$((pw > ph ? pw : ph)) lh=$((pw > ph ? ph : pw))
A shell input tap $((lw / 2)) $((lh / 2))
sleep 0.5
pad=$(grep -l "QuickRemote" /sys/class/input/*/name 2>/dev/null | head -1 || true)
A shell input gamepad keyevent KEYCODE_BUTTON_A
sleep 1

# A12 on Android: multi-finger gestures through the emulator's own
# multi-touch device (raw events: `input` has single touches only). Panel
# coordinates are 0–32767 in the natural orientation; the gestures don't
# depend on where exactly they land.
mtdev=$(A shell getevent -pl 2>/dev/null | tr -d '\r' | awk '/^add device/{d=$4} /name:.*"virtio_input_multi_touch_1"/{print d}')
# mt <fingers> <x> <y> <dx> <dy> <spread-step> <steps>: fingers land around
# (x, y), move by (dx, dy) and spread by spread-step per step, then lift.
# Each frame is written whole (one process per event would stretch a tap
# past the tap time on a busy emulator).
mt() {
	local frames="$LOGDIR/mt"
	rm -rf "$frames"; mkdir -p "$frames"
	python3 - "$frames" "$@" <<'PY'
import struct, sys
out, n, x, y, dx, dy, sp, steps = sys.argv[1], *map(int, sys.argv[2:])
ev = lambda t, c, v: struct.pack("<qqHHi", 0, 0, t, c, v & 0xFFFFFFFF if v < 0 else v)
frames = []
f = b""
for i in range(n):
    f += ev(3, 47, i) + ev(3, 57, 100 + i) + ev(3, 55, 0) + ev(3, 53, x + (i - n // 2) * 3000) + ev(3, 54, y) + ev(3, 58, 60) + ev(3, 48, 6)
frames.append(f + ev(0, 0, 0))
for s in range(1, steps + 1):
    f = b""
    for i in range(n):
        f += ev(3, 47, i) + ev(3, 53, x + (i - n // 2) * (3000 + s * sp) + s * dx) + ev(3, 54, y + s * dy)
    frames.append(f + ev(0, 0, 0))
f = b"".join(ev(3, 47, i) + struct.pack("<qqHHi", 0, 0, 3, 57, -1) for i in range(n))
frames.append(f + ev(0, 0, 0))
for k, f in enumerate(frames):
    open(f"{out}/{k:03d}", "wb").write(f)
PY
	# A tap: down and up in one write (a busy emulator takes longer to
	# start a process than a tap may last).
	if (($7 == 0)); then cat "$frames"/* >"$frames/tap"; rm "$frames"/0*; mv "$frames/tap" "$frames/000"; fi
	A shell rm -rf /data/local/tmp/mt
	A push "$frames" /data/local/tmp/ >/dev/null
	local cmd="" k last
	last=$(ls "$frames" | tail -1)
	for k in $(ls "$frames"); do
		[[ $k == "$last" ]] && cmd+="sleep 0.06;"
		cmd+="cat /data/local/tmp/mt/$k >$mtdev;"
		[[ $k != 000 && $k != "$last" ]] && cmd+="sleep 0.03;"
	done
	A shell "$cmd"
}
if [[ -n $mtdev ]]; then
	A shell input swipe $((lw / 2)) $((lh / 2)) $((lw / 2)) $((lh / 2)) 900 # long press
	sleep 0.8
	mt 2 16000 16000 0 0 0 0; sleep 0.8          # two-finger tap
	mt 3 16000 16000 0 0 0 0; sleep 0.8          # three-finger tap
	mt 2 16000 12000 0 600 0 12; sleep 0.8       # two-finger drag
	mt 2 16000 16000 0 0 500 12; sleep 0.8       # pinch out
	mt 4 16000 16000 0 0 0 0; sleep 1.5          # four-finger tap
fi
A exec-out screencap -p >"$LOGDIR/screen.png"
A logcat -d -s QuickRemote:I >"$LOGDIR/logcat.txt"
A logcat -d >"$LOGDIR/logcat-all.txt"

frames=$(grep -o "frames=[0-9]*" "$LOGDIR/logcat-live.txt" | tail -1 | cut -d= -f2)
fps=$(grep -o "stats fps=[0-9.]*" "$LOGDIR/logcat-live.txt" | cut -d= -f2 | sort -n | tail -1)
codec=$(grep -o "codec=[^ ]*" "$LOGDIR/logcat-live.txt" | tail -1 | cut -d= -f2)
# The emulator decodes and renders in software: no frame-rate target, but
# the stream must keep coming.
check A28a "the app connects and decodes (${frames:-0} frames, $codec, best ${fps:-0} fps)" \
	python3 -c "import sys; sys.exit(0 if int('${frames:-0}') >= 30 else 1)"
check A28b "a key from the emulator reaches the host (xev KeyPress q)" grep -q "keysym 0x71, q" "$LOGDIR/xev.log"
check A28c "a tap reaches the host as a click (xev ButtonPress)" grep -q "ButtonPress event" "$LOGDIR/xev.log"
if grep -q "gamepads and pen are off" "$LOGDIR/host.log"; then
	blocked A28d "the host has no uinput (the input group) for the gamepad"
else
	check A28d "a gamepad button from the emulator attaches a pad on the host" \
		grep -q "virtual gamepad attached" "$LOGDIR/host.log"
fi
# The video layer may not be in the emulator's screen capture (a hardware
# overlay), so this checks for a full-size capture, not its content.
python3 - "$LOGDIR/screen.png" >"$LOGDIR/screen.txt" <<'PY'
import struct, sys
d = open(sys.argv[1], "rb").read()
if d[:8] == b"\x89PNG\r\n\x1a\n" and d[12:16] == b"IHDR":
    w, h = struct.unpack(">II", d[16:24])
    print(f"png {w}x{h}")
else:
    print("bad", len(d))
PY
check A28e "a screenshot of the session ($(cat "$LOGDIR/screen.txt"))" grep -qE "^png ([0-9]{3,})x([0-9]{3,})" "$LOGDIR/screen.txt"
lg() { grep -c "$1" "$LOGDIR/logcat-live.txt"; }
if [[ -z $mtdev ]]; then
	blocked A12-android "no virtio multi-touch device in the emulator"
else
	check A12a-android "long press is a right click (xev button 3, $(lg "gesture long-press") long presses)" \
		sh -c "grep -q 'gesture long-press' '$LOGDIR/logcat-live.txt' && grep -q 'button 3,' '$LOGDIR/xev.log'"
	check A12b-android "two-finger tap is a right click, three-finger tap a middle click (xev button 2)" \
		sh -c "grep -q 'gesture tap 2' '$LOGDIR/logcat-live.txt' && grep -q 'gesture tap 3' '$LOGDIR/logcat-live.txt' && grep -q 'button 2,' '$LOGDIR/xev.log'"
	check A12c-android "two-finger drag scrolls (xev buttons 4–7: $(grep -cE 'button [4-7],' "$LOGDIR/xev.log"))" \
		grep -qE "button [4-7]," "$LOGDIR/xev.log"
	check A12d-android "pinch zooms the view ($(grep -o 'view zoom [0-9]*%' "$LOGDIR/logcat-live.txt" | tail -1))" \
		sh -c "grep 'view zoom' '$LOGDIR/logcat-live.txt' | grep -vq 'zoom 100%'"
	check A12e-android "four-finger tap shows the keyboard" grep -q "keyboard shown" "$LOGDIR/logcat-live.txt"
fi

# A16: follow the screen through a rotation, against a host with its own
# (resizable) display. Toolbar first.
A shell uiautomator dump /sdcard/ui.xml >/dev/null 2>&1
A shell cat /sdcard/ui.xml >"$LOGDIR/ui.xml" 2>/dev/null
check A16a "the toolbar shows Keyboard, Zoom and Display" \
	sh -c "grep -q 'content-desc=\"Keyboard\"' '$LOGDIR/ui.xml' && grep -q 'content-desc=\"Display settings\"' '$LOGDIR/ui.xml'"
if [[ ! -w /dev/dri/renderD128 ]] && ! with render test -w /dev/dri/renderD128; then
	blocked A16b "no GPU render node for the host's virtual display (the render group)"
else
	GST_REGISTRY="$LOGDIR/gst2.bin" with render "$root/target/debug/qr-host" --listen 0.0.0.0:9456 --config-dir "$LOGDIR/host2" \
		--name android-vd --pin 424242 --encoder vah264lpenc --source "vdisplay:sleep 600" --size 1280x720 --fps 30 --audio none \
		>"$LOGDIR/host2.log" 2>&1 &
	pids+=($!)
	sleep 3
	A shell am force-stop dev.quickremote
	A shell am start -n dev.quickremote/.MainActivity --es connect 10.0.2.2:9456 --es pin 424242 >/dev/null
	for _ in $(seq 40); do grep -q "display request" "$LOGDIR/logcat-live.txt" && break; sleep 1; done
	# The last stream size is portrait (1) or landscape (0).
	tall() { grep -o "stream [0-9]*x[0-9]*" "$LOGDIR/logcat-live.txt" | tail -1 | awk -v want="$1" '{split($2,a,"x"); exit !((a[2]>a[1]) == want)}'; }
	A shell wm user-rotation lock 1
	for _ in $(seq 20); do tall 0 && break; sleep 1; done
	sleep 2
	A shell wm user-rotation lock 0
	for _ in $(seq 20); do tall 1 && break; sleep 1; done
	sleep 1
	grep -E "display request|stream [0-9]+x[0-9]+|orientation" "$LOGDIR/logcat-live.txt" | sed 's/.*QuickRemote[^:]*: //' >"$LOGDIR/a16.txt"
	last=$(grep -o "stream [0-9]*x[0-9]*" "$LOGDIR/a16.txt" | tail -1)
	check A16b "rotating to portrait in Follow screen asks for a portrait display and the stream follows ($(grep -c 'display request' "$LOGDIR/a16.txt") requests, now $last)" \
		sh -c "grep 'display request' '$LOGDIR/a16.txt' | awk '{split(\$3,a,\"x\"); if (a[2]>a[1]) p=1; else l=1} END{exit !(p&&l)}' && echo '$last' | awk '{split(\$2,a,\"x\"); exit !(a[2]>a[1])}'"
fi

# KEEP=<seconds>: leave the emulator and hosts up for a look.
sleep "${KEEP:-0}"
echo "--- results ($LOGDIR)"
cat "$RESULTS"
! grep -q "^FAIL" "$RESULTS"
