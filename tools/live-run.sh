#!/usr/bin/env bash
# Runs a pokebot command as one instance behind the hub on port 8080, with
# its web UI at http://<host>:8080/<instance>/, and keeps it up after it
# finishes (--hold). Starting an instance replaces only the previous run of
# the same instance: the Switch and the emulator run side by side.
#
# Usage: tools/live-run.sh --hub <log file> [hub args...]
#        tools/live-run.sh [--instance switch|emu] <log file> <pokebot args...>
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

HUB_ONLY=0
if [[ "${1:-}" == "--hub" ]]; then HUB_ONLY=1; shift; fi
INSTANCE=""
if [[ "${1:-}" == "--instance" ]]; then
  INSTANCE="${2:?--instance needs switch or emu}"
  shift 2
fi
if [[ $# -lt 1 ]] || { [[ "${HUB_ONLY}" == 0 ]] && [[ $# -lt 2 ]]; }; then
  echo "usage: $0 --hub <log file> [hub args...] | [--instance switch|emu] <log file> <pokebot args...>" >&2
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

# Every long-lived process runs in its own systemd user unit (linger is on),
# never as a child of the shell or agent session that launched it: a
# `setsid nohup` child still dies with the launching service's cgroup (on
# 2026-09-24 a restart of the agent host killed the Switch run and the hub,
# and the idle Switch then went to sleep). Units are named after the process.
# The user manager lacks the video/dialout groups: `sg` adds them. A unit
# that exits with an error (the capture card or the ESP32 missing at start)
# is restarted after 30 s.
start_unit() {
  local unit="$1" log="$2"; shift 2
  systemctl --user stop "${unit}" 2>/dev/null || true
  systemctl --user reset-failed "${unit}" 2>/dev/null || true
  systemd-run --user --quiet --collect --unit="${unit}" \
    --working-directory="${ROOT}" \
    -p KillSignal=SIGINT -p TimeoutStopSec=10 \
    -p Restart=on-failure -p RestartSec=30 \
    -p StandardOutput="append:${log}" -p StandardError="append:${log}" \
    sg video -c "exec $(printf '%q ' "$@")"
}

unit_pid() {
  for _ in $(seq 50); do
    pgrep -x "$1" && return 0
    sleep 0.1
  done
  return 1
}

# Ctrl-C first so the run can finish cleanly, then TERM after 5 s.
stop_process() {
  local name="$1"
  systemctl --user stop "${name}" 2>/dev/null || true
  pgrep -x "${name}" >/dev/null || return 0
  pkill -INT -x "${name}" 2>/dev/null || true
  for _ in $(seq 50); do pgrep -x "${name}" >/dev/null || return 0; sleep 0.1; done
  echo "${name} ignored SIGINT for 5 s; sending SIGTERM"
  pkill -TERM -x "${name}" 2>/dev/null || true
  for _ in $(seq 30); do pgrep -x "${name}" >/dev/null || return 0; sleep 0.1; done
  echo "warning: ${name} is still running" >&2
}

# Start or upgrade just the supervisor; the physical Switch keeps running.
if [[ "${HUB_ONLY}" == 1 ]]; then
  stop_process pokebot-hub
  cp "${ROOT}/target/release/pokebot" /tmp/pokebot-hub
  start_unit pokebot-hub "$(realpath -m "${LOG}")" /tmp/pokebot-hub hub \
    --instances-dir "${INSTANCES_DIR}" "$@"
  unit_pid pokebot-hub >/dev/null || { echo "hub failed; see ${LOG}" >&2; exit 1; }
  echo "hub started (log ${LOG}); open /emulators/ to launch workers"
  exit 0
fi

# Replace only this instance. The pre-hub name pokebot-webrun was the Switch
# view and held port 8080 itself.
stop_process "${NAME}"
if [[ "${INSTANCE}" == switch ]] && pgrep -x pokebot-webrun >/dev/null; then
  echo "note: replacing the legacy pokebot-webrun (port ${HUB_PORT}); the Switch view is now at /switch/"
  stop_process pokebot-webrun
fi

BIN="${ROOT}/target/release/pokebot"

# Disk: recordings grow ~2 GB/hour. Prune now, and keep a watcher pruning
# every 30 minutes (log /tmp/pokebot-disk-guard.log).
"${ROOT}/tools/disk-guard.sh" --prune >> /tmp/pokebot-disk-guard.log 2>&1 \
  || echo "warning: disk-guard failed; see /tmp/pokebot-disk-guard.log" >&2
if ! systemctl --user is-active --quiet pokebot-disk-guard; then
  start_unit pokebot-disk-guard /tmp/pokebot-disk-guard.log "${ROOT}/tools/disk-guard.sh" --watch
  echo "started the disk guard (unit pokebot-disk-guard, log /tmp/pokebot-disk-guard.log)"
fi
if ! pgrep -x pokebot-hub >/dev/null; then
  if pgrep -x pokebot-webrun >/dev/null; then
    echo "note: stopping the legacy pokebot-webrun, which holds port ${HUB_PORT}; the Switch view moves to /switch/ and must be relaunched with: $0 --instance switch <log> <args...>"
    stop_process pokebot-webrun
  fi
  cp "${BIN}" /tmp/pokebot-hub
  : > /tmp/pokebot-hub.log
  start_unit pokebot-hub /tmp/pokebot-hub.log /tmp/pokebot-hub hub --listen "0.0.0.0:${HUB_PORT}" --instances-dir "${INSTANCES_DIR}"
  if unit_pid pokebot-hub >/dev/null; then
    echo "started the hub on port ${HUB_PORT} (log /tmp/pokebot-hub.log)"
  else
    echo "warning: the hub did not start; see /tmp/pokebot-hub.log" >&2
    cat /tmp/pokebot-hub.log >&2 || true
  fi
fi

cp "${BIN}" "/tmp/${NAME}"
cd "${ROOT}"
: > "${LOG}"
start_unit "${NAME}" "$(realpath -m "${LOG}")" \
  "/tmp/${NAME}" "$@" --web "127.0.0.1:${PORT}" --hold --instance-label "${LABEL}"
PID="$(unit_pid "${NAME}" || echo 0)"
if [[ "${PID}" == 0 ]]; then
  echo "error: ${NAME} did not start; see ${LOG} and: systemctl --user status ${NAME}" >&2
  exit 1
fi

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

# No route (offline) must not fail the launch: fall back to localhost.
LAN_IP="$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p' || true)"
LAN_IP="${LAN_IP:-localhost}"
echo "running ${INSTANCE} (unit ${NAME}, pid ${PID}), log ${LOG}, UI http://${LAN_IP}:${HUB_PORT}/${INSTANCE}/"
