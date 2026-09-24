#!/usr/bin/env bash
# Runs a pokebot command as one instance behind the hub on port 8080, with
# its web UI at http://<host>:8080/<instance>/, and keeps it up after it
# finishes (--hold). Starting an instance replaces only the previous run of
# the same instance: the Switch and the emulator run side by side.
#
# Usage: tools/live-run.sh [--instance switch|emu] <log file> <pokebot args...>
#   --instance defaults to switch when an argument starts with capture-card:,
#   else emu.
#
# Instance | process (/tmp copy) | backend port | label
# switch   | pokebot-switch      | 18080        | Switch
# emu      | pokebot-emu         | 18081        | Emulator
#
# The hub (`pokebot hub`, process pokebot-hub) is started if it isn't
# running; it serves / -> /switch/, /switch/, /emu/ and /api/instances.
# Each instance is described in /tmp/pokebot-instances/<instance>.json.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INSTANCES_DIR="${POKEBOT_INSTANCES_DIR:-/tmp/pokebot-instances}"
HUB_PORT=8080

INSTANCE=""
if [[ "${1:-}" == "--instance" ]]; then
  INSTANCE="${2:?--instance needs switch or emu}"
  shift 2
fi
if [[ $# -lt 2 ]]; then
  echo "usage: $0 [--instance switch|emu] <log file> <pokebot args...>" >&2
  exit 2
fi
LOG="$1"; shift
if [[ -z "${INSTANCE}" ]]; then
  INSTANCE=emu
  for arg in "$@"; do
    if [[ "${arg}" == capture-card:* ]]; then INSTANCE=switch; break; fi
  done
fi
case "${INSTANCE}" in
  switch) PORT=18080; LABEL=Switch ;;
  emu) PORT=18081; LABEL=Emulator ;;
  *) echo "unknown instance '${INSTANCE}' (switch or emu)" >&2; exit 2 ;;
esac
NAME="pokebot-${INSTANCE}"

# Ctrl-C first so the run can finish cleanly, then TERM after 5 s.
stop_process() {
  local name="$1"
  pgrep -x "${name}" >/dev/null || return 0
  pkill -INT -x "${name}" 2>/dev/null || true
  for _ in $(seq 50); do pgrep -x "${name}" >/dev/null || return 0; sleep 0.1; done
  echo "${name} ignored SIGINT for 5 s; sending SIGTERM"
  pkill -TERM -x "${name}" 2>/dev/null || true
  for _ in $(seq 30); do pgrep -x "${name}" >/dev/null || return 0; sleep 0.1; done
  echo "warning: ${name} is still running" >&2
}

# Replace only this instance. The pre-hub name pokebot-webrun was the Switch
# view and held port 8080 itself.
stop_process "${NAME}"
if [[ "${INSTANCE}" == switch ]] && pgrep -x pokebot-webrun >/dev/null; then
  echo "note: replacing the legacy pokebot-webrun (port ${HUB_PORT}); the Switch view is now at /switch/"
  stop_process pokebot-webrun
fi

BIN="${ROOT}/target/release/pokebot"
if ! pgrep -x pokebot-hub >/dev/null; then
  if pgrep -x pokebot-webrun >/dev/null; then
    echo "note: stopping the legacy pokebot-webrun, which holds port ${HUB_PORT}; the Switch view moves to /switch/ and must be relaunched with: $0 --instance switch <log> <args...>"
    stop_process pokebot-webrun
  fi
  cp "${BIN}" /tmp/pokebot-hub
  (cd "${ROOT}" && setsid nohup /tmp/pokebot-hub hub --listen "0.0.0.0:${HUB_PORT}" \
    > /tmp/pokebot-hub.log 2>&1 < /dev/null &)
  sleep 0.5
  if pgrep -x pokebot-hub >/dev/null; then
    echo "started the hub on port ${HUB_PORT} (log /tmp/pokebot-hub.log)"
  else
    echo "warning: the hub did not start; see /tmp/pokebot-hub.log" >&2
    cat /tmp/pokebot-hub.log >&2 || true
  fi
fi

cp "${BIN}" "/tmp/${NAME}"
cd "${ROOT}"
setsid nohup "/tmp/${NAME}" "$@" --web "127.0.0.1:${PORT}" --hold --instance-label "${LABEL}" \
  > "${LOG}" 2>&1 < /dev/null &
PID=$!

mkdir -p "${INSTANCES_DIR}"
python3 - "${INSTANCES_DIR}/${INSTANCE}.json" "${INSTANCE}" "${LABEL}" "${PORT}" "${PID}" \
  "$(realpath -m "${LOG}")" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "${NAME}" "$@" <<'EOF'
import json, os, shlex, sys
path, name, label, port, pid, log, started_at, binary, *args = sys.argv[1:]
entry = {
    "name": name, "label": label, "port": int(port), "pid": int(pid),
    "command": shlex.join([binary, *args]), "log": log, "started_at": started_at,
}
with open(path + ".tmp", "w") as f:
    json.dump(entry, f)
os.replace(path + ".tmp", path)
EOF

LAN_IP="$(ip -4 route get 1.1.1.1 | sed -n 's/.* src \([0-9.]*\).*/\1/p')"
echo "running ${INSTANCE} (pid ${PID}), log ${LOG}, UI http://${LAN_IP}:${HUB_PORT}/${INSTANCE}/"
