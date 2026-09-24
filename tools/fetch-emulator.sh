#!/usr/bin/env bash
# Downloads the mGBA libretro core used as the development stand-in for a
# console + capture card + controller. Only the frame and joypad interfaces of
# the core are ever used by PokéBot (see docs/architecture.md).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="${ROOT}/emulator"
URL="${MGBA_CORE_URL:-https://buildbot.libretro.com/nightly/linux/x86_64/latest/mgba_libretro.so.zip}"

mkdir -p "${DEST}"
TMP="$(mktemp -d)"
trap 'rm -rf "${TMP}"' EXIT

echo "Downloading ${URL}"
curl --fail --location --silent --show-error -o "${TMP}/core.zip" "${URL}"
python3 -c 'import sys, zipfile; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])' "${TMP}/core.zip" "${TMP}"
install -m 0755 "${TMP}/mgba_libretro.so" "${DEST}/mgba_libretro.so"
sha256sum "${DEST}/mgba_libretro.so"
echo "Installed ${DEST}/mgba_libretro.so"
