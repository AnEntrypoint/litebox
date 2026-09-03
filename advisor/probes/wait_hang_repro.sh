#!/bin/sh
# Phase 2 (long-lived background children) THEN phase 3.
# Discriminates "live sleeping siblings present" from "fork/execve churn".
echo E_START
j=0
while [ "$j" -lt 5 ]; do
    sleep 300 &
    j=$((j + 1))
done
echo E_PHASE2_OK
k=0
while [ "$k" -lt 20 ]; do
    ( /bin/true; /bin/echo "nested $k" >/dev/null ) &
    k=$((k + 1))
done
wait
echo E_DONE
