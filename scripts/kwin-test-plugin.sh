#!/bin/bash
# For the KDE tests on a machine with KWin but not Plasma: KWin's screencast
# plugin ships in kwin-common (which pulls in all of Plasma). This extracts
# just that plugin into ~/.cache/quickremote/kwin-plugins without installing
# anything; tests/e2e/kde_session.sh uses it when the system has none.
set -euo pipefail
dest="${XDG_CACHE_HOME:-$HOME/.cache}/quickremote/kwin-plugins"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
(cd "$tmp" && apt-get download kwin-common >/dev/null)
dpkg -x "$tmp"/kwin-common_*.deb "$tmp/x"
mkdir -p "$dest/kwin/plugins"
cp "$tmp"/x/usr/lib/*/qt6/plugins/kwin/plugins/screencast.so "$dest/kwin/plugins/"
if ldd "$dest/kwin/plugins/screencast.so" | grep -q "not found"; then
	echo "warning: the plugin needs libraries that aren't installed:" >&2
	ldd "$dest/kwin/plugins/screencast.so" | grep "not found" >&2
fi
echo "$dest"
