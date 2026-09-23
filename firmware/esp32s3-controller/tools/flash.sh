#!/usr/bin/env bash
# Builds (release) and flashes the firmware, then opens the serial monitor.
#
#   WIFI_SSID=home WIFI_PASS=secret tools/flash.sh [--port /dev/ttyUSB0]
#
# Connect the PC to the board's UART port (CP210x/CH34x bridge, usually
# labelled "UART" or "COM"); the native USB port ("USB"/"OTG") goes to the
# Switch. If flashing fails to connect, hold BOOT, tap RESET, release BOOT.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.local/bin:$PATH"

tools/build.sh --release
exec espflash flash --chip esp32s3 --monitor "$@" \
    target/xtensa-esp32s3-espidf/release/pokebot-esp32s3-controller
