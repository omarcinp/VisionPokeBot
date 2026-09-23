#!/usr/bin/env bash
# Runs a pokebot command with the web UI on the LAN and keeps it up after it
# finishes (--hold), replacing whatever run held the UI before.
# Usage: tools/live-run.sh <log file> <pokebot args...>
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOG="$1"; shift
pkill -INT -x pokebot-webrun 2>/dev/null || true
for _ in $(seq 50); do pgrep -x pokebot-webrun >/dev/null || break; sleep 0.1; done
cp "${ROOT}/target/release/pokebot" /tmp/pokebot-webrun
cd "${ROOT}"
setsid nohup /tmp/pokebot-webrun "$@" --web 0.0.0.0:8080 --hold > "${LOG}" 2>&1 < /dev/null &
echo "running (pid $!), log ${LOG}, UI http://$(ip -4 route get 1.1.1.1 | sed -n 's/.* src \([0-9.]*\).*/\1/p'):8080"
