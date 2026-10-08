#!/bin/sh
# Hardware H.265 (HEVC) encoding on Intel GPUs (Gen9 and later): Intel's
# full media driver from Debian's non-free component. The free driver
# (intel-media-va-driver) leaves out HEVC encoding. Run with sudo:
#   sudo scripts/setup-intel-hevc.sh
set -eu
[ "$(id -u)" = 0 ] || { echo "run with sudo" >&2; exit 1; }

# 1. Enable non-free next to main on the Debian lines (a backup is kept).
list=/etc/apt/sources.list
if [ -f "$list" ] && ! grep -Eq '^deb .*debian.* main.* non-free( |$)' "$list"; then
	cp "$list" "$list.bak-quickremote"
	sed -i -E '/^deb(-src)? .*debian/ { / non-free( |$)/! s/ main( |$)/ main non-free\1/ }' "$list"
	echo "enabled non-free in $list (backup: $list.bak-quickremote)"
fi
for f in /etc/apt/sources.list.d/debian.sources; do
	[ -f "$f" ] || continue
	if ! grep -Eq '^Components:.* non-free( |$)' "$f"; then
		cp "$f" "$f.bak-quickremote"
		sed -i -E '/^Components:/ s/ main( |$)/ main non-free\1/' "$f"
		echo "enabled non-free in $f (backup: $f.bak-quickremote)"
	fi
done

# 2. The driver (it replaces the free one) and vainfo to check it.
apt-get update
apt-get install -y intel-media-va-driver-non-free vainfo

# 3. GStreamer caches which VA encoders exist: drop the caches so the
#    host finds the new ones.
for home in /root /home/*; do
	rm -rf "$home/.cache/gstreamer-1.0"
done

echo
echo "HEVC encoders now available (want VAProfileHEVCMain ... VAEntrypointEncSlice*):"
vainfo --display drm --device /dev/dri/renderD128 2>/dev/null | grep -E "Driver version|HEVC.*Enc" || true
echo
echo "GStreamer elements:"
gst-inspect-1.0 va 2>/dev/null | grep -E "va(h265|h264)(lp)?enc" || true
