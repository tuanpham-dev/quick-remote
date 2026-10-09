#!/bin/sh
# Builds everything for a manual test round into dist/quickremote-test/:
# release qr-host (with the web client inside), qr-client and qr-relay for
# this Linux machine, the Android APK, and the test guide.
#   scripts/package.sh [--no-android]
set -eu
cd "$(dirname "$0")/.."
out=dist/quickremote-test
rm -rf "$out"
mkdir -p "$out"
echo "== web client"
scripts/build-web.sh
echo "== Linux binaries (release)"
cargo build --release -p qr-host -p qr-client -p qr-relay
cp target/release/qr-host target/release/qr-client target/release/qr-relay "$out/"
cp deploy/kde/quickremote-host.desktop "$out/"
cp -r deploy/systemd "$out/systemd"
cp scripts/xfce-desktop.sh scripts/setup-intel-hevc.sh "$out/"
if [ "${1:-}" != "--no-android" ]; then
	echo "== Android"
	scripts/build-android.sh
	cp android/app/build/outputs/apk/debug/app-debug.apk "$out/quickremote.apk"
fi
cp docs/TESTING.md "$out/"
git rev-parse --short HEAD >"$out/VERSION"
ls -la "$out"
