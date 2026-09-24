#!/usr/bin/env bash
# Keeps bot recordings from filling the disk (they grow ~2 GB per hour of
# Switch play; /tmp once filled the whole 581 GB disk).
#
# Usage: tools/disk-guard.sh            report only
#        tools/disk-guard.sh --prune    delete what is safe to delete
#        tools/disk-guard.sh --watch    --prune every WATCH_MINUTES (30),
#                                       forever (tools/live-run.sh starts it)
#
# A recording is a directory with metadata.json and frames/ (from --record).
# One that a running pokebot is writing (its --record argument, or written in
# the last 10 minutes) is "active" and is never deleted. --prune:
#   1. deletes inactive recordings older than KEEP_HOURS (default 12);
#   2. while free space is under MIN_FREE_GB (default 100), deletes the
#      oldest inactive recordings, whatever their age;
#   3. if still under MIN_FREE_GB, deletes frame PNGs older than TRIM_HOURS
#      (default 6) inside active recordings (frames.jsonl stays; those
#      frames just can't be replayed any more);
#   4. deletes debug bundles in captures/stuck older than BUNDLE_DAYS
#      (default 14).
# Copy anything worth keeping (fixtures: captures/fixtures/) before pruning.
#
# Recordings are looked for in RECORD_DIRS (default: /tmp).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KEEP_HOURS="${KEEP_HOURS:-12}"
MIN_FREE_GB="${MIN_FREE_GB:-100}"
TRIM_HOURS="${TRIM_HOURS:-6}"
BUNDLE_DAYS="${BUNDLE_DAYS:-14}"
RECORD_DIRS="${RECORD_DIRS:-/tmp}"
WATCH_MINUTES="${WATCH_MINUTES:-30}"
PRUNE=0
[[ "${1:-}" == "--prune" ]] && PRUNE=1
if [[ "${1:-}" == "--watch" ]]; then
  while true; do
    echo "== $(date -Is)"
    "$0" --prune || echo "disk-guard: prune failed ($?)"
    sleep "$(( WATCH_MINUTES * 60 ))"
  done
fi

free_gb() { df -BG --output=avail "$1" | tail -1 | tr -dc '0-9'; }

# --record directories of running bots.
active_dirs() {
  { pgrep -a -f 'pokebot' 2>/dev/null || true; } \
    | { grep -o -- '--record [^ ]*' || true; } | awk '{print $2}' \
    | while read -r d; do realpath -m "${d}"; done | sort -u
}

# Recordings, oldest first: "<mtime epoch> <dir>".
recordings() {
  for base in ${RECORD_DIRS}; do
    find "${base}" -mindepth 1 -maxdepth 2 -type f -name metadata.json 2>/dev/null \
      | while read -r meta; do
          d="$(dirname "${meta}")"
          [[ -d "${d}/frames" ]] && echo "$(stat -c %Y "${d}") $(realpath -m "${d}")"
        done
  done | sort -n
}

ACTIVE="$(active_dirs)"
# Also active: written in the last 10 minutes (a bot restarted by systemd
# records to <dir>.2, which isn't its --record argument).
is_active() {
  grep -qxF "$1" <<<"${ACTIVE}" && return 0
  [[ -n "$(find "$1/frames.jsonl" -mmin -10 2>/dev/null)" ]]
}

echo "free: $(free_gb /) GB on / (want >= ${MIN_FREE_GB} GB)"
now="$(date +%s)"
while read -r mtime dir; do
  [[ -z "${dir}" ]] && continue
  age_h=$(( (now - mtime) / 3600 ))
  size="$(du -sh "${dir}" 2>/dev/null | cut -f1)"
  state=inactive; is_active "${dir}" && state=ACTIVE
  echo "  ${size}  ${age_h} h  ${state}  ${dir}"
done < <(recordings)
bundles="${ROOT}/captures/stuck"
[[ -d "${bundles}" ]] && echo "debug bundles: $(ls "${bundles}" | wc -l) ($(du -sh "${bundles}" | cut -f1)) in ${bundles}"

(( PRUNE )) || exit 0

delete() { echo "deleting $2: $1"; rm -rf -- "$1"; }

# 1. Old inactive recordings.
while read -r mtime dir; do
  [[ -z "${dir}" ]] && continue
  if ! is_active "${dir}" && (( now - mtime > KEEP_HOURS * 3600 )); then
    delete "${dir}" "older than ${KEEP_HOURS} h"
  fi
done < <(recordings)

# 2. Low on space: oldest inactive recordings first.
while (( $(free_gb /) < MIN_FREE_GB )); do
  victim=""
  while read -r _ dir; do
    [[ -n "${dir}" ]] && ! is_active "${dir}" && { victim="${dir}"; break; }
  done < <(recordings)
  [[ -z "${victim}" ]] && break
  delete "${victim}" "free space under ${MIN_FREE_GB} GB"
done

# 3. Still low: old frames inside active recordings.
if (( $(free_gb /) < MIN_FREE_GB )); then
  while read -r dir; do
    [[ -d "${dir}/frames" ]] || continue
    n="$(find "${dir}/frames" -name '*.png' -mmin "+$(( TRIM_HOURS * 60 ))" | wc -l)"
    if (( n > 0 )); then
      echo "trimming ${n} frames older than ${TRIM_HOURS} h from active ${dir}"
      find "${dir}/frames" -name '*.png' -mmin "+$(( TRIM_HOURS * 60 ))" -delete
    fi
  done <<<"${ACTIVE}"
fi

# 4. Old debug bundles.
if [[ -d "${bundles}" ]]; then
  find "${bundles}" -mindepth 1 -maxdepth 1 -type d -mtime "+${BUNDLE_DAYS}" \
    -exec echo "deleting bundle older than ${BUNDLE_DAYS} days:" {} \; -exec rm -rf {} +
fi
echo "free after pruning: $(free_gb /) GB"
