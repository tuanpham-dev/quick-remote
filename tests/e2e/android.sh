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
with() { local g="$1"; shift; if id -nG | tr ' ' '\n' | grep -qx "$g"; then "$@"; else sg "$g" -c "$(printf '%q ' "$@")"; fi; }

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
"$ANDROID_HOME/cmdline-tools/latest/bin/avdmanager" create avd -n qr34 -k "$image" --force <<<"no" >"$LOGDIR/avd.log" 2>&1
with kvm "$emu" -avd qr34 -port 5580 -no-window -no-audio -no-boot-anim -no-snapshot -gpu swiftshader_indirect \
	>"$LOGDIR/emulator.log" 2>&1 &
pids+=($!)
A() { "$adb" -s emulator-5580 "$@"; }
A wait-for-device
for _ in $(seq 240); do [[ "$(A shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" == 1 ]] && break; sleep 1; done
A shell input keyevent KEYCODE_WAKEUP
A install -r "$apk" >"$LOGDIR/install.log" 2>&1 || { fail A28 "install failed ($LOGDIR/install.log)"; exit 1; }
A logcat -c
A shell am start -n dev.quickremote/.MainActivity --es connect 10.0.2.2:9455 --es pin 424242 >/dev/null
for _ in $(seq 60); do A logcat -d -s QuickRemote:I | grep -q "stats fps=[1-9]" && break; sleep 1; done
sleep 3

# Input: a key, a tap (trackpad mode: a click) and a gamepad button.
A shell input keyevent KEYCODE_Q
sleep 0.5
A shell input tap 640 360
sleep 0.5
pad=$(grep -l "QuickRemote" /sys/class/input/*/name 2>/dev/null | head -1 || true)
A shell input gamepad keyevent KEYCODE_BUTTON_A
sleep 1
A exec-out screencap -p >"$LOGDIR/screen.png"
A logcat -d -s QuickRemote:I >"$LOGDIR/logcat.txt"

fps=$(grep -o "stats fps=[0-9.]*" "$LOGDIR/logcat.txt" | tail -3 | cut -d= -f2 | sort -n | head -1)
codec=$(grep -o "codec=[^ ]*" "$LOGDIR/logcat.txt" | tail -1 | cut -d= -f2)
check A28a "the app connects and decodes ($codec, at least ${fps:-0} fps in the last seconds)" \
	python3 -c "import sys; sys.exit(0 if float('${fps:-0}') >= 20 else 1)"
check A28b "a key from the emulator reaches the host (xev KeyPress q)" grep -q "keysym 0x71, q" "$LOGDIR/xev.log"
check A28c "a tap reaches the host as a click (xev ButtonPress)" grep -q "ButtonPress event" "$LOGDIR/xev.log"
if grep -q "gamepads and pen are off" "$LOGDIR/host.log"; then
	blocked A28d "the host has no uinput (the input group) for the gamepad"
else
	check A28d "a gamepad button from the emulator attaches a pad on the host" \
		grep -q "virtual gamepad attached" "$LOGDIR/host.log"
fi
python3 - "$LOGDIR/screen.png" >"$LOGDIR/screen.txt" <<'PY'
import sys, zlib, struct
d = open(sys.argv[1], "rb").read()
ok = d[:8] == b"\x89PNG\r\n\x1a\n" and len(d) > 20000
print("png" if ok else "bad", len(d))
PY
check A28e "a screenshot of the session ($(cat "$LOGDIR/screen.txt"))" grep -q "^png" "$LOGDIR/screen.txt"
echo "--- results ($LOGDIR)"
cat "$RESULTS"
! grep -q "^FAIL" "$RESULTS"
