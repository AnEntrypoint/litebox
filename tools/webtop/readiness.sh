#!/usr/bin/env bash
set -euo pipefail
repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
runner=${RUNNER:-$repo/target/release/litebox_runner_linux_userland}
image_ref=${WEBTOP_IMAGE_REF:-$(cat "$repo/dist_tools/webtop-image.ref")}
host_port=${BENCH_HOST_PORT:-3000}
guest_port=${BENCH_GUEST_PORT:-3000}
deadline_s=${BENCH_DEADLINE_S:-600}
strict=${BENCH_STRICT:-0}
scratch=${BENCH_SCRATCH:-/config/workspace/tmp/litebox/readiness}
mkdir -p "$scratch"
log="$scratch/runner.log"
result="$scratch/result.txt"
runner_pid=""
sampled_rss_kb=0
sampled_threads=0
sampled_procs=1
last_http_code=""

cleanup() {
  if [ -n "$runner_pid" ] && kill -0 "$runner_pid" 2>/dev/null; then
    for child in $(pgrep -P "$runner_pid" 2>/dev/null || true); do kill "$child" 2>/dev/null || true; done
    kill "$runner_pid" 2>/dev/null || true
    wait "$runner_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

now_s() { date +%s.%N; }
elapsed_s() { awk -v t0="$1" -v now="$(now_s)" 'BEGIN { printf "%.3f", now - t0 }'; }
greater() { awk -v a="$1" -v b="$2" 'BEGIN { print (a > b) ? 1 : 0 }'; }

sample_resources() {
  [ -r "/proc/$runner_pid/status" ] || return 0
  rss=$(awk '/^VmRSS:/ { print $2 }' "/proc/$runner_pid/status" 2>/dev/null || echo 0)
  threads=$(find "/proc/$runner_pid/task" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)
  procs=1
  for stat in /proc/[0-9]*/stat; do
    [ -r "$stat" ] || continue
    ppid=$(awk '{ print $4 }' "$stat" 2>/dev/null || echo 0)
    if [ "$ppid" = "$runner_pid" ]; then procs=$((procs + 1)); fi
  done
  if [ "${rss:-0}" -gt "$sampled_rss_kb" ]; then sampled_rss_kb="$rss"; fi
  if [ "${threads:-0}" -gt "$sampled_threads" ]; then sampled_threads="$threads"; fi
  if [ "$procs" -gt "$sampled_procs" ]; then sampled_procs="$procs"; fi
}

cached_layers_before=$(find "$repo/.litebox-cache" -maxdepth 1 -name 'raw_*.tar' -type f 2>/dev/null | wc -l)
rootfs_bytes_before=$(find "$repo/.litebox-cache" -maxdepth 1 -name 'raw_*.tar' -type f -printf '%s\n' 2>/dev/null | awk '{ s += $1 } END { print s + 0 }')

: > "$log"
t0=$(now_s)
LITEBOX_LOG=warn "$runner" --oci-image "$image_ref" --publish "$host_port:$guest_port" > "$log" 2>&1 &
runner_pid=$!
runner_rc=alive
echo "BENCH_START image_ref=$image_ref runner=$runner host_port=$host_port deadline_s=$deadline_s cached_layers_before=$cached_layers_before"

http_200_s=""
first_frame_s=""
frame_evidence=""
status=NOT_READY
while :; do
  sample_resources
  now=$(elapsed_s "$t0")
  if [ "$(greater "$now" "$deadline_s")" = 1 ]; then break; fi
  if [ -z "$http_200_s" ]; then
    last_http_code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 1 "http://127.0.0.1:$host_port/" || true)
    if [ "$last_http_code" = "200" ]; then
      http_200_s=$(elapsed_s "$t0")
      echo "MILESTONE http_200_s=$http_200_s"
    fi
  fi
  if [ -z "$first_frame_s" ]; then
    frame_evidence=$(grep -aE 'EncFPS: [1-9]|\[LITEBOX_DUMP_FRAMES\] frame' "$log" 2>/dev/null | head -n 1 || true)
    if [ -n "$frame_evidence" ]; then
      first_frame_s=$(elapsed_s "$t0")
      echo "MILESTONE first_frame_s=$first_frame_s evidence=$frame_evidence"
    fi
  fi
  if [ -n "$http_200_s" ] && [ -n "$first_frame_s" ]; then
    status=READY
    break
  fi
  if ! kill -0 "$runner_pid" 2>/dev/null; then
    wait "$runner_pid" 2>/dev/null || runner_rc=$?
    echo "EVIDENCE runner_exited_before_readiness elapsed_s=$(elapsed_s "$t0") rc=$runner_rc"
    break
  fi
  sleep 0.25
done

sample_resources
end_s=$(elapsed_s "$t0")
layer_count=$(find "$repo/.litebox-cache" -maxdepth 1 -name 'raw_*.tar' -type f 2>/dev/null | wc -l)
rootfs_bytes=$(find "$repo/.litebox-cache" -maxdepth 1 -name 'raw_*.tar' -type f -printf '%s\n' 2>/dev/null | awk '{ s += $1 } END { print s + 0 }')
peak_rss_mb=$(awk -v kb="$sampled_rss_kb" 'BEGIN { printf "%.1f", kb / 1024 }')
readiness_s=${first_frame_s:-NOT_REACHED}
gap_s=$(awk -v r="$readiness_s" 'BEGIN { if (r == "NOT_REACHED") print "NOT_REACHED"; else printf "%.3f", r - 5 }')

{
  echo "BENCH_RESULT image_ref=$image_ref status=$status target_s=5"
  echo "  cold_or_warm_cached_layers_before=$cached_layers_before rootfs_bytes_before=$rootfs_bytes_before"
  echo "  wall_s=$end_s http_200_s=${http_200_s:-NOT_REACHED} first_frame_s=${first_frame_s:-NOT_REACHED} readiness_s=$readiness_s gap_to_target_s=$gap_s"
  echo "  peak_rss_mb=$peak_rss_mb rootfs_bytes=$rootfs_bytes layer_count=$layer_count host_procs=$sampled_procs guest_threads=$sampled_threads"
  echo "  last_http_code=$last_http_code frame_evidence=${frame_evidence:-NONE} runner_rc=$runner_rc"
  echo "  target_gap=under-5-seconds-target ${gap_s}"
  echo "EVIDENCE log_tail:"
  tail -n 20 "$log" 2>/dev/null || true
} | tee "$result"

if [ "$status" != READY ] && [ "$strict" = 1 ]; then
  exit 1
fi
