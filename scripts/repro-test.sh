#!/usr/bin/env bash
# Run one runner integration test with the stuck-thread diagnostic, and say where it stopped.
#
# usage: scripts/repro-test.sh <test-name> [cap-seconds]   (cap defaults to 300)
#
# The diagnostic (LITEBOX_DIAG_STUCK_SECS) prints, every window, each guest thread that has been
# parked in one syscall: `diag-stuck ... syscall=futex(...)`. A test that hangs therefore ends
# with the exact syscall of every stuck thread, instead of an unexplained timeout. Output is kept
# in $OUT so the full log can be read after the run.
set -uo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

test_name="${1:?usage: repro-test.sh <test-name> [cap-seconds]}"
cap="${2:-300}"
OUT="${REPRO_OUT:-${TMPDIR:-/tmp}/repro-test}"
mkdir -p "$OUT"
log="$OUT/$test_name.log"

start=$(date +%s)
LITEBOX_LOG="${LITEBOX_LOG:-warn}" LITEBOX_DIAG_STUCK_SECS="${LITEBOX_DIAG_STUCK_SECS:-20}" \
  timeout "$cap" cargo test --locked -p litebox_runner_linux_userland --test run "$test_name" -- --nocapture \
  > "$log" 2>&1
rc=$?
end=$(date +%s)

sed -E -i 's/(\x1b|\^\[)\[[0-9;]*m//g' "$log"
result=$(grep -aE '^test result' "$log" | head -1)
echo "rc=$rc wall=$((end - start))s log=$log"
echo "${result:-no test result line: the run was cut off by the ${cap}s cap}"
if [ "$rc" -eq 124 ] || [ -z "$result" ]; then
  echo "--- last stuck report per thread:"
  grep -a 'diag-stuck\] stuck=stuck_ms' "$log" | sed -E 's/^ *[0-9.]+s +WARN +//' \
    | awk '{ key=""; for (i=1;i<=NF;i++) if ($i ~ /^tid=/) key=$i; last[key]=$0 } END { for (k in last) print last[k] }' \
    | cut -c1-240
fi
