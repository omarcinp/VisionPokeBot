#!/usr/bin/env bash
# Installs everything needed to build, flash and emulate the ESP32-S3
# controller firmware. Idempotent: re-running skips what is already there.
#
#   * apt packages ESP-IDF needs (cmake, ninja, libudev, ...)
#   * espup → the Xtensa Rust toolchain (`+esp`) and its LLVM/GCC
#   * espflash / cargo-espflash (flash + monitor), ldproxy (linker shim)
#   * Espressif's QEMU fork with ESP32-S3 support
#   * dialout group membership for /dev/ttyUSB* / /dev/ttyACM*
#
# ESP-IDF itself is downloaded by esp-idf-sys on the first `cargo build`
# (into ~/.espressif, see ESP_IDF_TOOLS_INSTALL_DIR in .cargo/config.toml).
set -euo pipefail

BIN="$HOME/.local/bin"
TOOLS="$HOME/.local/share/pokebot-esp"
ESPUP_VERSION="v0.17.1"
ESPFLASH_VERSION="v4.6.0"
LDPROXY_VERSION="ldproxy-v0.3.2"
QEMU_TAG="esp-develop-9.2.2-20260417"
QEMU_ASSET="qemu-xtensa-softmmu-esp_develop_9.2.2_20260417-x86_64-linux-gnu.tar.xz"

mkdir -p "$BIN" "$TOOLS"
export PATH="$BIN:$HOME/.cargo/bin:$PATH"

step() { printf '\n==> %s\n' "$*"; }

fetch_zip() { # repo tag asset binary
    local repo=$1 tag=$2 asset=$3 bin=$4
    if [[ -x "$BIN/$bin" ]]; then
        echo "$bin already installed"
        return
    fi
    local tmp
    tmp=$(mktemp -d)
    curl -fsSL -o "$tmp/a.zip" "https://github.com/$repo/releases/download/$tag/$asset"
    unzip -q -o "$tmp/a.zip" -d "$tmp"
    install -m 755 "$tmp/$bin" "$BIN/$bin"
    rm -rf "$tmp"
    echo "installed $bin $tag"
}

step "system packages"
sudo apt-get install -y -qq git curl unzip xz-utils python3 python3-venv python3-pip \
    cmake ninja-build flex bison gperf ccache dfu-util libffi-dev libssl-dev \
    libudev-dev libusb-1.0-0-dev libslirp0 libsdl2-2.0-0 libgcrypt20 clang libclang-dev

step "serial port access"
if id -nG "$USER" | grep -qw dialout; then
    echo "$USER is in dialout"
else
    sudo usermod -aG dialout "$USER"
    echo "added $USER to dialout (log out/in, or use 'sg dialout' until then)"
fi

step "esp-rs tools"
if [[ ! -x "$BIN/espup" ]]; then
    curl -fsSL -o "$BIN/espup" \
        "https://github.com/esp-rs/espup/releases/download/$ESPUP_VERSION/espup-x86_64-unknown-linux-gnu"
    chmod +x "$BIN/espup"
fi
fetch_zip esp-rs/espflash "$ESPFLASH_VERSION" espflash-x86_64-unknown-linux-gnu.zip espflash
fetch_zip esp-rs/espflash "$ESPFLASH_VERSION" cargo-espflash-x86_64-unknown-linux-gnu.zip cargo-espflash
fetch_zip esp-rs/embuild "$LDPROXY_VERSION" ldproxy-x86_64-unknown-linux-gnu.zip ldproxy

step "Xtensa Rust toolchain (rustup +esp)"
if rustup toolchain list | grep -q '^esp'; then
    echo "esp toolchain already installed"
else
    espup install --targets esp32s3 --export-file "$HOME/export-esp.sh"
fi

step "Espressif QEMU ($QEMU_TAG)"
if [[ -x "$TOOLS/qemu/bin/qemu-system-xtensa" ]]; then
    echo "qemu already installed"
else
    curl -fsSL "https://github.com/espressif/qemu/releases/download/$QEMU_TAG/$QEMU_ASSET" |
        tar -xJ -C "$TOOLS"
fi
ln -sf "$TOOLS/qemu/bin/qemu-system-xtensa" "$BIN/qemu-system-xtensa"
qemu-system-xtensa -machine help | grep -q esp32s3 && echo "qemu supports esp32s3"

step "done"
cat <<EOF
Before building firmware in a new shell:   source ~/export-esp.sh
Host simulator (no toolchain needed):      cargo run -p pokebot-remote --bin pokebot-remote-sim
Firmware in QEMU:                          firmware/esp32s3-controller/tools/qemu.sh
Firmware on hardware:                      firmware/esp32s3-controller/tools/flash.sh
EOF
