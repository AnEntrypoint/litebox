#!/bin/sh
# Cheap discriminator for the fork_verify-scales-with-process-count hypothesis
# (Pass 347): no GUI, no weston/xwayland/dbus -- just N idle `sleep` background
# processes in the same guest, then a 200-exec busybox timing loop, using the
# same DIAG_TIMELINE execve-timestamp bracketing method as the combined
# XFCE+bench passes. If per-exec cost climbs with idle background process
# count alone, fork_verify's per-exec healing cost scales with process-tree
# size/shape independent of anything GUI-specific.
N_BG="$1"
echo "BG_COUNT=$N_BG"

i=0
while [ "$i" -lt "$N_BG" ]; do
  sleep 300 &
  i=$((i+1))
done
sleep 1
echo BG_SPAWNED

echo BENCH_N0_START
date +%s%3N
i=0
while [ "$i" -lt 1 ]; do i=$((i+1)); done
date +%s%3N
echo BENCH_N0_END

echo BENCH_N200_START
date +%s%3N
i=0
while [ "$i" -lt 200 ]; do
  busybox true
  i=$((i+1))
done
date +%s%3N
echo BENCH_N200_END

echo BENCH_DONE
