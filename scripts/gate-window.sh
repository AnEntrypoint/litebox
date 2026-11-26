#!/usr/bin/env bash
set -euo pipefail
label=${1:?label required}
window_script=${2:?window script required}
result_file=${3:?result file required}
wait_seconds=${4:-570}
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
pid_file="$result_file.pid"
gate_scratch=${GATE_SCRATCH:-${HYPERAGENT_SCRATCH:-/config/workspace/tmp}/gate}
gate_history=${GATE_HISTORY:-$gate_scratch/history.jsonl}
gate_cmd=${GATE_CMD:-node ${HYPERAGENT_ROOT:-/config/workspace/hyperagent}/scripts/gate-ticket.cjs}
export GATE_SCRATCH GATE_HISTORY
export GATE_TREE=${GATE_TREE:-$(git -C "$repo" rev-parse --short HEAD)}
part=$(printf '%s' "${label#litebox-}" | tr '[:lower:]-' '[:upper:]_')

decision=$(python3 -I - "$gate_history" "$label" "$GATE_TREE" <<'PY'
import json
import sys

path, label, tree = sys.argv[1:]
rows = []
try:
    with open(path, encoding="utf8") as stream:
        for line in stream:
            if not line.strip():
                continue
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError:
                pass
except FileNotFoundError:
    pass
matched = [row for row in rows if row.get("label") == label and row.get("tree") == tree]
if any(row.get("code") == 0 for row in matched):
    print("PASSED")
elif matched:
    print("FAILED")
else:
    print("NEW")
PY
)

if [ -z "${GATE_FORCE:-}" ]; then
  if [ "$decision" = PASSED ]; then
    printf 'CACHED %s on %s already passed; GATE_FORCE=1 reruns it\n' "$label" "$GATE_TREE" > "$result_file"
    printf '%s_DONE 0\n' "$part" >> "$result_file"
    cat "$result_file"
    exit 0
  fi
  if [ "$decision" = FAILED ]; then
    printf 'BLOCKED %s on %s has no passing row; GATE_FORCE=1 and GATE_RERUN_REASON are required\n' "$label" "$GATE_TREE" > "$result_file"
    printf '%s_DONE 2\n' "$part" >> "$result_file"
    cat "$result_file"
    exit 2
  fi
  : > "$result_file"
else
  if [ -z "${GATE_RERUN_REASON:-}" ]; then
    printf 'BLOCKED forced rerun of %s on %s needs GATE_RERUN_REASON\n' "$label" "$GATE_TREE" > "$result_file"
    printf '%s_DONE 2\n' "$part" >> "$result_file"
    cat "$result_file"
    exit 2
  fi
  printf 'FORCED %s on %s decision=%s reason=%s\n' "$label" "$GATE_TREE" "$decision" "$GATE_RERUN_REASON" > "$result_file"
fi

read -r -a gate_argv <<< "$gate_cmd"
rm -f "$pid_file" "$pid_file.status"
setsid nohup bash -c 'pid_file=$1; echo $$ > "$pid_file"; shift; "$@"; rc=$?; printf "%s\n" "$rc" > "$pid_file.status"; exit "$rc"' _ "$pid_file" "${gate_argv[@]}" run "$label" -- bash "$window_script" >> "$result_file" 2>&1 < /dev/null &
launcher=$!
sleep 1
if [ ! -s "$pid_file" ]; then
  printf '%s_DONE 2\n' "$part" >> "$result_file"
  tail -n 20 "$result_file"
  exit 2
fi
gate_pid=$(cat "$pid_file")
timeout "$wait_seconds" tail --pid="$gate_pid" -f /dev/null || true
wait "$launcher" 2>/dev/null || true
status=$(cat "$pid_file.status" 2>/dev/null || printf '1')
printf '%s_DONE %s\n' "$part" "$status" >> "$result_file"
tail -n 30 "$result_file"
exit "$status"
