#!/usr/bin/env bash
set -euo pipefail
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
scratch=${GUARD_CHECK_SCRATCH:-/config/workspace/tmp/litebox/guard-check}
wrapper="$repo/scripts/gate-window.sh"
label=litebox-guardtest
part=GUARDTEST
tree=$(git -C "$repo" rev-parse --short HEAD)
real_history=/config/workspace/tmp/gate/history.jsonl
real_gate=/config/workspace/hyperagent/scripts/gate-ticket.cjs
rm -rf "$scratch"
mkdir -p "$scratch/gateA" "$scratch/gateB" "$scratch/gateC" "$scratch/gateF" "$scratch/ticketsF"
window="$scratch/window.sh"
stub="$scratch/stub-gate.sh"
marker="$scratch/window.marker"
stub_log="$scratch/stub.log"

fail() { printf 'FAIL %s\n' "$1" >&2; exit 1; }
reset_side_effects() { : > "$stub_log"; rm -f "$marker"; }
seed() { printf '{"label":"%s","tree":"%s","code":%s}\n' "$label" "$tree" "$1" > "$2"; }
run_guard() {
  local history=$1 result=$2
  shift 2
  env GATE_SCRATCH="$(dirname "$history")" GATE_HISTORY="$history" \
    GATE_CMD="bash $stub" GATE_STUB_LOG="$stub_log" WINDOW_MARKER="$marker" "$@" \
    bash "$wrapper" "$label" "$window" "$result" 30
}

printf '#!/usr/bin/env bash\necho ran >> "$WINDOW_MARKER"\necho WINDOW_DONE 0\n' > "$window"
cat > "$stub" <<'SH'
#!/usr/bin/env bash
set -u
printf 'stub-invoked %s\n' "$*" >> "$GATE_STUB_LOG"
label=$2
shift 3
"$@"
rc=$?
python3 -I - "$GATE_HISTORY" "$label" "$GATE_TREE" "$rc" <<'PY'
import json
import sys

path, label, tree, code = sys.argv[1:]
with open(path, "a", encoding="utf8") as stream:
    stream.write(json.dumps({"label": label, "tree": tree, "code": int(code)}) + "\n")
PY
printf 'GRANTED %s\n' "$label"
exit "$rc"
SH
bash -n "$wrapper" || fail "wrapper syntax"
bash -n "$window" || fail "window syntax"
bash -n "$stub" || fail "stub syntax"

history_a="$scratch/gateA/history.jsonl"
seed 0 "$history_a"
reset_side_effects
out=$(run_guard "$history_a" "$scratch/a.out") || fail "cached pass row exited nonzero"
grep -q "^CACHED $label on $tree" <<< "$out" || fail "cached pass row did not print CACHED"
[ ! -s "$stub_log" ] || fail "cached pass row queued a gate ticket"
[ ! -e "$marker" ] || fail "cached pass row ran the window"
[ "$(wc -l < "$history_a")" -eq 1 ] || fail "cached pass row wrote history"
tail -n 1 "$scratch/a.out" | grep -q "^${part}_DONE 0$" || fail "cached pass row lacks the DONE marker"
printf 'PASS cached pass row prints CACHED and queues nothing\n'

history_b="$scratch/gateB/history.jsonl"
seed 12 "$history_b"
reset_side_effects
status=0
out=$(run_guard "$history_b" "$scratch/b.out") || status=$?
[ "$status" -eq 2 ] || fail "failed-only row exited $status, expected 2"
grep -q "^BLOCKED $label on $tree" <<< "$out" || fail "failed-only row did not print BLOCKED"
[ ! -s "$stub_log" ] || fail "failed-only row queued a gate ticket"
[ ! -e "$marker" ] || fail "failed-only row ran the window"
printf 'PASS failed-only row blocks an unexplained rerun\n'

history_c="$scratch/gateC/history.jsonl"
seed 12 "$history_c"
reset_side_effects
status=0
out=$(run_guard "$history_c" "$scratch/c.out" GATE_FORCE=1 GATE_RERUN_REASON='witness changed: static guest HTTP server') || status=$?
[ "$status" -eq 0 ] || fail "forced rerun exited $status"
grep -q '^FORCED .*reason=witness changed' "$scratch/c.out" || fail "forced rerun did not record its reason"
[ -s "$stub_log" ] || fail "forced rerun did not queue"
[ -e "$marker" ] || fail "forced rerun did not run the window"
[ "$(wc -l < "$history_c")" -eq 2 ] || fail "forced rerun did not append a history row"
tail -n 1 "$scratch/c.out" | grep -q "^${part}_DONE 0$" || fail "forced rerun lacks the DONE marker"
printf 'PASS forced rerun records its reason and runs once\n'

stub_calls_before=$(wc -l < "$stub_log")
window_calls_before=$(wc -l < "$marker")
out=$(run_guard "$history_c" "$scratch/d.out") || fail "post-rerun cached path exited nonzero"
grep -q "^CACHED $label on $tree" <<< "$out" || fail "post-rerun cached path did not print CACHED"
[ "$(wc -l < "$stub_log")" -eq "$stub_calls_before" ] || fail "post-rerun cached path queued again"
[ "$(wc -l < "$marker")" -eq "$window_calls_before" ] || fail "post-rerun cached path ran the window"
printf 'PASS same label/tree after a passing rerun prints CACHED (stub calls before=%s)\n' "$stub_calls_before"

reset_side_effects
out=$(env GATE_TREE=34001d81 GATE_HISTORY="$real_history" GATE_CMD="bash $stub" \
  GATE_STUB_LOG="$stub_log" WINDOW_MARKER="$marker" \
  bash "$wrapper" litebox-smoke "$window" "$scratch/e.out" 30) || fail "audited case exited nonzero"
grep -q '^CACHED litebox-smoke on 34001d81' <<< "$out" || fail "audited case did not print CACHED"
[ ! -s "$stub_log" ] || fail "audited litebox-smoke/34001d81 was queued again"
printf 'PASS audited litebox-smoke on 34001d81 prints CACHED\n'

mkdir -p "$scratch/gateF" "$scratch/ticketsF"
out=$(env GATE_TREE=34001d81 GATE_HISTORY="$real_history" GATE_SCRATCH="$scratch/gateF" \
  GATE_TICKET_DIR="$scratch/ticketsF" \
  node "$real_gate" run litebox-smoke -- bash -c 'exit 3' 2>&1) || fail "real gate CACHED check exited nonzero"
grep -q '^CACHED litebox-smoke on 34001d81' <<< "$out" || fail "real gate did not print CACHED"
[ -z "$(ls -A "$scratch/ticketsF")" ] || fail "real gate created a ticket for a CACHED label/tree"
printf 'PASS real gate also prints CACHED without taking a ticket\n'
printf 'PASS gate guard witness\n'
