#!/usr/bin/env bash
# Builds the QEMU variant of the firmware and boots it in Espressif's QEMU.
# The emulated board's network is forwarded to this machine:
#
#   127.0.0.1:7878  control protocol   (pokebot --controller esp32:127.0.0.1)
#   127.0.0.1:8078  HTTP API + page    (http://127.0.0.1:8078/)
#
# Override the host ports with CONTROL_PORT / HTTP_PORT. Quit with Ctrl-A X.
set -euo pipefail
cd "$(dirname "$0")/.."

CONTROL_PORT=${CONTROL_PORT:-7878}
HTTP_PORT=${HTTP_PORT:-8078}
export PATH="$HOME/.local/bin:$PATH"

tools/build.sh qemu
elf=target/qemu/xtensa-esp32s3-espidf/debug/pokebot-esp32s3-controller
image=target/qemu/flash.bin
# QEMU needs one image of exactly the flash size, bootloader included.
espflash save-image --chip esp32s3 --merge --flash-size 4mb "$elf" "$image" >/dev/null

# eFuses of a rev 0.3 chip (as `idf.py qemu` uses), plus BLK_VERSION_MAJOR=1
# so ADC calibration comes from eFuse: QEMU does not emulate the SAR ADC and
# startup self-calibration would spin forever. BLK0 and BLK1 are 24 bytes.
efuse=target/qemu/efuse.bin
python3 -c "
b = bytearray(1024)
b[24 + 114 // 8] = 3 << (114 % 8)  # WAFER_VERSION_MINOR_LO = 3
b[48 + 128 // 8] = 1               # BLK_VERSION_MAJOR = 1 (ADC calib V1)
open('$efuse', 'wb').write(b)"

exec qemu-system-xtensa \
    -nographic \
    -machine esp32s3 \
    -m 32M \
    -drive "file=$image,if=mtd,format=raw" \
    -drive "file=$efuse,if=none,format=raw,id=efuse" \
    -global driver=nvram.esp32s3.efuse,property=drive,value=efuse \
    -global driver=timer.esp32s3.timg,property=wdt_disable,value=true \
    -nic "user,model=open_eth,hostfwd=tcp:127.0.0.1:$CONTROL_PORT-:7878,hostfwd=tcp:127.0.0.1:$HTTP_PORT-:80"
