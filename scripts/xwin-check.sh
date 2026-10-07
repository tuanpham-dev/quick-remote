#!/bin/sh
# Compile-checks the Windows host and client from Linux: cargo-xwin for the
# MSVC CRT and Windows SDK, GStreamer's MSVC development package for
# pkg-config (only headers and import libraries are used).
#   scripts/xwin-check.sh [cargo check args…]
# First time: cargo install cargo-xwin, and fetch GStreamer:
#   mkdir -p ~/.cache/quickremote/gst-msvc-1.26.6 && cd $_ &&
#   curl -LO https://gstreamer.freedesktop.org/data/pkg/windows/1.26.6/msvc/gstreamer-1.0-devel-msvc-x86_64-1.26.6.msi &&
#   mkdir x && cd x && msiextract ../gstreamer-1.0-devel-msvc-x86_64-1.26.6.msi
set -eu
cd "$(dirname "$0")/.."
GST="${GST_MSVC:-$HOME/.cache/quickremote/gst-msvc-1.26.6/x/PFiles64/gstreamer/1.0/msvc_x86_64}"
if [ ! -d "$GST/lib/pkgconfig" ]; then
	echo "GStreamer MSVC development files not found at $GST (see the comment at the top of $0)" >&2
	exit 2
fi
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_PATH_x86_64_pc_windows_msvc="$GST/lib/pkgconfig"
export XWIN_ACCEPT_LICENSE=1
exec cargo xwin check --target x86_64-pc-windows-msvc -p qr-host -p qr-client "$@"
