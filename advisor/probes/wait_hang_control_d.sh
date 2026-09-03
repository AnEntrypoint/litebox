#!/bin/sh
# Phase 1 (fork+execve churn) THEN phase 3 (backgrounded subshells + wait).
# Phase 3 alone passes; if this hangs, the churn is what poisons it.
echo D_START
i=0
while [ "$i" -lt 60 ]; do
    /bin/true
    x=$(/bin/echo "iter $i")
    i=$((i + 1))
done
echo D_PHASE1_OK
k=0
while [ "$k" -lt 20 ]; do
    ( /bin/true; /bin/echo "nested $k" >/dev/null ) &
    k=$((k + 1))
done
wait
echo D_DONE
