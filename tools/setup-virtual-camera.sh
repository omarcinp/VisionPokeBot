#!/usr/bin/env bash
# Creates /dev/video10, a virtual webcam that `pokebot emulator serve` writes
# the game video into. The bot then reads it through the same capture-card
# adapter that a real HDMI capture card uses. Needs sudo; lasts until reboot.
set -euo pipefail

DEVICE_NR="${1:-10}"
DEVICE="/dev/video${DEVICE_NR}"

if ! modinfo videodev >/dev/null 2>&1; then
    echo "Installing kernel media modules (videodev) for $(uname -r)"
    sudo apt-get install -y "linux-modules-extra-$(uname -r)"
fi
if [ ! -e "${DEVICE}" ]; then
    sudo modprobe v4l2loopback devices=1 video_nr="${DEVICE_NR}" \
        card_label="PokeBot Emulator" exclusive_caps=1
fi
sudo chown "$(id -un)" "${DEVICE}"
echo "ready: ${DEVICE} ($(cat "/sys/class/video4linux/video${DEVICE_NR}/name"))"
