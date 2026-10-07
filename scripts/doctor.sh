#!/bin/bash
# Checks what building and testing QuickRemote needs on this machine and
# prints OK, or the fix, for each item. Exit status: number of problems.
#   scripts/doctor.sh
set -u
fails=0
ok() { printf '  \e[32mOK\e[0m    %s\n' "$1"; }
fix() { printf '  \e[31mFIX\e[0m   %s\n        → %s\n' "$1" "$2"; fails=$((fails + 1)); }
note() { printf '  \e[33mNOTE\e[0m  %s\n        → %s\n' "$1" "$2"; }
has() { command -v "$1" >/dev/null 2>&1; }
# Runs "$@" with group $1 if this session lacks it but the account has it.
in_group() {
	local g="$1"
	shift
	if id -nG | tr ' ' '\n' | grep -qx "$g"; then "$@"; else sg "$g" -c "$(printf '%q ' "$@")"; fi
}
setup="run: sudo scripts/setup-root.sh \$USER (then log out and in)"

echo "Devices and groups"
rn=/dev/dri/renderD128
if [[ -r $rn && -w $rn ]]; then
	ok "GPU render node $rn"
elif sg render -c "test -w $rn" 2>/dev/null; then
	note "GPU render node: the account is in 'render' but this session isn't" "log out and in, or prefix GPU commands with: sg render -c '…'"
else
	fix "GPU render node $rn not writable" "$setup"
fi
if [[ -w /dev/uinput ]]; then
	ok "/dev/uinput (pads, pen, Wayland-desktop input)"
elif sg input -c "test -w /dev/uinput" 2>/dev/null; then
	note "/dev/uinput: the account is in 'input' but this session isn't" "log out and in, or: sg input -c '…'"
else
	fix "/dev/uinput not writable" "$setup"
fi
if [[ -w /dev/kvm ]]; then
	ok "/dev/kvm (Android emulator)"
elif sg kvm -c "test -w /dev/kvm" 2>/dev/null; then
	note "/dev/kvm: the account is in 'kvm' but this session isn't" "log out and in, or: sg kvm -c '…'"
else
	fix "/dev/kvm not writable (Android emulator tests)" "$setup"
fi
rmem=$(cat /proc/sys/net/core/rmem_max)
if ((rmem >= 16777216)); then ok "net.core.rmem_max = $rmem"; else fix "net.core.rmem_max = $rmem (bursts drop packets)" "$setup"; fi

echo "Encoders and decoders"
if has gst-inspect-1.0; then
	reg=$(mktemp -d)
	out=$(GST_REGISTRY=$reg/r.bin in_group render gst-inspect-1.0 va 2>/dev/null)
	rm -rf "$reg"
	if grep -q vah264lpenc <<<"$out"; then ok "VA-API H.264 encoder (vah264lpenc)"; else fix "no VA-API H.264 encoder" "install intel-media-va-driver (Intel) or mesa-va-drivers (AMD); check the render group"; fi
	if grep -qE "vah265(lp)?enc" <<<"$out"; then ok "VA-API HEVC encoder"; else note "no VA-API HEVC encoder (H.264 only)" "Intel: enable Debian's non-free component and install intel-media-va-driver-non-free"; fi
	for e in x264enc opusenc opusdec pipewiresrc avdec_h264; do
		if gst-inspect-1.0 "$e" >/dev/null 2>&1; then ok "GStreamer $e"; else fix "GStreamer element $e missing" "$setup (gstreamer1.0-plugins-ugly/-good/-libav, gstreamer1.0-pipewire)"; fi
	done
else
	fix "gst-inspect-1.0 missing" "$setup"
fi

echo "Build libraries (pkg-config)"
for p in gstreamer-1.0 gstreamer-app-1.0 gstreamer-video-1.0 gstreamer-allocators-1.0 libpipewire-0.3 opus xkbcommon libudev gbm egl glesv2; do
	if pkg-config --exists "$p" 2>/dev/null; then ok "$p $(pkg-config --modversion "$p")"; else fix "$p" "$setup"; fi
done

echo "Virtual display"
for b in sway swaymsg pipewire wireplumber pipewire-pulse pw-metadata dbus-daemon; do
	if has "$b"; then ok "$b"; else fix "$b missing" "$setup"; fi
done
for b in /usr/libexec/xdg-desktop-portal /usr/libexec/xdg-desktop-portal-wlr; do
	if [[ -x $b ]]; then ok "$b"; else fix "$b missing" "$setup"; fi
done
if [[ -e /usr/lib/x86_64-linux-gnu/libgstva-1.0.so.0 ]]; then ok "libgstva-1.0 (zero-copy capture)"; else fix "libgstva-1.0 missing" "$setup (gstreamer1.0-plugins-bad)"; fi

echo "Test harness"
if unshare -rn true 2>/dev/null; then ok "user network namespaces (unshare -rn)"; else fix "unshare -rn fails" "sudo sysctl -w kernel.unprivileged_userns_clone=1 (and check AppArmor's userns restriction)"; fi
if unshare -rn sh -c 'ip link add qra type veth peer name qrb && tc qdisc add dev qra root netem delay 1ms' 2>/dev/null; then
	ok "veth + netem in a namespace"
else
	fix "veth/netem unavailable in a user namespace" "sudo modprobe sch_netem veth"
fi
for b in Xvfb xdotool xev python3 node; do
	if has "$b"; then ok "$b"; else fix "$b missing" "$setup (node: any Node.js ≥ 18 for the web client)"; fi
done
python3 -c 'import numpy' 2>/dev/null && ok "python3 numpy" || fix "python3 numpy missing" "sudo apt install python3-numpy"
if has google-chrome || has chromium; then ok "Chrome/Chromium (web client tests)"; else fix "no Chrome or Chromium" "install google-chrome-stable or chromium"; fi
if docker info >/dev/null 2>&1; then
	ok "docker"
elif sg docker -c "docker info" >/dev/null 2>&1; then
	note "docker: the account is in 'docker' but this session isn't" "log out and in, or: sg docker -c '…'"
else
	fix "docker unavailable (relay image test A5)" "$setup"
fi

echo "Cross builds"
[[ -d ${ANDROID_HOME:-$HOME/Android/sdk} ]] && ok "Android SDK" || note "no Android SDK" "scripts/build-android.sh installs it under ~/Android"
rustup target list --installed 2>/dev/null | grep -q x86_64-pc-windows-msvc && ok "Rust target x86_64-pc-windows-msvc" ||
	note "no Windows Rust target" "rustup target add x86_64-pc-windows-msvc"

echo
if ((fails == 0)); then echo "All required items OK."; else echo "$fails item(s) to fix."; fi
exit "$fails"
