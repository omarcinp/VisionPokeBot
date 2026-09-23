#!/usr/bin/env bash
# Builds the firmware. Pass `qemu` as the first argument for the QEMU image,
# anything else is forwarded to cargo.
#
#   tools/build.sh                 hardware, debug
#   tools/build.sh --release       hardware, release
#   tools/build.sh qemu            QEMU (OpenETH, no USB)
#
# WiFi credentials are baked in at build time:
#   WIFI_SSID=... WIFI_PASS=... tools/build.sh --release
# Without WIFI_SSID the firmware starts its own access point instead.
set -euo pipefail
cd "$(dirname "$0")/.."

# shellcheck disable=SC1091
source "$HOME/export-esp.sh"
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"

if [[ ${1:-} == qemu ]]; then
    shift
    export CARGO_TARGET_DIR="$PWD/target/qemu"
    export ESP_IDF_SDKCONFIG_DEFAULTS="$PWD/sdkconfig.defaults;$PWD/sdkconfig.qemu"
    exec cargo build --features qemu "$@"
fi
exec cargo build "$@"
