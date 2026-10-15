#!/usr/bin/env bash
# Run clippy the way CI does for each target, and print warning and error counts per target.
#
# usage: scripts/lint-targets.sh [host|windows|macos|all]   (default: all)
#
# CI treats every warning as an error, so any count above zero fails a job. Full output is
# kept under $OUT so a count can be traced to a file without another compile.
set -uo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

which_targets="${1:-all}"
OUT="${LINT_OUT:-${TMPDIR:-/tmp}/lint-targets}"
mkdir -p "$OUT"

run() {
  local name="$1"; shift
  local log="$OUT/$name.txt"
  local start end
  start=$(date +%s)
  cargo clippy --locked --all-targets --all-features "$@" 2>&1 | sed -E 's/(\x1b|\^\[)\[[0-9;]*m//g' > "$log"
  end=$(date +%s)
  local warnings errors
  warnings=$(grep -c '^warning' "$log" || true)
  errors=$(grep -c '^error' "$log" || true)
  printf '%-10s warnings=%-4s errors=%-3s %4ss  log=%s\n' "$name" "$warnings" "$errors" "$((end - start))" "$log"
}

case "$which_targets" in
  host|all)
    run host -p litebox -p litebox_shim_linux -p litebox_platform_linux_userland -p litebox_presenter_protocol ;;
esac
case "$which_targets" in
  windows|all)
    run windows --target x86_64-pc-windows-gnu \
      -p litebox_runner_linux_on_windows_userland -p litebox_platform_windows_userland -p litebox_presenter_protocol ;;
esac
case "$which_targets" in
  macos|all)
    run macos --target aarch64-apple-darwin -p litebox_shim_linux -p litebox ;;
esac
