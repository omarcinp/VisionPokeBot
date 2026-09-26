#!/usr/bin/env bash
# Downloads the libretro cores used as the development stand-in for a
# console + capture card + controller. Only the frame and joypad interfaces of
# the cores are ever used by PokéBot (see docs/architecture.md), plus gpSP's
# link port for trading between two emulators.
#
#   mgba_libretro.so  the default core
#   gpsp_libretro.so  links two emulators (Wireless Adapter / link cable)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="${ROOT}/emulator"
NIGHTLY="https://buildbot.libretro.com/nightly/linux/x86_64/latest"

mkdir -p "${DEST}"
TMP="$(mktemp -d)"
trap 'rm -rf "${TMP}"' EXIT

fetch() { # <core name> <url>
    echo "Downloading $2"
    curl --fail --location --silent --show-error -o "${TMP}/$1.zip" "$2"
    python3 -c 'import sys, zipfile; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])' "${TMP}/$1.zip" "${TMP}"
    install -m 0755 "${TMP}/$1_libretro.so" "${DEST}/$1_libretro.so"
    sha256sum "${DEST}/$1_libretro.so"
    echo "Installed ${DEST}/$1_libretro.so"
}

fetch mgba "${MGBA_CORE_URL:-${NIGHTLY}/mgba_libretro.so.zip}"
fetch gpsp "${GPSP_CORE_URL:-${NIGHTLY}/gpsp_libretro.so.zip}"
