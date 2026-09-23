#!/usr/bin/env bash
# Fetches the pret/pokefirered decompilation (map, tileset and font data) and
# builds data/world/ (map renders, world model, gamedata.json; local,
# gitignored). Safe to re-run.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PRET="${ROOT}/data/pret-pokefirered"
if [ ! -d "${PRET}/.git" ]; then
    git clone --depth 1 --filter=blob:none --sparse https://github.com/pret/pokefirered.git "${PRET}"
    git -C "${PRET}" sparse-checkout set data/maps data/layouts data/tilesets src include
fi
# Paths added after the first clone (no-op when already present).
git -C "${PRET}" sparse-checkout add graphics/fonts data/scripts
VENV="${ROOT}/.venv"
if [ ! -x "${VENV}/bin/python" ]; then
    python3 -m venv "${VENV}"
    "${VENV}/bin/pip" install -q -r "${ROOT}/tools/world/requirements.txt"
fi
cd "${ROOT}"
"${VENV}/bin/python" tools/world/extract_world.py "$@"
"${VENV}/bin/python" tools/gamedata/extract_gamedata.py
"${VENV}/bin/python" tools/gamedata/extract_font.py
"${VENV}/bin/python" tools/gamedata/extract_font.py --font small
