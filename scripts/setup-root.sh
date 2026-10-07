#!/bin/sh
# One-time root setup for building and testing QuickRemote on Debian 13.
# Run as: sudo scripts/setup-root.sh [user]   (then log out and back in)
set -eu
user="${1:-${SUDO_USER:-}}"
[ -n "$user" ] || { echo "usage: sudo $0 <user>" >&2; exit 2; }

grep -q 'non-free' /etc/apt/sources.list.d/debian.sources 2>/dev/null \
  || grep -qE '^deb .* non-free' /etc/apt/sources.list 2>/dev/null \
  || echo "note: enable the non-free component for intel-media-va-driver-non-free (HEVC/4:4:4 encode)"

apt-get update
apt-get install -y \
  libpipewire-0.3-dev libopus-dev libxkbcommon-dev libva-dev libasound2-dev \
  gstreamer1.0-libav gstreamer1.0-pipewire gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly \
  evtest xdotool x11-utils mesa-utils glmark2-es2-x11 vulkan-tools tcpdump \
  xdg-desktop-portal xdg-desktop-portal-gnome xdg-desktop-portal-wlr mutter \
  sway swaybg wlr-randr wayland-utils libseat1 dbus-x11 \
  xfce4-session xfwm4 xfce4-panel xterm \
  clang lld llvm msitools p7zip-full unzip
apt-get install -y intel-media-va-driver-non-free || echo "warning: intel-media-va-driver-non-free not installed (non-free component missing?)"

cat >/etc/udev/rules.d/70-quickremote.rules <<'EOF'
KERNEL=="uinput", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"
SUBSYSTEM=="input", ATTRS{name}=="QuickRemote*", GROUP="input", MODE="0660"
EOF
modprobe uinput
echo uinput >/etc/modules-load.d/quickremote.conf
udevadm control --reload-rules
udevadm trigger --name-match=uinput || true

# Large UDP socket buffers: video and file bursts overflow the default
# 208 KiB and drop packets.
cat >/etc/sysctl.d/90-quickremote.conf <<'SYSCTL'
net.core.rmem_max = 16777216
net.core.wmem_max = 16777216
SYSCTL
sysctl -p /etc/sysctl.d/90-quickremote.conf

for g in input kvm docker render video; do
  getent group "$g" >/dev/null && usermod -aG "$g" "$user"
done
echo "done: log out and back in (or reboot) so $user gets the new groups"
